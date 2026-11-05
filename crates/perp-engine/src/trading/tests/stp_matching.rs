use super::*;
use crate::{
    events::stream_test_support::{assert_account_update_groups, stream_shape},
    interface::IPerpDex::{cancelOrderCall, OrderCancelled, OrderExpired, OrderPlaced, Trade},
    types::{CancelReason, ExpiryReason, MarketHot, Side, StpMode, UserFeeRates},
};

const SIDES: [Side; 2] = [Side::Buy, Side::Sell];
const MAKER: StpMode = StpMode::ExpireMaker;
const TAKER: StpMode = StpMode::ExpireTaker;
const BOTH: StpMode = StpMode::ExpireBoth;
const MODES: [StpMode; 3] = [MAKER, TAKER, BOTH];
const STP: u8 = CancelReason::SelfTradePrevention as u8;
const STP_EXPIRY: u8 = ExpiryReason::SelfTradePrevention as u8;
const TIF_EXPIRY: u8 = ExpiryReason::Tif as u8;
const LIMIT: u8 = 0;
const MARKET: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;
const FOK: u8 = 2;
const POST_ONLY: u8 = 3;

fn opp(side: Side) -> Side {
    match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

/// The `k`-th price level INTO the book a `taker` walks — asks ascending from `PRICE` for a
/// buy, bids descending from `PRICE` for a sell. `k = 0` is the touch.
fn lvl(taker: Side, k: u64) -> u64 {
    match taker {
        Side::Buy => PRICE + k * TICK,
        Side::Sell => PRICE - k * TICK,
    }
}

/// `k` ticks AWAY from the touch on the taker's own (passive) side — never crossed by it.
fn away(taker: Side, k: u64) -> u64 {
    lvl(opp(taker), k)
}

/// ALICE is the self-trader; BOB and CAROL are third parties. Deep pockets throughout: these
/// tests are about who meets whom, never a margin reject (except where a test says so).
fn fixture() -> TestCtx {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, ALICE, WALLET * 10);
    fund(&mut ctx, BOB, WALLET * 10);
    fund(&mut ctx, CAROL, WALLET * 10);
    ctx
}

/// A resting GTC on the side `taker` walks, `k` levels in.
fn maker(ctx: &mut TestCtx, who: Address, taker: Side, k: u64, qty: u64) -> [u8; 32] {
    place(ctx, who, opp(taker) as u8, lvl(taker, k), qty, LIMIT, GTC)
}

fn input(side: Side, price: u64, qty: u64, order_type: u8, tif: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side: side as u8,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

/// ALICE's taker through the full shell; it must be accepted. Returns its id and its logs
/// (which also satisfy the account-update grouping invariant).
fn take(
    ctx: &mut TestCtx,
    side: Side,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
    mode: StpMode,
) -> ([u8; 32], Vec<primitives::Log>) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let out = run_perp_dex_call(
        &input(side, price, qty, order_type, tif, mode.to_flags()),
        30_000_000,
        ALICE,
        U256::ZERO,
        false,
        ctx,
    )
    .unwrap();
    assert!(!out.reverted, "taker reverted: {:?}", String::from_utf8_lossy(&out.bytes));
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_account_update_groups(&logs);
    (out.bytes[..32].try_into().unwrap(), logs)
}

/// ALICE's taker through the full shell; it must be REFUSED write-clean and log-clean. Returns
/// the revert reason.
fn take_refused(
    ctx: &mut TestCtx,
    side: Side,
    price: u64,
    qty: u64,
    tif: u8,
    mode: StpMode,
) -> String {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let writes = JournalTr::perp_write_count(ctx.journal_mut());
    let out = run_perp_dex_call(
        &input(side, price, qty, LIMIT, tif, mode.to_flags()),
        30_000_000,
        ALICE,
        U256::ZERO,
        false,
        ctx,
    )
    .unwrap();
    assert!(out.reverted, "expected a refusal");
    assert_eq!(JournalTr::perp_write_count(ctx.journal_mut()), writes, "refused write-clean");
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty(), "refused log-clean");
    String::from_utf8_lossy(&out.bytes).to_string()
}

fn names(logs: &[primitives::Log]) -> Vec<&'static str> {
    stream_shape(logs).into_iter().map(|(n, _)| n).collect()
}

fn decoded<E: SolEvent>(logs: &[primitives::Log]) -> Vec<E> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|l| E::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .collect()
}

/// `(order id, user, reason)` of every `OrderCancelled`, in stream order.
fn cancels(logs: &[primitives::Log]) -> Vec<([u8; 32], Address, u8)> {
    decoded::<OrderCancelled>(logs)
        .into_iter()
        .map(|e| (e.orderId.0, e.user, e.reason))
        .collect()
}

