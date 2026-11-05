use super::batch_cancel::{
    decode_statuses, expect, lock_abort_counter, register_key, Status, SIGNED_RECV, SIGNED_TS,
};
use super::*;
use crate::{
            batch::{
        self, PerpBatchReason, PerpBatchTag, BASE_BATCH_GAS, BATCH_STATUS_RECORD_LEN,
        MAX_BATCH_PLACE, PLACE_ITEM_ENCODED_LEN,
    },
    interface::IPerpDex::{
        batchPlaceOrdersCall, batchPlaceOrdersSignedCall, OrderPlaced, PlaceItem, Trade,
    },
    types::ApiKey,
    PLACE_ORDER_GAS,
    PerpError,
};
use ed25519_dalek::{Signer, SigningKey};

const ZERO_ID: [u8; 32] = [0u8; 32];
/// A market id that was never registered.
const NO_MARKET: u64 = 999;
/// Registered but `active: false`.
const INACTIVE_MARKET: u64 = 2;

// ── helpers ────────────────────────────────────────────────────────────

fn item_in(
    market_id: u64,
    side: u8,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
) -> PlaceItem {
    PlaceItem {
        marketId: market_id,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
                flags: 0,
    }
}

/// Limit GTC item on the default market.
fn gtc(side: u8, price: u64, qty: u64) -> PlaceItem {
    item_in(MARKET_ID, side, price, qty, 0, 0)
}

fn direct_calldata(items: &[PlaceItem]) -> Vec<u8> {
    batchPlaceOrdersCall {
        orders: items.to_vec(),
    }
    .abi_encode()
}

/// Runs a batch through the real entry point and returns the raw statuses blob.
fn batch_place(ctx: &mut TestCtx, caller: Address, items: &[PlaceItem]) -> Vec<u8> {
    let out = run_perp_dex_call(
        &direct_calldata(items),
        30_000_000,
        caller,
        U256::ZERO,
        false,
        ctx,
    )
    .expect("batch must not hard-fail");
    assert!(
        !out.reverted,
        "batch must return Ok once the loop has begun (reverted with {:?})",
        String::from_utf8_lossy(&out.bytes)
    );
    assert_eq!(
        out.gas_used,
        BASE_BATCH_GAS + items.len() as u64 * PLACE_ORDER_GAS,
        "the FULL dynamic cost must be what the output charges"
    );
    batchPlaceOrdersCall::abi_decode_returns(&out.bytes)
        .unwrap()
        .to_vec()
}

fn nonce(ctx: &mut TestCtx, user: Address) -> u64 {
    storage::load_user_nonce(ctx, user).unwrap()
}

/// The id item `k` of a direct batch must use, given the nonce the batch started from.
fn direct_id(user: Address, base_nonce: u64, ids_consumed_before: u64) -> [u8; 32] {
    derive_order_id(user, base_nonce + ids_consumed_before)
}

fn register_inactive_market(ctx: &mut TestCtx) {
    let mut m = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    m.market_id = INACTIVE_MARKET;
    m.active = false;
    storage::save_market(ctx, &m).unwrap();
}

fn order_placed_ids(ctx: &mut TestCtx) -> Vec<[u8; 32]> {
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|l| l.data.topics().first() == Some(&OrderPlaced::SIGNATURE_HASH))
        .map(|l| {
            OrderPlaced::decode_raw_log(l.data.topics(), &l.data.data)
                .unwrap()
                .orderId
                .0
        })
        .collect()
}

fn trade_count(ctx: &mut TestCtx) -> usize {
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|l| l.data.topics().first() == Some(&Trade::SIGNATURE_HASH))
        .count()
}

// ── 1. happy path: rest + fill in one batch ────────────────────────────

#[test]
fn batch_place_mixes_resting_and_filled_and_is_index_aligned() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    // One resting ask for item 0 to sweep.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let base = nonce(&mut ctx, ALICE);

    let items = [
        gtc(0, PRICE, QTY),            // crosses BOB's ask → fully filled
        gtc(0, PRICE - TICK, QTY),     // rests as a bid
        gtc(1, PRICE + 2 * TICK, QTY), // rests as an ask
    ];
    let blob = batch_place(&mut ctx, ALICE, &items);

    let (id0, id1, id2) = (
        direct_id(ALICE, base, 0),
        direct_id(ALICE, base, 1),
        direct_id(ALICE, base, 2),
    );
    assert_eq!(
        decode_statuses(&blob),
        vec![
            // Terminal ⇒ Filled: it swept the book and left no resting record.
            expect(PerpBatchTag::Filled, id0, PerpBatchReason::None),
            expect(PerpBatchTag::Accepted, id1, PerpBatchReason::None),
            expect(PerpBatchTag::Accepted, id2, PerpBatchReason::None),
        ]
    );
    // Tags agree with storage: Filled ⇒ deleted, Accepted ⇒ live.
    assert_terminal(&mut ctx, id0);
    assert!(storage::load_order(&mut ctx, &id1).unwrap().is_some());
    assert!(storage::load_order(&mut ctx, &id2).unwrap().is_some());
    // Three accepted items consumed exactly three ids.
    assert_eq!(nonce(&mut ctx, ALICE), base + 3);
    // One OrderPlaced per accepted item, in calldata order.
    assert_eq!(order_placed_ids(&mut ctx), vec![id0, id1, id2]);
}

/// A partial fill that rests its remainder is `PartiallyFilled` — NOT terminal, so tag 1.
#[test]
fn partially_filled_remainder_reports_the_resting_tag() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // only QTY on the ask
    let base = nonce(&mut ctx, ALICE);

    let blob = batch_place(&mut ctx, ALICE, &[gtc(0, PRICE, QTY * 2)]);

    let id0 = direct_id(ALICE, base, 0);
    assert_eq!(
        decode_statuses(&blob),
        vec![expect(PerpBatchTag::Accepted, id0, PerpBatchReason::None)]
    );
    let order = get_order(&mut ctx, id0);
    assert_eq!(order.filled, QTY);
    assert_eq!(order.status, OrderStatus::PartiallyFilled);
    assert_eq!(nonce(&mut ctx, ALICE), base + 1);
}

/// An IOC that matches nothing is ACCEPTED but terminal (Expired) — tag 2, not tag 1, and no
/// resting record. Pins the "terminal ⇒ Filled" rule against the non-fill flavour.
#[test]
fn accepted_but_expired_ioc_reports_the_terminal_tag() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let base = nonce(&mut ctx, ALICE);

    let items = [item_in(MARKET_ID, 0, PRICE, QTY, 0, 1)]; // IOC, empty book
    let blob = batch_place(&mut ctx, ALICE, &items);

    let id0 = direct_id(ALICE, base, 0);
    assert_eq!(
        decode_statuses(&blob),
        vec![expect(PerpBatchTag::Filled, id0, PerpBatchReason::None)]
    );
    assert_terminal(&mut ctx, id0);
    // Accepted ⇒ the id was consumed, and OrderPlaced was emitted.
    assert_eq!(nonce(&mut ctx, ALICE), base + 1);
    assert_eq!(order_placed_ids(&mut ctx), vec![id0]);
}

// ── 2. THE id/nonce rule ───────────────────────────────────────────────

