use super::batch_cancel::{decode_statuses, expect};
use super::*;
use crate::{
    trading::algo::{MAX_ALGO_ORDERS_PER_MARKET, MAX_OPEN_ORDERS_PER_MARKET},
    batch::{PerpBatchReason, PerpBatchTag},
    interface::IPerpDex::{batchPlaceOrdersCall, placeAlgoOrderCall, PlaceItem},
    types::AlgoOrderType,
    PerpOutput,
};

/// Deep pockets: these tests are about the COUNT, never a margin reject.
const RICH: u64 = WALLET * 10_000;
/// ALICE's own resting prices — far enough apart that nothing she rests ever crosses.
const BID: u64 = PRICE - TICK;
const ASK: u64 = PRICE + 10 * TICK;
const CAP_MSG: &str = "max open orders reached";

fn cap_ctx() -> TestCtx {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, ALICE, RICH);
    fund(&mut ctx, BOB, RICH);
    ctx
}

fn order_input(side: u8, price: u64, order_type: u8, tif: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: QTY,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

fn algo_input(trigger: u64) -> Vec<u8> {
    placeAlgoOrderCall {
        algoType: 0,
        marketId: MARKET_ID,
        side: 1,
        orderType: AlgoOrderType::StopMarket as u8,
        triggerPrice: trigger,
        price: 0,
        quantity: QTY,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        // reduce-only: the only conditional this build accepts. It needs no position to PLACE.
        flags: 1,
    }
    .abi_encode()
}

fn shell(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, 30_000_000, caller, U256::ZERO, false, ctx)
        .expect("a placement never hard-fails")
}

fn accepted(ctx: &mut TestCtx, caller: Address, input: &[u8]) {
    let out = shell(ctx, caller, input);
    assert!(!out.reverted, "expected acceptance, got {:?}", String::from_utf8_lossy(&out.bytes));
}

/// Refused with `sel: max open orders reached`, and nothing at all was written.
fn refused_at_cap(ctx: &mut TestCtx, caller: Address, input: &[u8], sel: &str) {
    let writes = JournalTr::perp_write_count(ctx.journal_mut());
    let nonce = storage::load_user_nonce(ctx, caller).unwrap();
    let book = storage::book_order_count(ctx, caller, MARKET_ID).unwrap();
    let out = shell(ctx, caller, input);
    assert!(out.reverted, "expected the cap to refuse it");
    let msg = String::from_utf8_lossy(&out.bytes).to_string();
    assert!(msg.contains(&format!("{sel}: {CAP_MSG}")), "wrong reject: {msg}");
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes,
        "a cap reject must be write-clean (commit-only #23)"
    );
    assert_eq!(storage::load_user_nonce(ctx, caller).unwrap(), nonce, "no id consumed");
    assert_eq!(storage::book_order_count(ctx, caller, MARKET_ID).unwrap(), book);
}

/// Rest `bids` GTC bids and `asks` GTC asks for ALICE.
fn rest(ctx: &mut TestCtx, bids: usize, asks: usize) {
    for _ in 0..bids {
        place(ctx, ALICE, 0, BID, QTY, 0, 0);
    }
    for _ in 0..asks {
        place(ctx, ALICE, 1, ASK, QTY, 0, 0);
    }
}

fn open_orders(ctx: &mut TestCtx, user: Address) -> usize {
    storage::book_order_count(ctx, user, MARKET_ID).unwrap()
        + storage::load_user_algo_orders(ctx, user, MARKET_ID).unwrap().len()
}

/// The boundary, on both sides of the book at once: 199 open → the 200th rests; at 200 a GTC
/// and a PostOnly are refused pre-write. A count that skipped either side would admit the
/// 201st here (100 bids alone, or 100 asks alone, are far below the cap).
#[test]
fn the_200th_order_rests_and_the_201st_is_refused_counting_both_sides() {
    let mut ctx = cap_ctx();
    rest(&mut ctx, 100, MAX_OPEN_ORDERS_PER_MARKET - 101);
    assert_eq!(open_orders(&mut ctx, ALICE), MAX_OPEN_ORDERS_PER_MARKET - 1);

    // 199 → 200: still admitted (an off-by-one `>` vs `>=` would refuse this one).
    accepted(&mut ctx, ALICE, &order_input(0, BID, 0, 0, 0));
    assert_eq!(open_orders(&mut ctx, ALICE), MAX_OPEN_ORDERS_PER_MARKET);

    // At 200: both kinds that can rest are refused, on either side.
    refused_at_cap(&mut ctx, ALICE, &order_input(0, BID, 0, 0, 0), "placeOrder");
    refused_at_cap(&mut ctx, ALICE, &order_input(1, ASK, 0, 0, 0), "placeOrder");
    refused_at_cap(&mut ctx, ALICE, &order_input(0, BID, 0, 3, 0), "placeOrder");
    refused_at_cap(&mut ctx, ALICE, &order_input(1, ASK, 0, 3, 0), "placeOrder");

    // The cap is per USER: BOB is unaffected by ALICE's 200.
    accepted(&mut ctx, BOB, &order_input(0, BID, 0, 0, 0));
    // ...and freeing one slot re-admits her: it is a count, not a latch.
    let id = place_id_of_first_bid(&mut ctx);
    let cancel = cancelOrderCall { orderId: id.into(), marketId: MARKET_ID }.abi_encode();
    accepted(&mut ctx, ALICE, &cancel);
    accepted(&mut ctx, ALICE, &order_input(0, BID, 0, 0, 0));
}