/// `(filled, expired, reason)` of every `OrderExpired`.
fn expiries(logs: &[primitives::Log]) -> Vec<(u64, u64, u8)> {
    decoded::<OrderExpired>(logs)
        .into_iter()
        .map(|e| (e.filledQuantity, e.expiredQuantity, e.reason))
        .collect()
}

/// `(maker order id, quantity)` of every `Trade`, in stream order.
fn fills(logs: &[primitives::Log]) -> Vec<([u8; 32], u64)> {
    decoded::<Trade>(logs)
        .into_iter()
        .map(|e| (e.makerOrderId.0, e.quantity))
        .collect()
}

/// `(FIFO ids, live count)` of the level at `price` on the side `taker` walks.
fn level(ctx: &mut TestCtx, taker: Side, price: u64) -> (Vec<[u8; 32]>, u64) {
    let blob = match taker {
        Side::Buy => storage::load_ask_level_arc(ctx, MARKET_ID, price).unwrap(),
        Side::Sell => storage::load_bid_level_arc(ctx, MARKET_ID, price).unwrap(),
    };
    (blob.ids.clone(), blob.count)
}

/// The price index of the side `taker` walks (`true`) or of its own side (`false`).
fn prices(ctx: &mut TestCtx, taker: Side, walked: bool) -> Vec<u64> {
    let bids = (taker == Side::Sell) == walked;
    if bids {
        storage::load_bid_prices(ctx, MARKET_ID).unwrap()
    } else {
        storage::load_ask_prices(ctx, MARKET_ID).unwrap()
    }
}

/// The cached best quote on the side `taker` walks (`true`) or on its own side (`false`).
fn best(ctx: &mut TestCtx, taker: Side, walked: bool) -> u64 {
    let h = storage::load_market_hot(ctx, MARKET_ID).unwrap();
    if (taker == Side::Sell) == walked {
        h.best_bid
    } else {
        h.best_ask
    }
}

fn hot(ctx: &mut TestCtx) -> MarketHot {
    storage::load_market_hot(ctx, MARKET_ID).unwrap()
}

/// ALICE's resting quantity on the side `taker` walks.
fn own_walked_side_qty(ctx: &mut TestCtx, taker: Side) -> u64 {
    let p = pos(ctx, ALICE);
    match taker {
        Side::Buy => p.total_sell_qty,
        Side::Sell => p.total_buy_qty,
    }
}

fn signed(taker: Side, qty: u64) -> i64 {
    match taker {
        Side::Buy => qty as i64,
        Side::Sell => -(qty as i64),
    }
}

// ── T1 / T16 / T17: the default mode, zero fills ─────────────────────────────────────────

/// T1 — `flags = 0` (EXPIRE_MAKER) and the walk meets only ALICE's own resting PostOnly order,
/// which is LARGER than the taker. The resting order's own kind is not consulted.
///
/// * the maker is cancelled (reason 9) for its WHOLE quantity, not only the taker's 1 lot;
/// * the taker's `remaining` is untouched, so it rests in full;
/// * nothing trades, and — T16 — nobody gets an `AccountBalanceChanged`: the stream is exactly
///   `OrderPlaced · OrderCancelled · OrderRested`;
/// * the emptied level leaves the index and the best caches / mid sample are exactly what the
///   owner cancelling that order herself and then placing the same GTC produces.
#[test]
fn t1_default_mode_cancels_the_whole_own_maker_and_rests_the_taker() {
    for side in SIDES {
        let run = |via_stp: bool| {
            let mut ctx = fixture();
            // A third-party quote on the TAKER's side, so every best-quote change moves a mid.
            place(&mut ctx, BOB, side as u8, away(side, 5), QTY, LIMIT, GTC);
            let own_side = opp(side) as u8;
            let own = place(&mut ctx, ALICE, own_side, lvl(side, 0), 3 * QTY, LIMIT, POST_ONLY);
            ctx.block.timestamp = U256::from(10);
            if !via_stp {
                run_cancel_order(
                    &cancelOrderCall { orderId: own.into(), marketId: MARKET_ID }.abi_encode(),
                    ALICE,
                    &mut ctx,
                )
                .unwrap();
            }
            let (id, logs) = take(&mut ctx, side, lvl(side, 0), QTY, LIMIT, GTC, MAKER);
            (ctx, own, id, logs)
        };
        let (mut ctx, own, id, logs) = run(true);
        assert_eq!(
            names(&logs),
            vec!["OrderPlaced", "OrderCancelled", "OrderRested"],
            "{side:?}: no Trade, no account row"
        );
        assert_eq!(cancels(&logs), vec![(own, ALICE, STP)], "{side:?}");
        assert_terminal(&mut ctx, own);
        assert_eq!(own_walked_side_qty(&mut ctx, side), 0, "{side:?}: all 3 lots left");
        let t = get_order(&mut ctx, id);
        assert_eq!((t.status, t.filled, t.quantity), (OrderStatus::Open, 0, QTY), "{side:?}");
        assert!(prices(&mut ctx, side, true).is_empty(), "{side:?}: the level left the index");
        assert_eq!(best(&mut ctx, side, true), 0, "{side:?}");
        assert_eq!(best(&mut ctx, side, false), lvl(side, 0), "{side:?}: the taker is best");
        assert_eq!(pos(&mut ctx, ALICE).amount, 0);
        assert_eq!(hot(&mut ctx).last_traded, 0, "{side:?}: no print");

        // The reference: the owner's own cancel, then the same GTC — same id (a cancel does not
        // consume a nonce), same book, same caches, same mid samples.
        let (mut reference, _, ref_id, _) = run(false);
        assert_eq!(id, ref_id);
        assert_eq!(hot(&mut ctx), hot(&mut reference), "{side:?}: best caches");
        assert_eq!(
            storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap(),
            storage::load_price_basis_window(&mut reference, MARKET_ID).unwrap(),
            "{side:?}: the mid samples"
        );
        // The FIRST best-quote change at a timestamp is the one sampled, and here that is the
        // STP cancel's: it emptied the walked side and left only BOB's quote.
        let window = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
        assert_eq!((window.last_sample_ts, window.last_mid_price), (10, away(side, 5)));
        for walked in [true, false] {
            assert_eq!(prices(&mut ctx, side, walked), prices(&mut reference, side, walked));
        }
        assert_eq!(pos(&mut ctx, ALICE), pos(&mut reference, ALICE), "{side:?}");
        assert_eq!(get_order(&mut ctx, id), get_order(&mut reference, id));
    }
}