/// A rejected item must consume NO id: the accepted items' ids stay gapless, and the nonce
/// advances by exactly the accepted count. (A drifting nonce would let a later batch re-derive an
/// id a resting order already holds, and `save_order` has no collision guard.)
#[test]
fn rejected_items_consume_no_id_so_ids_are_gapless() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let base = nonce(&mut ctx, ALICE);

    let items = [
        gtc(0, PRICE - TICK, QTY),
        item_in(NO_MARKET, 0, PRICE, QTY, 0, 0), // reject
        gtc(0, PRICE - 2 * TICK, QTY),
        item_in(MARKET_ID, 7, PRICE, QTY, 0, 0), // reject: invalid side
        gtc(0, PRICE - 3 * TICK, QTY),
    ];
    let blob = batch_place(&mut ctx, ALICE, &items);

    let (d0, d1, d2) = (
        direct_id(ALICE, base, 0),
        direct_id(ALICE, base, 1),
        direct_id(ALICE, base, 2),
    );
    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(PerpBatchTag::Accepted, d0, PerpBatchReason::None),
            // A rejected item has NO id — the field is zero, not an echo.
            expect(
                PerpBatchTag::Rejected,
                ZERO_ID,
                PerpBatchReason::PlaceUnknownMarket
            ),
            expect(PerpBatchTag::Accepted, d1, PerpBatchReason::None),
            expect(
                PerpBatchTag::Rejected,
                ZERO_ID,
                PerpBatchReason::InvalidSide
            ),
            expect(PerpBatchTag::Accepted, d2, PerpBatchReason::None),
        ]
    );
    for id in [d0, d1, d2] {
        assert!(
            storage::load_order(&mut ctx, &id).unwrap().is_some(),
            "every accepted item must rest under its reported id"
        );
    }
    assert_eq!(
        nonce(&mut ctx, ALICE),
        base + 3,
        "the nonce must advance by the ACCEPTED count (3), not by N (5)"
    );
    // …and the very next single-order placement continues the same chain.
    assert_eq!(place(&mut ctx, ALICE, 0, PRICE - 4 * TICK, QTY, 0, 0), {
        direct_id(ALICE, base, 3)
    });
}

/// Two successive direct batches from the same account must never produce a duplicate live id.
#[test]
fn two_successive_direct_batches_never_reuse_a_live_order_id() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let base = nonce(&mut ctx, ALICE);

    let first = batch_place(
        &mut ctx,
        ALICE,
        &[
            gtc(0, PRICE - TICK, QTY),
            gtc(0, PRICE - 2 * TICK, QTY),
            gtc(0, PRICE - 3 * TICK, QTY),
        ],
    );
    let second = batch_place(
        &mut ctx,
        ALICE,
        &[
            gtc(0, PRICE - 4 * TICK, QTY),
            gtc(0, PRICE - 5 * TICK, QTY),
            gtc(0, PRICE - 6 * TICK, QTY),
        ],
    );

    let mut ids: Vec<[u8; 32]> = decode_statuses(&first)
        .iter()
        .chain(decode_statuses(&second).iter())
        .map(|s| {
            assert_eq!(s.tag, PerpBatchTag::Accepted as u8);
            s.order_id
        })
        .collect();
    assert_eq!(ids.len(), 6);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(*id, direct_id(ALICE, base, i as u64), "id {i} off-chain");
        assert!(
            storage::load_order(&mut ctx, id).unwrap().is_some(),
            "all six orders must be live at once — a reused id would have overwritten one"
        );
    }
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 6, "ids must be pairwise distinct");
    assert_eq!(nonce(&mut ctx, ALICE), base + 6);
}

/// An ALL-rejected batch must leave the nonce key untouched (no redundant same-value write, so
/// the batch contributes zero keys to the block delta).
#[test]
fn all_rejected_batch_writes_nothing_at_all() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let base = nonce(&mut ctx, ALICE);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

    let blob = batch_place(
        &mut ctx,
        ALICE,
        &[
            item_in(NO_MARKET, 0, PRICE, QTY, 0, 0),
            item_in(MARKET_ID, 9, PRICE, QTY, 0, 0),
        ],
    );

    assert!(decode_statuses(&blob)
        .iter()
        .all(|s| s.tag == PerpBatchTag::Rejected as u8 && s.order_id == ZERO_ID));
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes_before,
        "an all-rejected batch must be write-clean, nonce included"
    );
    assert_eq!(nonce(&mut ctx, ALICE), base);
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
}

// ── 3. reject reason codes ─────────────────────────────────────────────

/// Every place-path reject the engine can raise from one call, each mapped to its code — and the
/// whole batch write-clean, which is what the driver's runtime classification keys off.
#[test]
fn every_place_reject_maps_to_its_code_and_is_write_clean() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    register_inactive_market(&mut ctx);
    // A resting ask at PRICE gives the PostOnly-cross and FOK-unfillable cases something real.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

    let cases: [(PlaceItem, PerpBatchReason); 14] = [
        (
            item_in(NO_MARKET, 0, PRICE, QTY, 0, 0),
            PerpBatchReason::PlaceUnknownMarket,
        ),
        (
            item_in(INACTIVE_MARKET, 0, PRICE, QTY, 0, 0),
            PerpBatchReason::MarketNotActive,
        ),
        (
            item_in(MARKET_ID, 7, PRICE, QTY, 0, 0),
            PerpBatchReason::InvalidSide,
        ),
        (
            item_in(MARKET_ID, 0, PRICE, QTY, 9, 0),
            PerpBatchReason::InvalidOrderType,
        ),
        (
            item_in(MARKET_ID, 0, PRICE, QTY, 0, 9),
            PerpBatchReason::InvalidTif,
        ),
        (
            gtc(0, PRICE, QTY / 2),
            PerpBatchReason::QuantityBelowMinimum,
        ),
        (
            gtc(0, PRICE, QTY * 2_000),
            PerpBatchReason::QuantityAboveMaximum,
        ),
        (gtc(0, PRICE, QTY + 1), PerpBatchReason::QuantityStepSize),
        (gtc(0, 0, QTY), PerpBatchReason::PriceZero),
        (
            gtc(0, PRICE * 2_000, QTY),
            PerpBatchReason::PriceAboveMaximum,
        ),
        (gtc(0, PRICE + 1, QTY), PerpBatchReason::PriceTickSize),
        (
            item_in(MARKET_ID, 0, PRICE, QTY, 0, 3), // PostOnly, would cross BOB's ask
            PerpBatchReason::PostOnlyWouldMatch,
        ),
        (
            item_in(MARKET_ID, 0, PRICE - 4 * TICK, QTY, 0, 2), // FOK, nothing to fill against
            PerpBatchReason::FokUnfillable,
        ),
        (
            // Rests (below the ask, so no match) but its margin dwarfs the 10-USDC wallet.
            gtc(0, PRICE - 5 * TICK, QTY * 1_000),
            PerpBatchReason::InsufficientMargin,
        ),
    ];

    let items: Vec<PlaceItem> = cases.iter().map(|(i, _)| i.clone()).collect();
    let blob = batch_place(&mut ctx, ALICE, &items);

    assert_eq!(
        decode_statuses(&blob),
        cases
            .iter()
            .map(|(_, code)| expect(PerpBatchTag::Rejected, ZERO_ID, *code))
            .collect::<Vec<Status>>()
    );
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes_before,
        "every place-path genuine reject must be write-clean"
    );
    assert!(
        JournalTr::take_logs(ctx.journal_mut()).is_empty(),
        "and log-clean: a rejected placement emits no OrderPlaced (Phase 0)"
    );
}