fn place_id_of_first_bid(ctx: &mut TestCtx) -> [u8; 32] {
    storage::load_buy_orders(ctx, ALICE, MARKET_ID).unwrap()[0].order_id
}

/// The count is PRE-match: at the cap even a GTC that would have filled completely (and so
/// never rested) is refused — a submission filter, like Binance's. IOC, FOK and market orders
/// never add a book entry and are never refused by it, so a user at the cap can still trade
/// and close.
#[test]
fn at_the_cap_only_orders_that_can_rest_are_refused() {
    let mut ctx = cap_ctx();
    // BOB's liquidity sits BELOW ALICE's own asks, so her takers fill against him, not herself.
    for _ in 0..4 {
        place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);
    }
    rest(&mut ctx, 100, MAX_OPEN_ORDERS_PER_MARKET - 100);
    assert_eq!(open_orders(&mut ctx, ALICE), MAX_OPEN_ORDERS_PER_MARKET);

    // A crossing GTC that WOULD fully fill against BOB: refused anyway, BOB's ask untouched.
    refused_at_cap(&mut ctx, ALICE, &order_input(0, PRICE + TICK, 0, 0, 0), "placeOrder");
    assert_eq!(storage::book_order_count(&mut ctx, BOB, MARKET_ID).unwrap(), 4);

    accepted(&mut ctx, ALICE, &order_input(0, PRICE + TICK, 0, 1, 0)); // IOC
    accepted(&mut ctx, ALICE, &order_input(0, PRICE + TICK, 0, 2, 0)); // FOK
    accepted(&mut ctx, ALICE, &order_input(0, 0, 1, 0, 0)); // market
    assert_eq!(pos(&mut ctx, ALICE).amount, 3 * QTY as i64, "all three filled");
    assert_eq!(
        open_orders(&mut ctx, ALICE),
        MAX_OPEN_ORDERS_PER_MARKET,
        "none of them rested"
    );
    // An IOC with NOTHING to fill expires — still admitted.
    accepted(&mut ctx, ALICE, &order_input(0, BID, 0, 1, 0));
}

/// Reduce-only resting orders count like any other, and a reduce-only order is refused at the
/// cap only when it could rest: a reduce-only IOC still closes.
#[test]
fn reduce_only_orders_count_and_a_reduce_only_ioc_still_closes_at_the_cap() {
    let mut ctx = cap_ctx();
    // ALICE goes long 4·QTY against BOB.
    for _ in 0..4 {
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    }
    place(&mut ctx, ALICE, 0, PRICE, 4 * QTY, 0, 0);
    assert_eq!(pos(&mut ctx, ALICE).amount, 4 * QTY as i64);

    // Three reduce-only asks rest (far above the touch).
    for _ in 0..3 {
        accepted(&mut ctx, ALICE, &order_input(1, ASK, 0, 0, 1));
    }
    rest(&mut ctx, MAX_OPEN_ORDERS_PER_MARKET - 3, 0);
    assert_eq!(
        open_orders(&mut ctx, ALICE),
        MAX_OPEN_ORDERS_PER_MARKET,
        "three reduce-only + 197 normal = the cap; a count that skipped reduce-only would be at 197"
    );
    refused_at_cap(&mut ctx, ALICE, &order_input(0, BID, 0, 0, 0), "placeOrder");
    refused_at_cap(&mut ctx, ALICE, &order_input(1, ASK, 0, 0, 1), "placeOrder");

    // A reduce-only IOC into BOB's bid closes one lot at the cap. (BOB bids ABOVE her own
    // resting bids, so the sell meets him first rather than self-matching.)
    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0);
    accepted(&mut ctx, ALICE, &order_input(1, PRICE, 0, 1, 1));
    assert_eq!(pos(&mut ctx, ALICE).amount, 3 * QTY as i64);
}