/// T17 — a match that was ONLY blocked prints nothing: no last traded price, no fee, and the
/// market's trade counter does not move (the next real trade is still trade 0).
#[test]
fn t17_a_blocked_only_match_moves_no_price_fee_or_trade_id() {
    for side in SIDES {
        for mode in MODES {
            let mut ctx = fixture();
            for who in [ALICE, BOB, CAROL] {
                storage::save_user_fee_rates(
                    &mut ctx,
                    who,
                    UserFeeRates { maker_fee_ppm: 1_000, taker_fee_ppm: 1_000 },
                )
                .unwrap();
            }
            maker(&mut ctx, ALICE, side, 0, QTY);
            let (_, logs) = take(&mut ctx, side, lvl(side, 0), QTY, LIMIT, IOC, mode);
            assert!(fills(&logs).is_empty(), "{side:?} {mode:?}");
            assert_eq!(hot(&mut ctx).last_traded, 0, "{side:?} {mode:?}");
            assert_eq!(market_fee_total(&mut ctx), 0, "{side:?} {mode:?}");

            // A real trade afterwards is the market's FIRST.
            place(&mut ctx, BOB, opp(side) as u8, away(side, 3), QTY, LIMIT, GTC);
            let _ = JournalTr::take_logs(ctx.journal_mut());
            place(&mut ctx, CAROL, side as u8, away(side, 3), QTY, LIMIT, GTC);
            let trades = decoded::<Trade>(&JournalTr::take_logs(ctx.journal_mut()));
            assert_eq!(trades.len(), 1);
            assert_eq!(trades[0].tradeId, 0, "{side:?} {mode:?}: no trade id was consumed");
        }
    }
}

// ── T2 / T3: FIFO and depth under EXPIRE_MAKER ───────────────────────────────────────────

/// T2 — per resting ORDER, in FIFO: a third party queued AHEAD of ALICE's own order at the same
/// price fills first; then her order is cancelled; then the walk goes on to the third party
/// queued behind it.
#[test]
fn t2_others_ahead_fill_first_then_the_own_maker_is_cancelled_and_the_walk_goes_on() {
    for side in SIDES {
        let mut ctx = fixture();
        let bob = maker(&mut ctx, BOB, side, 0, QTY);
        let own = maker(&mut ctx, ALICE, side, 0, QTY);
        let carol = maker(&mut ctx, CAROL, side, 0, QTY);
        let (id, logs) = take(&mut ctx, side, lvl(side, 0), 2 * QTY, LIMIT, GTC, MAKER);
        let order: Vec<&str> = names(&logs)
            .into_iter()
            .filter(|n| matches!(*n, "Trade" | "OrderCancelled"))
            .collect();
        assert_eq!(order, vec!["Trade", "OrderCancelled", "Trade"], "{side:?}");
        assert_eq!(fills(&logs), vec![(bob, QTY), (carol, QTY)], "{side:?}");
        assert_eq!(cancels(&logs), vec![(own, ALICE, STP)], "{side:?}");
        assert_terminal(&mut ctx, id);
        assert!(prices(&mut ctx, side, true).is_empty(), "{side:?}");
        assert_eq!(pos(&mut ctx, ALICE).amount, signed(side, 2 * QTY));
    }
}