/// The Phase 0 synergy that makes a batch safe: a rejected item emits NO `OrderPlaced`, even
/// though the batch returns `Ok` and therefore never truncates the log stream.
#[test]
fn rejected_item_emits_no_order_placed_while_the_accepted_one_does() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let base = nonce(&mut ctx, ALICE);

    let blob = batch_place(
        &mut ctx,
        ALICE,
        &[
            item_in(MARKET_ID, 0, PRICE, QTY, 0, 3), // PostOnly cross → rejected
            gtc(0, PRICE - TICK, QTY),               // rests
        ],
    );

    let accepted = direct_id(ALICE, base, 0);
    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(
                PerpBatchTag::Rejected,
                ZERO_ID,
                PerpBatchReason::PostOnlyWouldMatch
            ),
            expect(PerpBatchTag::Accepted, accepted, PerpBatchReason::None),
        ]
    );
    assert_eq!(
        order_placed_ids(&mut ctx),
        vec![accepted],
        "exactly one OrderPlaced, for the accepted item only"
    );
}

// ── 4. intra-batch determinism ─────────────────────────────────────────

/// Item `i` rests, item `i+1` crosses it. Items execute IN ORDER and a later item is the taker
/// against the earlier ones, so self-trade prevention applies inside a batch exactly as it does
/// across two transactions: under the default `EXPIRE_MAKER` the later GTC CANCELS the earlier
/// item (reason 9) and rests itself. (Binance does not promise an intra-batch matching order at
/// all — "processed concurrently" — so this is stricter than, not equal to, its behaviour.)
#[test]
fn intra_batch_self_cross_cancels_the_earlier_item() {
    for (maker_side, taker_side) in [(1u8, 0u8), (0, 1)] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let base = nonce(&mut ctx, ALICE);

        let blob = batch_place(
            &mut ctx,
            ALICE,
            &[gtc(maker_side, PRICE, QTY), gtc(taker_side, PRICE, QTY)],
        );

        let (maker, taker) = (direct_id(ALICE, base, 0), direct_id(ALICE, base, 1));
        assert_eq!(
            decode_statuses(&blob),
            vec![
                // `statuses` is the outcome AT THE TIME the item ran: item 0 DID rest. Item 1
                // then cancelled it, so it is terminal by end-of-call — the logs say so.
                expect(PerpBatchTag::Accepted, maker, PerpBatchReason::None),
                expect(PerpBatchTag::Accepted, taker, PerpBatchReason::None),
            ],
            "taker side {taker_side}"
        );
        let logs = JournalTr::take_logs(ctx.journal_mut());
        let cancels: Vec<_> = logs
            .iter()
            .filter(|l| {
                l.data.topics().first()
                    == Some(&crate::interface::IPerpDex::OrderCancelled::SIGNATURE_HASH)
            })
            .map(|l| {
                let e = crate::interface::IPerpDex::OrderCancelled::decode_raw_log(
                    l.data.topics(),
                    &l.data.data,
                )
                .unwrap();
                (e.user, e.orderId.0, e.reason)
            })
            .collect();
        assert_eq!(cancels, vec![(ALICE, maker, 9)], "item 0 cancelled by STP");
        assert!(
            !logs.iter().any(|l| l.data.topics().first() == Some(&Trade::SIGNATURE_HASH)),
            "no self-trade"
        );
        assert_terminal(&mut ctx, maker);
        let t = get_order(&mut ctx, taker);
        assert_eq!((t.status, t.filled), (OrderStatus::Open, 0), "item 1 rests in full");
        assert_eq!(nonce(&mut ctx, ALICE), base + 2);
        assert_eq!(wallet(&mut ctx, ALICE), WALLET);
        let p = pos(&mut ctx, ALICE);
        assert_eq!(p.amount, 0);
        let (bids, asks) = if taker_side == 0 { (QTY, 0) } else { (0, QTY) };
        assert_eq!((p.total_buy_qty, p.total_sell_qty), (bids, asks));
    }
}

/// The FOK form of the same batch still SELF-FILLS: STP does not apply to a FOK taker.
/// Identical to submitting the two as separate transactions in that order.
#[test]
fn intra_batch_self_cross_by_a_fok_item_fills_the_earlier_item() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let base = nonce(&mut ctx, ALICE);

    let blob = batch_place(
        &mut ctx,
        ALICE,
        &[gtc(1, PRICE, QTY), item_in(MARKET_ID, 0, PRICE, QTY, 0, 2)],
    );

    let (maker, taker) = (direct_id(ALICE, base, 0), direct_id(ALICE, base, 1));
    assert_eq!(
        decode_statuses(&blob),
        vec![
            // Item 0 DID rest when it ran; item 1 (FOK) then consumed it.
            expect(PerpBatchTag::Accepted, maker, PerpBatchReason::None),
            expect(PerpBatchTag::Filled, taker, PerpBatchReason::None),
        ]
    );
    assert_terminal(&mut ctx, maker);
    assert_terminal(&mut ctx, taker);
    assert_eq!(nonce(&mut ctx, ALICE), base + 2);
    // Self-trade nets out exactly as the equivalent two single-order txs do.
    assert_eq!(wallet(&mut ctx, ALICE), WALLET);
    assert_eq!(
        pos(&mut ctx, ALICE),
        PerpPosition {
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        }
    );
    assert_eq!(trade_count(&mut ctx), 1);
}

#[test]
fn status_blob_is_byte_identical_across_runs() {
    let scenario = || {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        batch_place(
            &mut ctx,
            ALICE,
            &[
                gtc(0, PRICE, QTY),                      // fills
                item_in(NO_MARKET, 0, PRICE, QTY, 0, 0), // rejected
                gtc(0, PRICE - TICK, QTY),               // rests
                item_in(MARKET_ID, 0, PRICE, QTY, 0, 3), // PostOnly, book now empty → rests
            ],
        )
    };
    let first = scenario();
    assert_eq!(first, scenario());
    assert_eq!(first, scenario());
    assert_eq!(first.len(), 4 * BATCH_STATUS_RECORD_LEN);
}

// ── 5. pre-loop faults revert the whole call ───────────────────────────

#[test]
fn empty_and_oversized_batches_revert_write_clean() {
    for items in [
        Vec::<PlaceItem>::new(),
        vec![gtc(0, PRICE - TICK, QTY); MAX_BATCH_PLACE + 1],
    ] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

        let out = run_perp_dex_call(
            &direct_calldata(&items),
            u64::MAX, // never let gas be the reason
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(
            out.reverted,
            "N = {} must revert the whole call",
            items.len()
        );
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "a pre-loop fault must be write-clean"
        );
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
    }
}

/// N == MAX is accepted: 64 real resting orders, 64 distinct ids, one nonce advance of 64.
#[test]
fn max_batch_place_is_accepted() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, ALICE, WALLET * 20); // 64 rests at ~1 USDC of margin each
    let base = nonce(&mut ctx, ALICE);

    let items: Vec<PlaceItem> = (0..MAX_BATCH_PLACE as u64)
        .map(|i| gtc(0, PRICE - i * TICK, QTY))
        .collect();
    let blob = batch_place(&mut ctx, ALICE, &items);

    assert_eq!(blob.len(), MAX_BATCH_PLACE * BATCH_STATUS_RECORD_LEN);
    let mut ids: Vec<[u8; 32]> = decode_statuses(&blob)
        .iter()
        .map(|s| {
            assert_eq!(s.tag, PerpBatchTag::Accepted as u8, "{s:?}");
            s.order_id
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), MAX_BATCH_PLACE);
    assert_eq!(nonce(&mut ctx, ALICE), base + MAX_BATCH_PLACE as u64);
}

