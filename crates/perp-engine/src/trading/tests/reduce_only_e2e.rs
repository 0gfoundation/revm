use super::*;
use crate::interface::IPerpDex::OrderCancelled;
use crate::types::{CancelReason, PerpPosition};
use alloy_sol_types::SolEvent;

const RO: u8 = 1; // flags bit 0

/// Gives `who` a long of `lots` (each `QTY`), funded so the margin maths is never the reason a
/// placement is refused.
fn seed_long(ctx: &mut TestCtx, who: Address, lots: i64) {
    let notional = lots * FILL_VALUE as i64;
    storage::save_position(
        ctx,
        who,
        MARKET_ID,
        &PerpPosition {
            amount: lots * QTY as i64,
            v_quote_balance: -notional,
            margin: notional.abs(),
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

fn place_flagged(
    ctx: &mut TestCtx,
    who: Address,
    side: u8,
    price: u64,
    qty: u64,
    flags: u8,
) -> Result<[u8; 32], String> {
    place_typed(ctx, who, side, price, qty, 0, 0, flags)
}

/// A MARKET order: `orderType = 1`, `tif = IOC`, and `price` is IGNORED — callers send 0, which
/// is exactly what the priority bug fed into the admission predicate.
fn place_market(
    ctx: &mut TestCtx,
    who: Address,
    side: u8,
    qty: u64,
    flags: u8,
) -> Result<[u8; 32], String> {
    place_typed(ctx, who, side, 0, qty, 1, 1, flags)
}

#[allow(clippy::too_many_arguments)]
fn place_typed(
    ctx: &mut TestCtx,
    who: Address,
    side: u8,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
    flags: u8,
) -> Result<[u8; 32], String> {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
    .abi_encode();
    let (id, _) = crate::trading::peek_order_id(ctx, who).unwrap();
    let out = run_perp_dex_call(&input, 30_000_000, who, U256::ZERO, false, ctx)
        .expect("must not hard-fail");
    if out.reverted {
        Err(String::from_utf8_lossy(&out.bytes).to_string())
    } else {
        Ok(id)
    }
}

fn cancel_reasons(ctx: &mut TestCtx) -> Vec<u8> {
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
        .map(|l| {
            OrderCancelled::decode_raw_log(l.data.topics(), &l.data.data)
                .unwrap()
                .reason
        })
        .collect()
}

#[test]
fn a_flat_position_has_nothing_to_reduce() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let err = place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY, RO).unwrap_err();
    assert!(err.contains("reduce-only requires an open position"), "got {err}");
}

/// A reduce-only BUY against a LONG would increase it. The side is not a preference — it is
/// determined by the position's sign.
#[test]
fn the_opening_side_is_refused() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);
    let err = place_flagged(&mut ctx, ALICE, 0, PRICE - TICK, QTY, RO).unwrap_err();
    assert!(
        err.contains("reduce-only order is on the position's opening side"),
        "got {err}"
    );
}

/// SILENT truncation: over-sized is accepted, not rejected, and the stored order carries the
/// SHRUNK quantity. `quantity` is a request, not a promise.
#[test]
fn an_oversized_request_is_silently_truncated() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);
    let id = place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY * 10, RO)
        .expect("must be ACCEPTED, not rejected");
    let order = storage::load_order(&mut ctx, &id).unwrap().unwrap();
    assert_eq!(order.quantity, QTY * 3, "truncated to the position");
    assert!(order.reduce_only);
}

/// The first reduce-only order commits the position; a second gets only the remainder. This is
/// the case a predicate that only counted NORMAL orders ahead would miss entirely.
#[test]
fn a_second_reduce_only_order_gets_only_the_remainder() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);
    place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY * 2, RO).unwrap();
    let id = place_flagged(&mut ctx, ALICE, 1, PRICE + 2 * TICK, QTY * 5, RO).unwrap();
    let order = storage::load_order(&mut ctx, &id).unwrap().unwrap();
    assert_eq!(order.quantity, QTY, "2 of 3 lots already committed");

    // And a third has nothing left.
    let err = place_flagged(&mut ctx, ALICE, 1, PRICE + 3 * TICK, QTY, RO).unwrap_err();
    assert!(err.contains("reduce-only capacity exhausted"), "got {err}");
}