/// T3 — EXPIRE_MAKER cancels ALICE's orders across several levels WITHOUT consuming the taker,
/// fills the third parties among and beyond them, and rests what is left.
#[test]
fn t3_expire_maker_cancels_across_levels_without_consuming_the_taker() {
    for side in SIDES {
        let mut ctx = fixture();
        let a = maker(&mut ctx, ALICE, side, 0, QTY);
        let b = maker(&mut ctx, ALICE, side, 1, 2 * QTY);
        let carol = maker(&mut ctx, CAROL, side, 1, QTY);
        let c = maker(&mut ctx, ALICE, side, 1, QTY);
        let d = maker(&mut ctx, ALICE, side, 2, QTY);
        let bob = maker(&mut ctx, BOB, side, 3, QTY);
        let (id, logs) = take(&mut ctx, side, lvl(side, 3), 3 * QTY, LIMIT, GTC, MAKER);

        assert_eq!(
            cancels(&logs),
            vec![(a, ALICE, STP), (b, ALICE, STP), (c, ALICE, STP), (d, ALICE, STP)],
            "{side:?}: price order, then FIFO"
        );
        assert_eq!(fills(&logs), vec![(carol, QTY), (bob, QTY)], "{side:?}");
        // 3 lots in, 2 filled, 1 rests: the four cancels took nothing from the taker.
        let t = get_order(&mut ctx, id);
        assert_eq!(
            (t.status, t.filled, t.quantity),
            (OrderStatus::PartiallyFilled, 2 * QTY, 3 * QTY),
            "{side:?}"
        );
        assert!(prices(&mut ctx, side, true).is_empty(), "{side:?}: every level emptied");
        for k in 0..=3 {
            assert_eq!(level(&mut ctx, side, lvl(side, k)), (vec![], 0), "{side:?} level {k}");
        }
        assert_eq!(prices(&mut ctx, side, false), vec![lvl(side, 3)], "{side:?}: rests");
        assert_eq!(own_walked_side_qty(&mut ctx, side), 0);
        assert_eq!(pos(&mut ctx, ALICE).amount, signed(side, 2 * QTY));
    }
}

// ── T4–T7: stopping the walk ─────────────────────────────────────────────────────────────

/// T4 — EXPIRE_TAKER on a GTC: a third party at L0 fills, ALICE's own order at L1 stops the
/// walk. The fills stand; the remainder EXPIRES with reason 9 and never rests (no
/// `OrderRested`, no record); ALICE's maker stays exactly where it was in its FIFO, and L1's
/// live count is unchanged.
#[test]
fn t4_expire_taker_keeps_the_fills_expires_the_rest_and_leaves_the_own_maker_in_place() {
    for side in SIDES {
        let mut ctx = fixture();
        let bob = maker(&mut ctx, BOB, side, 0, QTY);
        let own = maker(&mut ctx, ALICE, side, 1, QTY);
        let carol = maker(&mut ctx, CAROL, side, 1, QTY);
        let (id, logs) = take(&mut ctx, side, lvl(side, 1), 3 * QTY, LIMIT, GTC, TAKER);

        assert_eq!(fills(&logs), vec![(bob, QTY)], "{side:?}");
        assert!(cancels(&logs).is_empty(), "{side:?}: EXPIRE_TAKER cancels nothing");
        assert_eq!(expiries(&logs), vec![(QTY, 2 * QTY, STP_EXPIRY)], "{side:?}");
        assert!(!names(&logs).contains(&"OrderRested"), "{side:?}: never rests");
        assert_eq!(names(&logs).last().copied(), Some("OrderExpired"));
        assert!(storage::load_order(&mut ctx, &id).unwrap().is_none(), "{side:?}");
        assert!(prices(&mut ctx, side, false).is_empty(), "{side:?}: nothing on its own side");
        assert_eq!(level(&mut ctx, side, lvl(side, 1)), (vec![own, carol], 2), "{side:?}");
        assert_eq!(get_order(&mut ctx, own).filled, 0);
        assert_eq!(get_order(&mut ctx, carol).filled, 0);
        assert_eq!(pos(&mut ctx, ALICE).amount, signed(side, QTY));
    }
}