// ── 6. gas ─────────────────────────────────────────────────────────────

#[test]
fn insufficient_gas_limit_is_out_of_gas_before_any_write() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
    let items = [gtc(0, PRICE - TICK, QTY), gtc(0, PRICE - 2 * TICK, QTY)];

    let full = BASE_BATCH_GAS + 2 * PLACE_ORDER_GAS;
    let err = run_perp_dex_call(
        &direct_calldata(&items),
        full - 1,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, PerpError::OutOfGas), "got {err:?}");
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes_before,
        "OutOfGas must precede every write"
    );
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());

    // Exactly the computed cost succeeds and charges exactly that.
    let out = run_perp_dex_call(
        &direct_calldata(&items),
        full,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!out.reverted);
    assert_eq!(out.gas_used, full);
}

/// The 224-byte element stride must be what backs the pre-decode length bound. The decisive case
/// is `stride confusion`: a declared length that a 32-byte-per-element bound would accept and a
/// 256-byte-per-element bound must reject.
#[test]
fn hostile_length_word_reverts_without_allocating() {
    // 8 words: marketId, side, price, quantity, orderType, tif, clientOrderId, flags.
    assert_eq!(PLACE_ITEM_ENCODED_LEN, 256);
    // selector || offset(0x20) || length || <body>
    let head = |len: u32, body: usize| {
        let mut v = vec![0u8; 4 + 64 + body];
        v[..4].copy_from_slice(&batchPlaceOrdersCall::SELECTOR);
        v[35] = 0x20;
        v[64..68].copy_from_slice(&len.to_be_bytes());
        v
    };

    let huge_u32 = head(u32::MAX, 0);
    let mut huge_u256 = head(1, 0);
    huge_u256[36..68].fill(0xff);
    let mut bad_offset = head(1, PLACE_ITEM_ENCODED_LEN);
    bad_offset[35] = 0x40;
    // 8 items declared, 8*32 bytes of body: enough for a bytes32[] of 8, 8× short for PlaceItem[].
    let stride_confusion = head(8, 8 * 32);
    // One item declared, one word short of its 256 bytes.
    let truncated = head(1, PLACE_ITEM_ENCODED_LEN - 32);

    for (name, input) in [
        ("u32::MAX length", huge_u32),
        ("u256 length", huge_u256),
        ("non-canonical offset", bad_offset),
        ("32-byte-stride body for a 256-byte item", stride_confusion),
        ("truncated final item", truncated),
    ] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
        let out =
            run_perp_dex_call(&input, u64::MAX, ALICE, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted, "{name} must revert");
        assert_eq!(
            out.gas_used, BASE_BATCH_GAS,
            "{name}: an unreadable length word charges only the envelope floor"
        );
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "{name} must be write-clean"
        );
    }

    // A well-formed one-item call is exactly selector + offset + length + one 224-byte item, and
    // the pre-decode read agrees with the decoder.
    let good = direct_calldata(&[gtc(0, PRICE, QTY)]);
    assert_eq!(good.len(), 4 + 32 + 32 + PLACE_ITEM_ENCODED_LEN);
    assert_eq!(
        batch::PLACE_DIRECT_LAYOUT.checked_len(&good).unwrap(),
        1,
        "the pre-decode read must accept the canonical encoding"
    );
    assert_eq!(
        batch::batch_dynamic_gas(batchPlaceOrdersCall::SELECTOR, &good),
        Some(BASE_BATCH_GAS + PLACE_ORDER_GAS)
    );
}

#[test]
fn gas_math_saturates_instead_of_wrapping() {
    assert_eq!(batch::batch_gas(usize::MAX, PLACE_ORDER_GAS), u64::MAX);
    assert_eq!(
        batch::batch_gas(MAX_BATCH_PLACE, PLACE_ORDER_GAS),
        BASE_BATCH_GAS + MAX_BATCH_PLACE as u64 * PLACE_ORDER_GAS
    );
    assert!(batch::batch_dynamic_gas(batchPlaceOrdersCall::SELECTOR, &[0u8; 8]).is_none());
}

// ── 7. signed batch ────────────────────────────────────────────────────

fn sign(
    items: &[PlaceItem],
    sk: &SigningKey,
    account: Address,
    ts: u64,
    recv: u64,
) -> [u8; 64] {
    let msg = batch_place_message(TEST_CHAIN_ID, account, 0, ts, recv, items);
    sk.sign(&msg).to_bytes()
}

fn signed_calldata(
    sk: &SigningKey,
    account: Address,
    timestamp: u64,
    recv_window: u64,
    items: &[PlaceItem],
    // Items actually put on the wire; `None` = same as the signed set.
    wire_items: Option<&[PlaceItem]>,
) -> Vec<u8> {
    batchPlaceOrdersSignedCall {
        account,
        keyId: 0,
        timestamp,
        recvWindow: recv_window,
        orders: wire_items.unwrap_or(items).to_vec(),
        signature: sign(items, sk, account, timestamp, recv_window)
            .to_vec()
            .into(),
    }
    .abi_encode()
}

/// `orderId[k] = keccak256(signature || u32BE(k))`.
fn expected_signed_id(signature: &[u8; 64], k: u32) -> [u8; 32] {
    let mut buf = [0u8; 68];
    buf[..64].copy_from_slice(signature);
    buf[64..].copy_from_slice(&k.to_be_bytes());
    primitives::keccak256(buf).0
}

#[test]
fn signed_batch_place_happy_path_with_index_distinct_ids() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register_key(&mut ctx, ALICE, &sk);
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let base = nonce(&mut ctx, ALICE);

    let items = [
        gtc(0, PRICE, QTY),        // fills against BOB
        gtc(0, PRICE - TICK, QTY), // rests
        gtc(1, PRICE + TICK, QTY), // rests
    ];
    let input = signed_calldata(&sk, ALICE, SIGNED_TS, SIGNED_RECV, &items, None);
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(!out.reverted, "{:?}", String::from_utf8_lossy(&out.bytes));
    assert_eq!(out.gas_used, BASE_BATCH_GAS + 3 * PLACE_ORDER_GAS);

    let sig = sign(&items, &sk, ALICE, SIGNED_TS, SIGNED_RECV);
    let ids = [
        expected_signed_id(&sig, 0),
        expected_signed_id(&sig, 1),
        expected_signed_id(&sig, 2),
    ];
    let blob = batchPlaceOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(PerpBatchTag::Filled, ids[0], PerpBatchReason::None),
            expect(PerpBatchTag::Accepted, ids[1], PerpBatchReason::None),
            expect(PerpBatchTag::Accepted, ids[2], PerpBatchReason::None),
        ]
    );
    // Pairwise distinct — the raw keccak256(signature) of the single-order path would have
    // handed all three the SAME id and silently overwritten two orders.
    let mut sorted = ids;
    sorted.sort_unstable();
    assert!(sorted.windows(2).all(|w| w[0] != w[1]));
    assert!(storage::load_order(&mut ctx, &ids[1]).unwrap().is_some());
    assert!(storage::load_order(&mut ctx, &ids[2]).unwrap().is_some());
    // The signed path does NOT touch the per-user nonce.
    assert_eq!(nonce(&mut ctx, ALICE), base);
    assert_eq!(order_placed_ids(&mut ctx), ids.to_vec());
}