/// The eviction trigger that is not about the position at all. A NORMAL sell nearer the touch
/// fills FIRST, so it would consume the long before the reduce-only order could reduce it.
/// Binance accepts the normal order and kills the reduce-only one — normal orders are
/// first-class.
#[test]
fn a_normal_order_resting_ahead_evicts_the_reduce_only_order() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);
    let ro = place_flagged(&mut ctx, ALICE, 1, PRICE + 5 * TICK, QTY * 3, RO).unwrap();
    assert!(storage::load_order(&mut ctx, &ro).unwrap().is_some());
    let _ = cancel_reasons(&mut ctx); // drain

    // A sell is NEARER the touch the lower its price, so this one fills first.
    place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY * 3, 0).unwrap();

    assert!(
        storage::load_order(&mut ctx, &ro).unwrap().is_none(),
        "delete-on-terminal: the evicted order is gone"
    );
    assert!(
        cancel_reasons(&mut ctx).contains(&(CancelReason::ReduceOnlyOvertaken as u8)),
        "must report WHY it died"
    );
}

/// The negative control for the test above: a normal order resting BEHIND fills after, so it
/// cannot consume the position first and must leave the reduce-only order alone. Without this,
/// "evict whenever any normal order rests" would pass.
#[test]
fn a_normal_order_resting_behind_leaves_it_alone() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);
    let ro = place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY * 3, RO).unwrap();
    place_flagged(&mut ctx, ALICE, 1, PRICE + 5 * TICK, QTY * 3, 0).unwrap();
    assert!(
        storage::load_order(&mut ctx, &ro).unwrap().is_some(),
        "an order that fills LATER cannot starve this one"
    );
}

fn placed_quantities(ctx: &mut TestCtx) -> Vec<u64> {
    use crate::interface::IPerpDex::OrderPlaced;
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|l| l.data.topics().first() == Some(&OrderPlaced::SIGNATURE_HASH))
        .map(|l| {
            OrderPlaced::decode_raw_log(l.data.topics(), &l.data.data)
                .unwrap()
                .quantity
        })
        .collect()
}

fn amount_of(ctx: &mut TestCtx, who: Address) -> i64 {
    storage::load_position(ctx, who, MARKET_ID).unwrap().amount
}

/// The truncated size has to be readable from the EVENT, not just from storage — that is the
/// on-chain analogue of Binance's rewritten `origQty`, and `OrderPlaced` is the only event that
/// fires on EVERY accepted order (`OrderRested` is absent for an IOC or a full fill).
#[test]
fn order_placed_carries_the_truncated_quantity() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);
    let _ = placed_quantities(&mut ctx); // drain

    place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY * 10, RO).unwrap();
    assert_eq!(
        placed_quantities(&mut ctx),
        vec![QTY * 3],
        "the event must publish the ACCEPTED size, not the request"
    );
}

/// A reduce-only order actually being MATCHED, which every other test here stops short of.
/// Closes the position exactly and leaves nothing behind.
#[test]
fn a_reduce_only_order_closes_the_position_when_it_fills() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);
    let ro = place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY * 3, RO).unwrap();

    // BOB lifts it.
    place_flagged(&mut ctx, BOB, 0, PRICE + TICK, QTY * 3, 0).unwrap();

    assert_eq!(amount_of(&mut ctx, ALICE), 0, "closed, not flipped");
    assert!(storage::load_order(&mut ctx, &ro).unwrap().is_none());
}

/// ⚠️ THE test for the claim that mid-walk eviction is unnecessary.
///
/// One sweep crosses a NORMAL sell and then a reduce-only sell behind it. Admission priced the
/// reduce-only order against the normal one already being ahead (capacity 3 − 1 = 2), so when
/// the walk reaches it the position still covers it in full and no clamp is needed. Without
/// the "count normal orders ahead" half of the prefix condition, the reduce-only order would
/// have been admitted at 3 and this sweep would leave ALICE SHORT.
#[test]
fn a_sweep_over_a_normal_order_and_then_a_reduce_only_one_never_opens() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);

    // Nearer the touch (lower price) fills first.
    place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY, 0).unwrap();
    let ro = place_flagged(&mut ctx, ALICE, 1, PRICE + 2 * TICK, QTY * 3, RO).unwrap();
    assert_eq!(
        storage::load_order(&mut ctx, &ro).unwrap().unwrap().quantity,
        QTY * 2,
        "truncated to 3 − 1 ahead"
    );

    // One sweep takes both levels.
    place_flagged(&mut ctx, BOB, 0, PRICE + 2 * TICK, QTY * 3, 0).unwrap();

    assert_eq!(
        amount_of(&mut ctx, ALICE),
        0,
        "1 + 2 closes exactly 3 — never short"
    );
}