/// T5 — EXPIRE_TAKER where ALICE's own order is the LAST id of its level and a third party
/// waits one level deeper: the walk must stop dead. A stop that only ended the level (an inner
/// `break`) would let the next level fill. The "flag checked at the top of the `while`" form
/// fills nothing but rewrites the next level; `stp_spec_tests::t5b_*` pins that one.
#[test]
fn t5_expire_taker_stops_dead_when_the_own_maker_is_the_levels_last_id() {
    for side in SIDES {
        let mut ctx = fixture();
        let bob = maker(&mut ctx, BOB, side, 0, QTY);
        let own = maker(&mut ctx, ALICE, side, 0, QTY);
        let carol = maker(&mut ctx, CAROL, side, 1, QTY);
        let (_, logs) = take(&mut ctx, side, lvl(side, 1), 3 * QTY, LIMIT, GTC, TAKER);

        assert_eq!(fills(&logs), vec![(bob, QTY)], "{side:?}: nothing behind the stop");
        assert_eq!(expiries(&logs), vec![(QTY, 2 * QTY, STP_EXPIRY)], "{side:?}");
        assert_eq!(get_order(&mut ctx, carol).filled, 0, "{side:?}");
        assert_eq!(level(&mut ctx, side, lvl(side, 0)), (vec![own], 1), "{side:?}");
        assert_eq!(level(&mut ctx, side, lvl(side, 1)), (vec![carol], 1), "{side:?}");
        assert_eq!(best(&mut ctx, side, true), lvl(side, 0), "{side:?}");
    }
}

/// T6 — EXPIRE_BOTH on ALICE's order when it is the level's last LIVE order: it is cancelled,
/// the price leaves the index, the best quote moves to the next level, and the taker's whole
/// quantity expires with reason 9. Nothing traded, so no account row.
#[test]
fn t6_expire_both_on_the_levels_last_live_order_removes_the_price() {
    for side in SIDES {
        let mut ctx = fixture();
        let own = maker(&mut ctx, ALICE, side, 0, QTY);
        let bob = maker(&mut ctx, BOB, side, 1, QTY);
        let (_, logs) = take(&mut ctx, side, lvl(side, 1), QTY, LIMIT, GTC, BOTH);

        assert_eq!(names(&logs), vec!["OrderPlaced", "OrderCancelled", "OrderExpired"]);
        assert_eq!(cancels(&logs), vec![(own, ALICE, STP)], "{side:?}");
        assert_eq!(expiries(&logs), vec![(0, QTY, STP_EXPIRY)], "{side:?}");
        assert_terminal(&mut ctx, own);
        assert_eq!(prices(&mut ctx, side, true), vec![lvl(side, 1)], "{side:?}");
        assert_eq!(best(&mut ctx, side, true), lvl(side, 1), "{side:?}: best refreshed");
        assert_eq!(get_order(&mut ctx, bob).filled, 0, "{side:?}: the walk stopped");
    }
}

/// T7 — EXPIRE_BOTH on ALICE's order with others queued BEHIND it: the level's live count drops
/// by exactly one and their FIFO order is kept; the price stays best.
#[test]
fn t7_expire_both_with_others_behind_decrements_the_count_and_keeps_their_order() {
    for side in SIDES {
        let mut ctx = fixture();
        let own = maker(&mut ctx, ALICE, side, 0, QTY);
        let bob = maker(&mut ctx, BOB, side, 0, QTY);
        let carol = maker(&mut ctx, CAROL, side, 0, QTY);
        let (_, logs) = take(&mut ctx, side, lvl(side, 0), QTY, LIMIT, GTC, BOTH);

        assert_eq!(cancels(&logs), vec![(own, ALICE, STP)], "{side:?}");
        assert_eq!(expiries(&logs), vec![(0, QTY, STP_EXPIRY)], "{side:?}");
        assert_eq!(level(&mut ctx, side, lvl(side, 0)), (vec![bob, carol], 2), "{side:?}");
        assert_eq!(best(&mut ctx, side, true), lvl(side, 0), "{side:?}");
    }
}

// ── T8 / T9: IOC and market ──────────────────────────────────────────────────────────────

/// T8 — IOC and market under EXPIRE_MAKER: the own order is cancelled, the walk goes on and
/// then runs out of book. STP did not stop it, so the remainder is a plain TIF expiry (reason
/// 0) — Binance's market order that empties the book after an STP cancel is `EXPIRED`.
#[test]
fn t8_ioc_and_market_under_expire_maker_run_out_of_book_with_reason_0() {
    for side in SIDES {
        for (order_type, tif, price) in [(LIMIT, IOC, lvl(side, 1)), (MARKET, GTC, 0)] {
            let mut ctx = fixture();
            let own = maker(&mut ctx, ALICE, side, 0, QTY);
            let bob = maker(&mut ctx, BOB, side, 1, QTY);
            let (_, logs) = take(&mut ctx, side, price, 2 * QTY, order_type, tif, MAKER);
            let case = format!("{side:?} type {order_type}");
            assert_eq!(cancels(&logs), vec![(own, ALICE, STP)], "{case}");
            assert_eq!(fills(&logs), vec![(bob, QTY)], "{case}");
            assert_eq!(expiries(&logs), vec![(QTY, QTY, TIF_EXPIRY)], "{case}");
        }
    }
}