#[test]
fn signed_batch_rejects_tampered_content() {
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let a = gtc(0, PRICE - TICK, QTY);
    let b = gtc(0, PRICE - 2 * TICK, QTY);
    let c = gtc(0, PRICE - 3 * TICK, QTY);
    // Same item as `a` with one field changed.
    let a_price = gtc(0, PRICE - 5 * TICK, QTY);
    let a_qty = gtc(0, PRICE - TICK, QTY * 2);
    let mut a_cloid = a.clone();
    a_cloid.clientOrderId = FixedBytes([9u8; 16]);

    for (name, signed, wire) in [
        (
            "item price changed",
            vec![a.clone(), b.clone()],
            vec![a_price, b.clone()],
        ),
        (
            "item quantity changed",
            vec![a.clone(), b.clone()],
            vec![a_qty, b.clone()],
        ),
        (
            "clientOrderId changed",
            vec![a.clone(), b.clone()],
            vec![a_cloid, b.clone()],
        ),
        (
            "N grew",
            vec![a.clone(), b.clone()],
            vec![a.clone(), b.clone(), c.clone()],
        ),
        ("N shrank", vec![a.clone(), b.clone()], vec![a.clone()]),
        (
            "order permuted",
            vec![a.clone(), b.clone()],
            vec![b.clone(), a.clone()],
        ),
    ] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        register_key(&mut ctx, ALICE, &sk);
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
        // Drain setup's own events: account funding now emits AccountBalanceChanged at the
        // write site, so only the logs produced by the call below are under test here.
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let input = signed_calldata(&sk, ALICE, SIGNED_TS, SIGNED_RECV, &signed, Some(&wire));
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted, "{name} must fail verification");
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "{name}: signature failure is pre-write"
        );
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
    }
}

/// A replayed signed batch must REVERT rather than re-derive its (live) ids: the ids are a pure
/// function of the signature, so without the burn the second run would `save_order` over the
/// first run's resting orders.
#[test]
fn signed_batch_replay_reverts_instead_of_re_deriving_live_ids() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register_key(&mut ctx, ALICE, &sk);

    let items = [
        gtc(0, PRICE - TICK, QTY),
        item_in(NO_MARKET, 0, PRICE, QTY, 0, 0), // rejected → partial acceptance
    ];
    let input = signed_calldata(&sk, ALICE, SIGNED_TS, SIGNED_RECV, &items, None);
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(!out.reverted);
    let blob = batchPlaceOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
    let st = decode_statuses(&blob);
    assert_eq!(st[0].tag, PerpBatchTag::Accepted as u8);
    assert_eq!(st[1].tag, PerpBatchTag::Rejected as u8);
    let live = st[0].order_id;

    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(out.reverted, "a replayed batch must revert");
    assert!(String::from_utf8_lossy(&out.bytes).contains("duplicate signature"));
    assert!(
        storage::load_order(&mut ctx, &live).unwrap().is_some(),
        "the first run's order must still be there, untouched"
    );
}

#[test]
fn signed_batch_requires_key_window_and_expiry() {
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let items = [gtc(0, PRICE - TICK, QTY)];

    // No key registered.
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let input = signed_calldata(&sk, ALICE, SIGNED_TS, SIGNED_RECV, &items, None);
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(out.reverted);
    assert!(String::from_utf8_lossy(&out.bytes).contains("no api key"));

    // Timestamp in the future.
    let mut ctx = make_ctx();
    setup(&mut ctx);
    register_key(&mut ctx, ALICE, &sk);
    let input = signed_calldata(&sk, ALICE, 10_000, SIGNED_RECV, &items, None);
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(out.reverted);
    assert!(String::from_utf8_lossy(&out.bytes).contains("future"));
}

/// The digest is the TIGHT 43-bytes-per-item packing, not the 224-byte ABI encoding.
#[test]
fn signed_digest_layout_is_pinned() {
    let items = [gtc(0, PRICE, QTY), gtc(1, PRICE + TICK, QTY * 2)];
    let msg = batch_place_message(TEST_CHAIN_ID, ALICE, 3, 111, 22, &items);
    // ⚠️ Deliberately LITERAL offsets. This test's job is to pin the wire layout, so it must
    // not be rewritten in terms of the production cursor helpers — it would then agree with
    // whatever the production code does, including a mistake. Every other signed-message
    // builder in this file DOES reuse those helpers, because their job is behaviour.
    assert_eq!(msg.len(), 22 + 8 + 20 + 1 + 8 + 8 + 4 + 2 * 44);
    assert_eq!(&msg[..22], b"perpdex_v1_batch_order");
    assert_eq!(&msg[22..30], &TEST_CHAIN_ID.to_be_bytes(), "chainId follows the prefix");
    assert_eq!(&msg[30..50], ALICE.as_slice());
    assert_eq!(msg[50], 3);
    assert_eq!(&msg[51..59], &111u64.to_be_bytes());
    assert_eq!(&msg[59..67], &22u64.to_be_bytes());
    assert_eq!(&msg[67..71], &2u32.to_be_bytes());
    // First item, tightly packed.
    assert_eq!(&msg[71..79], &MARKET_ID.to_be_bytes());
    assert_eq!(msg[79], 0);
    assert_eq!(&msg[80..88], &PRICE.to_be_bytes());
    assert_eq!(&msg[88..96], &QTY.to_be_bytes());
    assert_eq!(msg[96], 0);
    assert_eq!(msg[97], 0);
    assert_eq!(&msg[98..114], &[0u8; 16]);
    assert_eq!(msg[114], 0, "flags");
    // Second item starts right after.
    assert_eq!(&msg[115..123], &MARKET_ID.to_be_bytes());
    assert_eq!(msg[123], 1);
}

#[test]
fn batch_place_selectors_are_not_static_callable() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let err = run_perp_dex_call(
        &direct_calldata(&[gtc(0, PRICE, QTY)]),
        30_000_000,
        ALICE,
        U256::ZERO,
        true,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, PerpError::StaticRestrictionViolation),
        "got {err:?}"
    );
}

// ── 8. abort-forward on the PLACE path ─────────────────────────────────

/// Arms the one post-write error the place path still has: the fee recipient is cleared while the
/// taker still owes a taker fee, so `credit_fee_recipient` — which runs in `finalize_apply`,
/// AFTER `registry.flush` committed the fill — raises "fee recipient not initialised".
///
/// ALICE pays a 1% taker fee; BOB (the maker) pays none, which is what keeps the walk's own
/// admin check (`settle_maker_fill_registry`, only reached when `maker_fee > 0`) from catching it
/// pre-flush. Leaves a resting ask at PRICE for the crossing item to consume.
fn arm_post_write_place_abort(ctx: &mut TestCtx) {
    storage::save_user_fee_rates(
        ctx,
        ALICE,
        UserFeeRates {
            maker_fee_ppm: 0,
            taker_fee_ppm: 10_000,
        },
    )
    .unwrap();
    place(ctx, BOB, 1, PRICE, QTY, 0, 0);
    storage::save_admin(ctx, Address::ZERO).unwrap();
}