/// A partial fill leaves the remainder resting, and the prefix condition still holds against
/// the smaller position (1 of 3 left, 1 lot still committed). ⚠️ Whether Binance keeps the
/// remainder alive is UNMEASURED — five rounds never partially filled a reduce-only order — so
/// this pins OUR reading (it survives), not an observed behaviour.
#[test]
fn a_partially_filled_reduce_only_order_keeps_its_remainder() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);
    let ro = place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY * 3, RO).unwrap();

    place_flagged(&mut ctx, BOB, 0, PRICE + TICK, QTY * 2, 0).unwrap();

    assert_eq!(amount_of(&mut ctx, ALICE), QTY as i64, "1 lot left");
    let order = storage::load_order(&mut ctx, &ro).unwrap().expect("remainder survives");
    assert_eq!(order.quantity - order.filled, QTY, "1 lot still committed");
}

/// The flag has to be recoverable OFF-CHAIN, from all four surfaces an indexer or a REST
/// gateway actually reads. Without this the only route is decoding `placeOrder` calldata and
/// re-deriving each batch item's order id to attribute it — not something an indexer can do —
/// and three already-published docs promise the field (`@order`'s `R`, the indexer's order
/// projection, and Binance parity for `/fapi/v1/order` + `/fapi/v1/openOrders`).
#[test]
fn the_reduce_only_flag_is_published_on_every_surface() {
    use crate::interface::IPerpDex::{
        getOpenOrdersCall, getOrderCall, OrderPlaced, OrderRested,
    };
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let ro = place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY, RO).unwrap();
    let plain = place_flagged(&mut ctx, ALICE, 1, PRICE + 5 * TICK, QTY, 0).unwrap();

    // ── the event stream ────────────────────────────────────────────────
    let mut placed = Vec::new();
    let mut rested = Vec::new();
    for l in JournalTr::take_logs(ctx.journal_mut()) {
        let t = l.data.topics().first().copied();
        if t == Some(OrderPlaced::SIGNATURE_HASH) {
            let e = OrderPlaced::decode_raw_log(l.data.topics(), &l.data.data).unwrap();
            placed.push((e.orderId.0, e.flags));
        } else if t == Some(OrderRested::SIGNATURE_HASH) {
            let e = OrderRested::decode_raw_log(l.data.topics(), &l.data.data).unwrap();
            rested.push((e.orderId.0, e.flags));
        }
    }
    assert_eq!(placed.iter().find(|(id, _)| *id == ro).unwrap().1, RO);
    assert_eq!(placed.iter().find(|(id, _)| *id == plain).unwrap().1, 0);
    assert_eq!(rested.iter().find(|(id, _)| *id == ro).unwrap().1, RO);
    assert_eq!(rested.iter().find(|(id, _)| *id == plain).unwrap().1, 0);

    // ── getOrder ────────────────────────────────────────────────────────
    let read = |ctx: &mut TestCtx, id: [u8; 32]| {
        let out = run_perp_dex_call(
            &getOrderCall { orderId: id.into(), marketId: MARKET_ID }.abi_encode(),
            10_000_000,
            ALICE,
            U256::ZERO,
            true,
            ctx,
        )
        .unwrap();
        getOrderCall::abi_decode_returns(&out.bytes).unwrap().flags
    };
    assert_eq!(read(&mut ctx, ro), RO);
    assert_eq!(read(&mut ctx, plain), 0);

    // ── getOpenOrders: index-aligned with orderIds ──────────────────────
    let out = run_perp_dex_call(
        &getOpenOrdersCall { user: ALICE, marketId: MARKET_ID }.abi_encode(),
        10_000_000,
        ALICE,
        U256::ZERO,
        true,
        &mut ctx,
    )
    .unwrap();
    let open = getOpenOrdersCall::abi_decode_returns(&out.bytes).unwrap();
    assert_eq!(open.flags.len(), open.orderIds.len(), "index-aligned");
    let flag_of = |id: [u8; 32]| {
        let i = open.orderIds.iter().position(|o| o.0 == id).unwrap();
        open.flags[i]
    };
    assert_eq!(flag_of(ro), RO);
    assert_eq!(flag_of(plain), 0);
}

// ── MARKET reduce-only: a market order has NO book position ──────────────