/// T9 — IOC and market under EXPIRE_TAKER / EXPIRE_BOTH, ALICE's order at the touch: the
/// remainder expires with reason 9. With zero fills the stream is the bare placement and its
/// expiry (plus the maker's cancel under EXPIRE_BOTH) — no account row for anyone (T16).
#[test]
fn t9_ioc_and_market_under_expire_taker_or_both_expire_with_reason_9() {
    for side in SIDES {
        for (order_type, tif, price) in [(LIMIT, IOC, lvl(side, 1)), (MARKET, GTC, 0)] {
            for mode in [TAKER, BOTH] {
                let mut ctx = fixture();
                let own = maker(&mut ctx, ALICE, side, 0, QTY);
                let bob = maker(&mut ctx, BOB, side, 1, QTY);
                let (_, logs) = take(&mut ctx, side, price, QTY, order_type, tif, mode);
                let case = format!("{side:?} type {order_type} {mode:?}");
                assert_eq!(expiries(&logs), vec![(0, QTY, STP_EXPIRY)], "{case}");
                assert_eq!(get_order(&mut ctx, bob).filled, 0, "{case}");
                if mode == TAKER {
                    assert_eq!(names(&logs), vec!["OrderPlaced", "OrderExpired"], "{case}");
                    assert_eq!(get_order(&mut ctx, own).status, OrderStatus::Open, "{case}");
                } else {
                    assert_eq!(
                        names(&logs),
                        vec!["OrderPlaced", "OrderCancelled", "OrderExpired"],
                        "{case}"
                    );
                    assert_eq!(cancels(&logs), vec![(own, ALICE, STP)], "{case}");
                    assert_terminal(&mut ctx, own);
                }
            }
        }
    }
}

// ── T10 / T11: the kinds STP does not apply to ───────────────────────────────────────────

/// T10 — a FOK taker SELF-FILLS in every mode: STP does not apply to FOK (Binance: "STP won't
/// take effect"). This pins OUR behaviour — Binance documents nothing about what a FOK does
/// against its owner's own liquidity — and `check_fok_feasibility` counting that liquidity is
/// consistent with it.
#[test]
fn t10_a_fok_taker_still_self_fills_in_every_mode() {
    for side in SIDES {
        for mode in MODES {
            let mut ctx = fixture();
            let own = maker(&mut ctx, ALICE, side, 0, QTY);
            let (id, logs) = take(&mut ctx, side, lvl(side, 0), QTY, LIMIT, FOK, mode);
            let trades = decoded::<Trade>(&logs);
            assert_eq!(trades.len(), 1, "{side:?} {mode:?}");
            assert_eq!((trades[0].maker, trades[0].taker), (ALICE, ALICE));
            assert_eq!(trades[0].makerOrderId.0, own);
            assert!(cancels(&logs).is_empty() && expiries(&logs).is_empty());
            assert_terminal(&mut ctx, own);
            assert_terminal(&mut ctx, id);
            assert_eq!(pos(&mut ctx, ALICE).amount, 0, "{side:?} {mode:?}: round trip");
        }
    }
}

/// T11 — a PostOnly taker that would cross its OWN order is refused like any other cross, in
/// every mode (Binance -5022). The resting order is untouched.
#[test]
fn t11_a_post_only_taker_crossing_its_own_order_is_refused() {
    for side in SIDES {
        for mode in MODES {
            let mut ctx = fixture();
            let own = maker(&mut ctx, ALICE, side, 0, QTY);
            let reason = take_refused(&mut ctx, side, lvl(side, 0), QTY, POST_ONLY, mode);
            assert!(reason.contains("PostOnly order would match"), "{mode:?} {reason}");
            assert_eq!(get_order(&mut ctx, own).status, OrderStatus::Open);
        }
    }
}

// ── T12: the reserved mode ───────────────────────────────────────────────────────────────

/// T12 — the reserved mode `0b11` is refused BEFORE any write and before any log, on both
/// direct selector families (the signed and batch forms are pinned in `signed_replay` and
/// `stp_encoding`).
#[test]
fn t12_the_reserved_mode_is_refused_write_clean() {
    use crate::{interface::IPerpDex::placeAlgoOrderCall, types::AlgoOrderType};
    let mut ctx = fixture();
    let algo = placeAlgoOrderCall {
        algoType: 0,
        marketId: MARKET_ID,
        side: 1,
        orderType: AlgoOrderType::StopMarket as u8,
        triggerPrice: PRICE - TICK,
        price: 0,
        quantity: QTY,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags: 1 | StpMode::FLAG_MASK,
    }
    .abi_encode();
    for (call, sel) in [
        (input(Side::Buy, PRICE, QTY, LIMIT, GTC, StpMode::FLAG_MASK), "placeOrder"),
        (algo, "placeAlgoOrder"),
    ] {
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let writes = JournalTr::perp_write_count(ctx.journal_mut());
        let out = run_perp_dex_call(&call, 30_000_000, ALICE, U256::ZERO, false, &mut ctx)
            .unwrap();
        assert!(out.reverted, "{sel}");
        let reason = String::from_utf8_lossy(&out.bytes).to_string();
        assert!(
            reason.contains(&format!("{sel}: invalid self-trade prevention mode")),
            "{reason}"
        );
        assert_eq!(JournalTr::perp_write_count(ctx.journal_mut()), writes, "{sel}");
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty(), "{sel}");
    }
}