/// End-to-end abort-forward on `batchPlaceOrders` (the cancel-path analogue is
/// `batch_cancel_aborts_forward_on_a_post_write_invariant`): the crossing item writes and THEN
/// fails, so it is `Aborted`, the tail is `NotAttempted`, the call still returns `Ok`, the
/// committed prefix keeps its order — and the nonce advances PAST the aborted item, so its id can
/// never be re-derived (`save_order` has no collision guard). The aborted record reports that
/// burned id, which is the only way a caller can learn which one was consumed.
#[test]
fn aborts_forward_on_a_post_write_place_error() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    arm_post_write_place_abort(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let base = nonce(&mut ctx, ALICE);
    let _abort_guard = lock_abort_counter();
    let aborts_before = batch::perp_batch_abort_count();

    let items = [
        gtc(0, PRICE - TICK, QTY),     // rests → the committed prefix
        gtc(0, PRICE, QTY),            // crosses BOB's ask → writes, then the fee reject
        gtc(0, PRICE - 2 * TICK, QTY), // never attempted
    ];
    // `batch_place` asserts the call did NOT revert.
    let blob = batch_place(&mut ctx, ALICE, &items);

    let (id0, id1) = (direct_id(ALICE, base, 0), direct_id(ALICE, base, 1));
    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(PerpBatchTag::Accepted, id0, PerpBatchReason::None),
            expect(
                PerpBatchTag::Aborted,
                id1,
                PerpBatchReason::FeeRecipientNotSet
            ),
            expect(PerpBatchTag::NotAttempted, ZERO_ID, PerpBatchReason::None),
        ]
    );
    assert_eq!(batch::perp_batch_abort_count(), aborts_before + 1);
    assert!(
        storage::load_order(&mut ctx, &id0).unwrap().is_some(),
        "the committed prefix must survive the abort, with its order intact"
    );
    assert_eq!(
        nonce(&mut ctx, ALICE),
        base + 2,
        "consumed = 1 accepted + 1 aborted: the aborted id must be BURNED, not reusable"
    );
    // The next placement continues past the burned id rather than re-deriving it.
    let next = place(&mut ctx, ALICE, 0, PRICE - 5 * TICK, QTY, 0, 0);
    assert_ne!(next, id1, "an aborted item's id must never be re-derived");
    assert_eq!(next, direct_id(ALICE, base, 2));
}

/// A zero-fill GTC whose rest is unaffordable is a ROUTINE user reject, and must stay one: the
/// match walk enters the crossing level (its only queued id is stale, so nothing fills) and
/// records a `SaveLevel`, but that write must not commit — otherwise the driver's write-counter
/// witness sees movement and mis-classifies the reject as an `Aborted`, dropping the rest of the
/// batch. Third-party influenceable (anyone can leave a stale id at the victim's price), which is
/// why it is pinned end-to-end.
#[test]
fn zero_fill_rest_reject_is_a_reject_not_an_abort() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let stale = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    storage::delete_order(&mut ctx, &stale).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let base = nonce(&mut ctx, ALICE);
    let _abort_guard = lock_abort_counter();
    let aborts_before = batch::perp_batch_abort_count();

    let blob = batch_place(
        &mut ctx,
        ALICE,
        &[
            // Crosses the stale level, fills nothing, then wants 11 USDC of margin for the rest
            // out of a 10 USDC wallet.
            gtc(0, PRICE, QTY * 11),
            gtc(0, PRICE - TICK, QTY), // must still be attempted
        ],
    );

    let id1 = direct_id(ALICE, base, 0);
    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(
                PerpBatchTag::Rejected,
                ZERO_ID,
                PerpBatchReason::InsufficientMargin
            ),
            expect(PerpBatchTag::Accepted, id1, PerpBatchReason::None),
        ]
    );
    assert_eq!(
        batch::perp_batch_abort_count(),
        aborts_before,
        "the reject is write-clean, so nothing may be counted as an abort"
    );
    assert_eq!(nonce(&mut ctx, ALICE), base + 1);
}

/// The nonce advance must be CHECKED: a saturating add would clamp at `u64::MAX` and leave the
/// nonce on a value already spent, so the next placement would re-derive a live id.
#[test]
fn nonce_overflow_is_rejected_rather_than_clamped() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let err = commit_batch_order_nonce(&mut ctx, ALICE, u64::MAX, 1, None).unwrap_err();
    assert!(err.to_string().contains("order nonce overflow"), "{err}");
    assert_eq!(nonce(&mut ctx, ALICE), 0, "and nothing was written");
}

/// An over-cap batch reverts in `check_batch_len` having done ZERO work, so it must be billed the
/// envelope floor only — never `BASE + N * unit` for work it never performs.
#[test]
fn over_cap_batch_is_billed_only_the_envelope_floor() {
    let over = direct_calldata(&vec![gtc(0, PRICE - TICK, QTY); MAX_BATCH_PLACE + 1]);
    assert_eq!(
        batch::batch_dynamic_gas(batchPlaceOrdersCall::SELECTOR, &over),
        None,
        "an over-cap declared length must not be billed per item"
    );

    let mut ctx = make_ctx();
    setup(&mut ctx);
    let out = run_perp_dex_call(&over, u64::MAX, ALICE, U256::ZERO, false, &mut ctx).unwrap();
    assert!(out.reverted, "over-cap must still revert the whole call");
    assert!(String::from_utf8_lossy(&out.bytes).contains("batch too large"));
    assert_eq!(out.gas_used, BASE_BATCH_GAS);

    // N == MAX is billed in full, so the clamp cannot silently under-bill a legal batch.
    let at_cap = direct_calldata(&vec![gtc(0, PRICE - TICK, QTY); MAX_BATCH_PLACE]);
    assert_eq!(
        batch::batch_dynamic_gas(batchPlaceOrdersCall::SELECTOR, &at_cap),
        Some(BASE_BATCH_GAS + MAX_BATCH_PLACE as u64 * PLACE_ORDER_GAS)
    );
}

// ── 9. signed: header tampering + the burn under an abort ──────────────

/// Every HEADER field is inside the digest, not just the items: `account`, `keyId`, `timestamp`
/// and `recvWindow` (item fields / N / permutation live in `signed_batch_rejects_tampered_content`).
/// A key is registered for BOTH accounts under BOTH key ids so each case fails on VERIFICATION and
/// not for a boring reason like "no api key", and the tampered timestamp/recvWindow are chosen to
/// still pass `check_recv_window` (skew 5s, window clamped to 60s).
#[test]
fn signed_batch_rejects_tampered_header_fields() {
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let items = [gtc(0, PRICE - TICK, QTY)];
    // Signed under ALICE / keyId 0 / SIGNED_TS / SIGNED_RECV.
    let sig = sign(&items, &sk, ALICE, SIGNED_TS, SIGNED_RECV);
    let wire = |account: Address, key_id: u8, ts: u64, recv: u64| {
        batchPlaceOrdersSignedCall {
            account,
            keyId: key_id,
            timestamp: ts,
            recvWindow: recv,
            orders: items.to_vec(),
            signature: sig.to_vec().into(),
        }
        .abi_encode()
    };

    for (name, input) in [
        ("account swapped", wire(BOB, 0, SIGNED_TS, SIGNED_RECV)),
        ("keyId changed", wire(ALICE, 1, SIGNED_TS, SIGNED_RECV)),
        (
            "timestamp changed",
            wire(ALICE, 0, SIGNED_TS + 1, SIGNED_RECV),
        ),
        (
            "recvWindow changed",
            wire(ALICE, 0, SIGNED_TS, SIGNED_RECV + 1),
        ),
    ] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        for user in [ALICE, BOB] {
            for key_id in [0u8, 1] {
                storage::save_api_key(
                    &mut ctx,
                    user,
                    key_id,
                    ApiKey {
                        pubkey: sk.verifying_key().to_bytes(),
                        expiry: 0,
                    },
                )
                .unwrap();
            }
        }
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
        // Drain setup's own events (funding emits AccountBalanceChanged at the write site) so
        // only the logs produced by the call below are under test.
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted, "{name} must fail verification");
        let reason = String::from_utf8_lossy(&out.bytes).to_string();
        assert!(
            reason.contains("signature verification failed"),
            "{name} must fail on the SIGNATURE, got {reason}"
        );
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "{name}: signature failure is pre-write"
        );
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
    }
}