/// ⚠️ REGRESSION. A market order's `price` is IGNORED (callers send 0), but admission fed it
/// into `distance_from_touch` as if it were a book position. On the BUY side
/// `distance_from_touch(Buy, 0) = u64::MAX` — the FARTHEST possible — so every one of the
/// owner's resting normal buys counted as being "ahead" of an order that in fact fills
/// IMMEDIATELY, before all of them.
///
/// Consequence: a market reduce-only BUY was refused, or silently truncated, because of resting
/// orders that could not possibly fill first. That is the worst failure this feature can have —
/// the whole point of reduce-only is closing, and this BLOCKED closing.
///
/// Reported from the API repo against the pinned engine; reproduced here.
#[test]
fn a_market_reduce_only_buy_is_not_ranked_behind_the_owners_resting_buys() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, -3); // short 3

    // A resting normal BUY, far from the touch. It cannot fill before a market order.
    place_flagged(&mut ctx, ALICE, 0, PRICE - TICK, QTY * 3, 0).unwrap();
    // Liquidity for the market order to take.
    place_flagged(&mut ctx, BOB, 1, PRICE, QTY * 3, 0).unwrap();

    let id = place_market(&mut ctx, ALICE, 0, QTY * 3, RO)
        .expect("a market reduce-only BUY must not be refused by RESTING orders");
    let _ = id;
    assert_eq!(
        amount_of(&mut ctx, ALICE),
        0,
        "the short must be closed in full"
    );
}

/// The truncation half of the same defect: with a smaller resting buy the order was accepted but
/// SHRUNK, leaving exposure open — a quieter failure than the outright reject and a worse one,
/// because the caller believes the position is closed.
#[test]
fn a_market_reduce_only_buy_is_not_truncated_by_the_owners_resting_buys() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, -3);

    place_flagged(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0).unwrap();
    place_flagged(&mut ctx, BOB, 1, PRICE, QTY * 3, 0).unwrap();

    place_market(&mut ctx, ALICE, 0, QTY * 3, RO).unwrap();
    assert_eq!(
        amount_of(&mut ctx, ALICE),
        0,
        "truncated to 3 - 1 = 2 would leave 1 lot of exposure open"
    );
}

/// The SELL side happened to escape: `distance_from_touch(Sell, 0) = 0`, the NEAREST value, so a
/// market sell was ranked in front rather than behind. Pinned so the fix does not "correct" the
/// side that was already right, and so the asymmetry cannot come back.
#[test]
fn a_market_reduce_only_sell_closes_a_long_past_resting_sells() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, 3);

    place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY * 3, 0).unwrap();
    place_flagged(&mut ctx, BOB, 0, PRICE, QTY * 3, 0).unwrap();

    place_market(&mut ctx, ALICE, 1, QTY * 3, RO).unwrap();
    assert_eq!(amount_of(&mut ctx, ALICE), 0);
}

/// A market reduce-only order is still bounded by the POSITION — that is the invariant, and the
/// fix must not trade the priority bug for an unbounded one.
#[test]
fn a_market_reduce_only_order_is_still_truncated_to_the_position() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, -2);
    place_flagged(&mut ctx, BOB, 1, PRICE, QTY * 10, 0).unwrap();

    place_market(&mut ctx, ALICE, 0, QTY * 10, RO).unwrap();
    assert_eq!(
        amount_of(&mut ctx, ALICE),
        0,
        "closed exactly, never flipped long"
    );
}

/// The same defect reached IOC and FOK, not just MARKET — those also take liquidity
/// immediately, so the owner's resting orders cannot precede them either. It needed a specific
/// price ordering to trigger (an IOC priced FARTHER from the touch than a resting order), which
/// is why the market case surfaced first: a market order's price of 0 is the farthest possible
/// on the buy side, so it triggered unconditionally.
#[test]
fn an_ioc_reduce_only_buy_priced_behind_a_resting_buy_is_still_admitted() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    seed_long(&mut ctx, ALICE, -3);
    // NEARER the touch for a buy (higher price) than the IOC below.
    place_flagged(&mut ctx, ALICE, 0, PRICE + TICK, QTY * 3, 0).unwrap();

    // No asks, so this fills nothing and expires — the assertion is about ADMISSION.
    place_typed(&mut ctx, ALICE, 0, PRICE, QTY * 3, 0, 1, RO)
        .expect("an IOC reduce-only order has no queue position either");
}

/// MEASURED exemption (§1.6): a one-step reduce-only order below the market minimum is
/// accepted. It has to be — truncation routinely lands below any sane floor, and closing the
/// last sliver of a position is the order a trader most needs.
#[test]
fn the_minimum_size_does_not_apply_to_reduce_only() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let mut market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    market.min_quantity = QTY * 2;
    storage::save_market(&mut ctx, &market).unwrap();
    seed_long(&mut ctx, ALICE, 3);

    let err = place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY, 0).unwrap_err();
    assert!(err.contains("quantity below minimum"), "normal order: got {err}");

    place_flagged(&mut ctx, ALICE, 1, PRICE + TICK, QTY, RO)
        .expect("reduce-only is exempt");
}