// ── T18: a refused rest discards the STP cancels with everything else ────────────────────

/// T18 — EXPIRE_MAKER cancels ALICE's crossing order and fills BOB, but the rest of the GTC is
/// unaffordable: the WHOLE order is refused — and the STP cancel with it. Write-clean,
/// log-clean: her resting order, BOB's and her same-side order are all exactly as before. (The
/// FOK form of this fixture is
/// `self_trade_taker_margin_expiry_does_not_cancel_current_taker_order`.)
#[test]
fn t18_an_unaffordable_rest_after_stp_cancels_refuses_the_whole_order() {
    for side in SIDES {
        for affordable in [false, true] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            let old = place(&mut ctx, ALICE, side as u8, away(side, 2), QTY, LIMIT, GTC);
            let own = maker(&mut ctx, ALICE, side, 0, QTY);
            let bob = maker(&mut ctx, BOB, side, 0, QTY);
            let spare = if affordable { 10 * FILL_VALUE as i64 } else { 20_000 };
            set_available(&mut ctx, ALICE, spare);
            let before = pos(&mut ctx, ALICE);
            if affordable {
                // Control: the same order with the money for its rest goes through.
                let (_, logs) = take(&mut ctx, side, lvl(side, 0), 2 * QTY, LIMIT, GTC, MAKER);
                assert_eq!(cancels(&logs), vec![(own, ALICE, STP)], "{side:?}");
                continue;
            }
            let reason = take_refused(&mut ctx, side, lvl(side, 0), 2 * QTY, GTC, MAKER);
            assert!(reason.contains("insufficient perp wallet for margin"), "{reason}");
            for id in [old, own, bob] {
                let o = get_order(&mut ctx, id);
                assert_eq!((o.status, o.filled), (OrderStatus::Open, 0), "{side:?}");
            }
            assert_eq!(level(&mut ctx, side, lvl(side, 0)), (vec![own, bob], 2), "{side:?}");
            assert_eq!(pos(&mut ctx, ALICE), before, "{side:?}");
        }
    }
}

// ── T21 / T22: interactions ──────────────────────────────────────────────────────────────