/// The signature is burned BEFORE the loop, unconditionally — so a batch that ABORTS mid-way
/// (returning `Ok`, with committed writes) can never be replayed to re-run its tail.
#[test]
fn signed_batch_abort_still_burns_the_signature() {
    // Produces an abort (bumps the global counter) without asserting a delta — same
    // reason as `driver_classifies_by_write_count_not_by_message`: it must hold the lock.
    let _abort_guard = lock_abort_counter();
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register_key(&mut ctx, ALICE, &sk);
    arm_post_write_place_abort(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let items = [
        gtc(0, PRICE, QTY),        // crosses BOB's ask → writes, then the fee reject
        gtc(0, PRICE - TICK, QTY), // never attempted
    ];
    let input = signed_calldata(&sk, ALICE, SIGNED_TS, SIGNED_RECV, &items, None);
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(!out.reverted, "an abort must still return Ok");
    let blob = batchPlaceOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
    let st = decode_statuses(&blob);
    let sig = sign(&items, &sk, ALICE, SIGNED_TS, SIGNED_RECV);
    assert_eq!(
        st,
        vec![
            // The signed path reports the aborted item's derived id too.
            expect(
                PerpBatchTag::Aborted,
                expected_signed_id(&sig, 0),
                PerpBatchReason::FeeRecipientNotSet
            ),
            expect(PerpBatchTag::NotAttempted, ZERO_ID, PerpBatchReason::None),
        ]
    );

    // Replay of the very same signature reverts the whole call.
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(
        out.reverted,
        "an aborted batch must still burn its signature"
    );
    assert!(String::from_utf8_lossy(&out.bytes).contains("duplicate signature"));
}

// ── 10. batch single-user working-set equivalence (this PR) ─────────────
//
// The working-set relocates the initiator's account/position/order-lists into a batch-scoped
// local, flushed to the main store ONCE at end-of-batch. These three tests pin the property
// that makes it a pure perf refactor: the net state — and the off-trie perp delta / block
// commitment — a batch produces is IDENTICAL to the same operations run as separate
// single-order txs, including the abort-forward no-undo case and a same-initiator self-match
// (maker == taker == the working-set owner), which is the split-copy double-spend risk.

/// Places one order through the SAME entry point a batch uses (`run_perp_dex_call`), so a
/// batch-vs-per-item comparison differs only in the batching, never in the entry path.
fn single_place(ctx: &mut TestCtx, caller: Address, item: &PlaceItem) {
    let input = placeOrderCall {
        marketId: item.marketId,
        side: item.side,
        price: item.price,
        quantity: item.quantity,
        orderType: item.orderType,
        tif: item.tif,
        clientOrderId: item.clientOrderId,
                flags: 0,
    }
    .abi_encode();
    let out = run_perp_dex_call(&input, 30_000_000, caller, U256::ZERO, false, ctx)
        .expect("single place must not hard-fail");
    assert!(
        !out.reverted,
        "single place reverted: {}",
        String::from_utf8_lossy(&out.bytes)
    );
}

/// Full observable state the working-set hoists: position, visible + total perp collateral, and
/// both order-entry lists.
fn user_state(
    ctx: &mut TestCtx,
    user: Address,
) -> (
    PerpPosition,
    u64,
    Vec<crate::types::OrderEntry>,
    Vec<crate::types::OrderEntry>,
) {
    let p = storage::load_position(ctx, user, MARKET_ID).unwrap();
    let acct = storage::load_account(ctx, user).unwrap();
    let buy = storage::load_buy_orders(ctx, user, MARKET_ID).unwrap();
    let sell = storage::load_sell_orders(ctx, user, MARKET_ID).unwrap();
    (
        p,
        acct.visible_perp_wallet_balance(),
        Vec::from(buy),
        Vec::from(sell),
    )
}

/// The block's net off-trie perp delta folded into a commitment (drains the dirty set — call
/// last). Equal commitments ⇒ byte-identical net write-set ⇒ golden-neutral.
fn perp_commitment(ctx: &mut TestCtx) -> U256 {
    let delta = ctx.journal_mut().take_perp_delta();
    storage::compute_block_commitment(U256::ZERO, &delta)
}

/// Runs `items` as ONE batch and as separate single-order txs on twin contexts, asserts the
/// state, the nonce and the folded perp delta are identical, and returns `(statuses, batch
/// logs, per-tx logs)` with the `AccountBalanceChanged` rows dropped from both log streams —
/// those are coalesced per CALL by the end-of-call drain, so a batch legitimately merges them.
fn batch_equals_separate_txs(
    items: &[PlaceItem],
) -> (Vec<Status>, Vec<primitives::Log>, Vec<primitives::Log>) {
    let no_drain_rows = |logs: Vec<primitives::Log>| -> Vec<primitives::Log> {
        logs.into_iter()
            .filter(|l| {
                l.data.topics().first() != Some(&AccountBalanceChanged::SIGNATURE_HASH)
            })
            .collect()
    };
    // Batch path (one call, working-set active).
    let mut cb = make_ctx();
    setup(&mut cb);
    let _ = JournalTr::take_logs(cb.journal_mut());
    let blob = batch_place(&mut cb, ALICE, items);
    let batch_logs = no_drain_rows(JournalTr::take_logs(cb.journal_mut()));

    // Per-item path (same entry point, separate txs).
    let mut cs = make_ctx();
    setup(&mut cs);
    let _ = JournalTr::take_logs(cs.journal_mut());
    assert_eq!(nonce(&mut cs, ALICE), nonce(&mut cb, ALICE) - items.len() as u64);
    let mut single_logs = Vec::new();
    for it in items {
        single_place(&mut cs, ALICE, it);
        single_logs.extend(no_drain_rows(JournalTr::take_logs(cs.journal_mut())));
    }

    // Single coherent copy: the batch state equals the per-tx state — no double-spend.
    assert_eq!(
        user_state(&mut cb, ALICE),
        user_state(&mut cs, ALICE),
        "self-match batch state must equal separate single-order txs"
    );
    assert_eq!(nonce(&mut cb, ALICE), nonce(&mut cs, ALICE));
    // And the off-trie perp delta folds to the same commitment (golden-neutral).
    assert_eq!(
        perp_commitment(&mut cb),
        perp_commitment(&mut cs),
        "self-match batch perp delta must equal the per-tx delta"
    );
    (decode_statuses(&blob), batch_logs, single_logs)
}

/// A same-initiator self-match INSIDE one batch, in the one form that still TRADES: item 0
/// rests ALICE's bid, item 1 is ALICE's crossing FOK sell (STP does not apply to FOK) that
/// consumes it. ALICE is BOTH the resting maker and the taker, so the working-set must serve
/// ONE coherent copy to both sides of the match — a split ws/main copy would double-spend. The
/// batch must net out to EXACTLY the two-separate-tx result.
#[test]
fn initiator_self_match_within_batch() {
    let items = [
        gtc(0, PRICE, QTY),                      // rests as ALICE's bid
        item_in(MARKET_ID, 1, PRICE, QTY, 0, 2), // ALICE's FOK sell fills her own bid
    ];
    let base = {
        let mut c = make_ctx();
        setup(&mut c);
        nonce(&mut c, ALICE)
    };
    let (statuses, _, _) = batch_equals_separate_txs(&items);
    assert_eq!(
        statuses,
        vec![
            expect(
                PerpBatchTag::Accepted,
                direct_id(ALICE, base, 0),
                PerpBatchReason::None
            ),
            expect(
                PerpBatchTag::Filled,
                direct_id(ALICE, base, 1),
                PerpBatchReason::None
            ),
        ],
        "item 0 rested, item 1 self-matched it (fully filled)"
    );
}

/// The GTC form (T19), both side orders: under the default `EXPIRE_MAKER` item 1 cancels item 0
/// (reason 9) and rests in full — `[Accepted, Accepted]`. The batch equals the two separate txs
/// in state, nonce and commitment, AND item for item in its log stream (drain rows aside): the
/// earlier item's `OrderPlaced` → `OrderRested`, then the later item's `OrderPlaced` → the STP
/// `OrderCancelled` of the earlier one → its own `OrderRested`. The `OrderCancelled ⇒ prior
/// OrderRested` guard sees item 0 rest in the SAME call before it is cancelled, so it passes.
#[test]
fn initiator_self_cross_within_batch_is_prevented_like_two_txs() {
    use crate::events::stream_test_support::stream_shape;
    for (maker_side, taker_side) in [(0u8, 1u8), (1, 0)] {
        let items = [gtc(maker_side, PRICE, QTY), gtc(taker_side, PRICE, QTY)];
        let base = {
            let mut c = make_ctx();
            setup(&mut c);
            nonce(&mut c, ALICE)
        };
        let (statuses, batch_logs, single_logs) = batch_equals_separate_txs(&items);
        let (id0, id1) = (direct_id(ALICE, base, 0), direct_id(ALICE, base, 1));
        assert_eq!(
            statuses,
            vec![
                expect(PerpBatchTag::Accepted, id0, PerpBatchReason::None),
                expect(PerpBatchTag::Accepted, id1, PerpBatchReason::None),
            ],
            "taker side {taker_side}"
        );
        assert_eq!(
            batch_logs, single_logs,
            "item-for-item the same log stream as two separate txs"
        );
        assert_eq!(
            stream_shape(&batch_logs),
            vec![
                ("OrderPlaced", Some(ALICE)),
                ("OrderRested", Some(ALICE)),
                ("OrderPlaced", Some(ALICE)),
                ("OrderCancelled", Some(ALICE)),
                ("OrderRested", Some(ALICE)),
            ]
        );
        let cancelled = batch_logs
            .iter()
            .find(|l| {
                l.data.topics().first()
                    == Some(&crate::interface::IPerpDex::OrderCancelled::SIGNATURE_HASH)
            })
            .map(|l| {
                crate::interface::IPerpDex::OrderCancelled::decode_raw_log(
                    l.data.topics(),
                    &l.data.data,
                )
                .unwrap()
            })
            .unwrap();
        assert_eq!((cancelled.orderId.0, cancelled.reason), (id0, 9));
    }
}

/// A post-write abort on the INITIATOR inside a batch (reusing `arm_post_write_place_abort`):
/// the crossing item fills ALICE against BOB — writing ALICE's working-set position/account —
/// and THEN the fee-recipient reject fires. It must be `Aborted`, the tail `NotAttempted`, the
/// call still `Ok`, the committed prefix present, and — the working-set-specific angle — the
/// aborted item's PARTIAL writes must be flushed and KEPT (commit-only has no undo: the ws is
/// flushed on the abort path, never restored).
#[test]
fn batch_post_write_abort_still_caught() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    arm_post_write_place_abort(&mut ctx); // ALICE taker-fee 100bps, BOB rests ask @ PRICE, admin=ZERO
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let base = nonce(&mut ctx, ALICE);
    let _abort_guard = lock_abort_counter();
    let aborts_before = batch::perp_batch_abort_count();

    let items = [
        gtc(0, PRICE - TICK, QTY),     // item 0: rests → the committed prefix
        gtc(0, PRICE, QTY),            // item 1: crosses BOB, writes, then fee reject
        gtc(0, PRICE - 2 * TICK, QTY), // item 2: never attempted
    ];
    // `batch_place` asserts the call did NOT revert (abort-forward returns Ok).
    let blob = batch_place(&mut ctx, ALICE, &items);

    let (id0, id1) = (direct_id(ALICE, base, 0), direct_id(ALICE, base, 1));
    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(PerpBatchTag::Accepted, id0, PerpBatchReason::None),
            expect(
                PerpBatchTag::Aborted,
                id1,
                PerpBatchReason::FeeRecipientNotSet
            ),
            expect(PerpBatchTag::NotAttempted, ZERO_ID, PerpBatchReason::None),
        ],
        "post-write error on the initiator → Aborted, tail NotAttempted"
    );
    assert_eq!(batch::perp_batch_abort_count(), aborts_before + 1);

    // Committed prefix (item 0's resting bid) survives with its order intact.
    assert!(
        storage::load_order(&mut ctx, &id0).unwrap().is_some(),
        "committed prefix order must survive the abort"
    );
    // The ABORTED item's partial writes are KEPT, not restored: item 1's fill opened ALICE +QTY
    // long in the working-set, which the abort path flushed to the main store.
    assert_eq!(
        pos(&mut ctx, ALICE).amount,
        QTY as i64,
        "the aborted item's partial fill must remain committed (flushed, not restored)"
    );
    assert_eq!(
        pos(&mut ctx, BOB).amount,
        -(QTY as i64),
        "BOB (non-initiator maker, written straight to main) took the other side"
    );
    // Nonce advanced past the burned aborted id (1 accepted + 1 aborted both consume an id).
    assert_eq!(nonce(&mut ctx, ALICE), base + 2);
}