/// Untriggered conditionals count toward the same cap, both ways: they block a resting order,
/// and resting orders block a conditional — while the conditional sub-cap of 32 still applies
/// on its own, far below 200.
#[test]
fn conditional_orders_share_the_cap_with_the_book() {
    let mut ctx = cap_ctx();
    for i in 0..5u64 {
        accepted(&mut ctx, ALICE, &algo_input(PRICE / 2 + i * TICK));
    }
    rest(&mut ctx, 100, MAX_OPEN_ORDERS_PER_MARKET - 106);
    assert_eq!(open_orders(&mut ctx, ALICE), MAX_OPEN_ORDERS_PER_MARKET - 1);

    // 199 → 200 through a CONDITIONAL.
    accepted(&mut ctx, ALICE, &algo_input(PRICE / 2 + 10 * TICK));
    assert_eq!(open_orders(&mut ctx, ALICE), MAX_OPEN_ORDERS_PER_MARKET);
    assert_eq!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(), 6);

    // At 200 with only 6 conditionals: the BOOK order is refused (the algo list is counted), and
    // so is another conditional (the book is counted), though its own sub-cap is nowhere near.
    refused_at_cap(&mut ctx, ALICE, &order_input(0, BID, 0, 0, 0), "placeOrder");
    refused_at_cap(&mut ctx, ALICE, &algo_input(PRICE / 2 + 11 * TICK), "placeAlgoOrder");
}

/// The 32 sub-cap is still its own limit, with its own message, below the shared cap.
#[test]
fn the_conditional_sub_cap_still_binds_first() {
    let mut ctx = cap_ctx();
    for i in 0..MAX_ALGO_ORDERS_PER_MARKET as u64 {
        accepted(&mut ctx, ALICE, &algo_input(PRICE / 2 + i * TICK));
    }
    let out = shell(&mut ctx, ALICE, &algo_input(PRICE / 4));
    assert!(out.reverted);
    assert!(String::from_utf8_lossy(&out.bytes).contains("too many open conditional orders"));
}

/// Each batch item is checked against the count as left by the items before it; the item that
/// would be the 201st is a write-clean `Rejected` (reason 33) and the batch carries on — an IOC
/// after it is still accepted.
#[test]
fn a_batch_checks_each_item_against_the_items_before_it() {
    let mut ctx = cap_ctx();
    place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);
    rest(&mut ctx, 100, MAX_OPEN_ORDERS_PER_MARKET - 102);
    assert_eq!(open_orders(&mut ctx, ALICE), MAX_OPEN_ORDERS_PER_MARKET - 2);
    let base = storage::load_user_nonce(&mut ctx, ALICE).unwrap();

    let gtc = |side: u8, price: u64, tif: u8| PlaceItem {
        marketId: MARKET_ID,
        side,
        price,
        quantity: QTY,
        orderType: 0,
        tif,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    };
    let items = vec![
        gtc(0, BID, 0),            // 198 → 199
        gtc(1, ASK, 3),            // PostOnly, 199 → 200
        gtc(0, BID, 0),            // the 201st: refused
        gtc(0, PRICE + TICK, 1),   // IOC: never refused, fills BOB
    ];
    let out = shell(&mut ctx, ALICE, &batchPlaceOrdersCall { orders: items }.abi_encode());
    assert!(!out.reverted, "{:?}", String::from_utf8_lossy(&out.bytes));
    let blob = batchPlaceOrdersCall::abi_decode_returns(&out.bytes).unwrap().to_vec();
    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(PerpBatchTag::Accepted, derive_order_id(ALICE, base), PerpBatchReason::None),
            expect(PerpBatchTag::Accepted, derive_order_id(ALICE, base + 1), PerpBatchReason::None),
            expect(PerpBatchTag::Rejected, [0u8; 32], PerpBatchReason::MaxOpenOrders),
            // The rejected item consumed no id, so the IOC takes the next one.
            expect(PerpBatchTag::Filled, derive_order_id(ALICE, base + 2), PerpBatchReason::None),
        ]
    );
    assert_eq!(open_orders(&mut ctx, ALICE), MAX_OPEN_ORDERS_PER_MARKET);
    assert_eq!(storage::load_user_nonce(&mut ctx, ALICE).unwrap(), base + 3);
}

/// The direct `placeAlgoOrder` used to persist its nonce bump BEFORE the capacity checks, so a
/// refused conditional advanced the nonce under a reverted call (a commit-only #23 leak the
/// call-shell tripwire catches). The sub-cap reject must be write-clean through the shell too.
#[test]
fn a_refused_conditional_consumes_no_nonce() {
    let mut ctx = cap_ctx();
    for i in 0..MAX_ALGO_ORDERS_PER_MARKET as u64 {
        accepted(&mut ctx, ALICE, &algo_input(PRICE / 2 + i * TICK));
    }
    let writes = JournalTr::perp_write_count(ctx.journal_mut());
    let nonce = storage::load_user_nonce(&mut ctx, ALICE).unwrap();
    let out = shell(&mut ctx, ALICE, &algo_input(PRICE / 4));
    assert!(out.reverted);
    assert_eq!(JournalTr::perp_write_count(ctx.journal_mut()), writes);
    assert_eq!(storage::load_user_nonce(&mut ctx, ALICE).unwrap(), nonce);
}