/// Seeds ALICE with an open position of `amount` (signed) at `PRICE`.
fn seed_alice(ctx: &mut TestCtx, amount: i64) {
    storage::save_position(
        ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance: -amount / QTY as i64 * FILL_VALUE as i64,
            margin: (amount.unsigned_abs() / QTY * FILL_VALUE) as i64,
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

/// T21 — reduce-only and STP are independent.
///
/// (a) A reduce-only TAKER: admission truncates it on the taker's side only, so cancelling
///     ALICE's opposite-side order does not change the truncated size; the walk cancels it
///     (reason 9) and closes against BOB.
/// (b) A reduce-only MAKER cancelled by STP reports reason 9 — a plain protocol cancel, not a
///     reduce-only eviction (5/6/7).
#[test]
fn t21_reduce_only_admission_is_unchanged_and_a_cancelled_reduce_only_maker_reports_9() {
    for side in SIDES {
        // (a) ALICE holds 1 lot the taker side closes; it asks to close 2.
        let mut ctx = fixture();
        seed_alice(&mut ctx, -signed(side, QTY));
        let own = maker(&mut ctx, ALICE, side, 0, QTY);
        let bob = maker(&mut ctx, BOB, side, 1, 2 * QTY);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let out = run_perp_dex_call(
            &input(side, lvl(side, 1), 2 * QTY, LIMIT, IOC, 1),
            30_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(!out.reverted, "{:?}", String::from_utf8_lossy(&out.bytes));
        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_eq!(decoded::<OrderPlaced>(&logs)[0].quantity, QTY, "{side:?}: truncated to 1");
        assert_eq!(cancels(&logs), vec![(own, ALICE, STP)], "{side:?}");
        assert_eq!(fills(&logs), vec![(bob, QTY)], "{side:?}");
        assert_eq!(pos(&mut ctx, ALICE).amount, 0, "{side:?}: closed, not flipped");

        // (b) ALICE holds 1 lot her resting reduce-only order closes; her own GTC crosses it.
        let mut ctx = fixture();
        seed_alice(&mut ctx, signed(side, QTY));
        let ro_input = input(opp(side), lvl(side, 0), QTY, LIMIT, GTC, 1);
        let ro: [u8; 32] = run_place_order(&ro_input, ALICE, &mut ctx).unwrap()[..32]
            .try_into()
            .unwrap();
        assert!(get_order(&mut ctx, ro).reduce_only);
        let (id, logs) = take(&mut ctx, side, lvl(side, 0), QTY, LIMIT, GTC, MAKER);
        assert_eq!(cancels(&logs), vec![(ro, ALICE, STP)], "{side:?}: 9, and nothing else");
        assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open, "{side:?}: rests");
    }
}

/// T22 — ALICE's own order on a level OUTSIDE the fill-time band is skipped before it is even
/// loaded, like any out-of-band level: no STP cancel, and the in-band third party behind it
/// fills.
#[test]
fn t22_an_own_order_on_an_out_of_band_level_is_never_met() {
    for side in SIDES {
        let mut ctx = make_ctx();
        setup_banded(&mut ctx, 100);
        // Rest ALICE's order while the mark makes it in-band, then move the mark away from it.
        let stranded = away(side, 5);
        set_mark(&mut ctx, stranded);
        let own = place(&mut ctx, ALICE, opp(side) as u8, stranded, QTY, LIMIT, GTC);
        set_mark(&mut ctx, PRICE);
        let bob = maker(&mut ctx, BOB, side, 0, QTY);
        let (_, logs) = take(&mut ctx, side, lvl(side, 0), QTY, LIMIT, GTC, MAKER);
        assert!(cancels(&logs).is_empty(), "{side:?}");
        assert_eq!(fills(&logs), vec![(bob, QTY)], "{side:?}");
        assert_eq!(get_order(&mut ctx, own).status, OrderStatus::Open, "{side:?}");
    }
}

// ── T23 / T23b: the pre-walk affordability filter ────────────────────────────────────────

/// T23 — under EXPIRE_MAKER the pre-walk filter does not over-reject. ALICE rests 3 lots on the
/// walked side and holds exactly the 2 lots of margin her 2-lot GTC will need once STP has
/// cancelled them — her available balance is NEGATIVE before the order. The filter values the
/// order against her book as it stands (`max(Bid, Ask)` does not grow), so it lets it through,
/// and the result is the full path's: accepted at exactly the threshold, refused one unit
/// below it (by the rest, write-clean).
#[test]
fn t23_the_prefilter_does_not_kill_an_order_only_stp_makes_affordable() {
    for side in SIDES {
        for spare in [0i64, -1] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            set_mark(&mut ctx, PRICE);
            let own = maker(&mut ctx, ALICE, side, 0, 3 * QTY);
            // wallet = 2 lots (+ spare): available = wallet − 3 lots of resting requirement.
            set_available(&mut ctx, ALICE, -(FILL_VALUE as i64) + spare);
            if spare == 0 {
                let (id, logs) = take(&mut ctx, side, lvl(side, 0), 2 * QTY, LIMIT, GTC, MAKER);
                assert_eq!(cancels(&logs), vec![(own, ALICE, STP)], "{side:?}");
                assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open, "{side:?}");
                assert_eq!(available(&mut ctx, ALICE), 0, "{side:?}: exactly the threshold");
            } else {
                let reason = take_refused(&mut ctx, side, lvl(side, 0), 2 * QTY, GTC, MAKER);
                assert!(reason.contains("insufficient perp wallet for margin"), "{reason}");
                assert_eq!(get_order(&mut ctx, own).status, OrderStatus::Open, "{side:?}");
            }
        }
    }
}

/// T23b — under EXPIRE_TAKER the pre-walk filter is conservative ON PURPOSE. ALICE's own order
/// sits at the touch, so the 3-lot GTC would stop at once and expire (needing no margin) — but
/// the filter assumes every unit fills or rests, and her wallet does not cover resting 3 lots.
/// The order is refused cleanly, as Binance admits an order against its full size, rather than
/// expired. (Skipping the filter for this case would reopen the deep-walk DoS it closes.) With
/// the money for the rest, the same order is accepted and expires with reason 9.
#[test]
fn t23b_expire_taker_is_admitted_against_its_full_size() {
    for side in SIDES {
        for funded in [false, true] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            set_mark(&mut ctx, PRICE);
            let own = maker(&mut ctx, ALICE, side, 0, QTY);
            set_available(&mut ctx, ALICE, if funded { 3 * FILL_VALUE as i64 } else { 0 });
            if funded {
                let (_, logs) = take(&mut ctx, side, lvl(side, 0), 3 * QTY, LIMIT, GTC, TAKER);
                assert_eq!(expiries(&logs), vec![(0, 3 * QTY, STP_EXPIRY)], "{side:?}");
            } else {
                let reason = take_refused(&mut ctx, side, lvl(side, 0), 3 * QTY, GTC, TAKER);
                assert!(reason.contains("insufficient perp wallet for margin"), "{reason}");
            }
            assert_eq!(get_order(&mut ctx, own).status, OrderStatus::Open, "{side:?}");
        }
    }
}