/// A fully-accepted multi-item batch (one crossing a maker, two resting) must produce the
/// identical final state — and the identical off-trie perp delta / commitment — as the same
/// operations run as N single-order txs: the flush-once ≡ write-through equivalence.
#[test]
fn batch_all_accepted_flush_equals_per_item() {
    let items = [
        gtc(0, PRICE, QTY),            // crosses BOB's resting ask → fills
        gtc(0, PRICE - TICK, QTY),     // rests as a bid
        gtc(1, PRICE + 2 * TICK, QTY), // rests as an ask
    ];

    let mut cb = make_ctx();
    setup(&mut cb);
    place(&mut cb, BOB, 1, PRICE, QTY, 0, 0); // maker ALICE's item 0 sweeps
    let _ = JournalTr::take_logs(cb.journal_mut());
    batch_place(&mut cb, ALICE, &items);

    let mut cs = make_ctx();
    setup(&mut cs);
    place(&mut cs, BOB, 1, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(cs.journal_mut());
    for it in &items {
        single_place(&mut cs, ALICE, it);
    }

    // Both the initiator and the (non-hoisted) maker end identically.
    assert_eq!(
        user_state(&mut cb, ALICE),
        user_state(&mut cs, ALICE),
        "ALICE (initiator) state must match per-item"
    );
    assert_eq!(
        user_state(&mut cb, BOB),
        user_state(&mut cs, BOB),
        "BOB (maker) state must match per-item"
    );
    assert_eq!(nonce(&mut cb, ALICE), nonce(&mut cs, ALICE));
    // The whole net off-trie perp delta folds to the same commitment (golden-neutral).
    assert_eq!(
        perp_commitment(&mut cb),
        perp_commitment(&mut cs),
        "batch block commitment must equal the per-item commitment"
    );
}
