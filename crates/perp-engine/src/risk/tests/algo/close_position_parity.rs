use super::*;
use crate::interface::IPerpDex::{
    AlgoOrderCancelled, AlgoOrderTriggered, OrderPlaced, placeAlgoOrderCall,
};
use crate::types::{AlgoOrderType, CancelReason};

const RO: u8 = 1;
const CP: u8 = 2;

#[allow(clippy::too_many_arguments)]
fn try_arm(
    ctx: &mut TestCtx,
    side: Side,
    kind: AlgoOrderType,
    trigger: u64,
    price: u64,
    qty: u64,
    flags: u8,
) -> Result<[u8; 32], PerpError> {
    crate::trading::algo::run_place_algo_order(
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: side as u8,
            orderType: kind as u8,
            triggerPrice: trigger,
            price,
            quantity: qty,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags,
        }
        .abi_encode(),
        ALICE,
        ctx,
    )
    .map(|out| out[..32].try_into().unwrap())
}

/// A closePosition STOP_MARKET on `side`.
fn try_close_position_stop(ctx: &mut TestCtx, side: Side, trigger: u64) -> Result<[u8; 32], PerpError> {
    try_arm(ctx, side, AlgoOrderType::StopMarket, trigger, 0, 0, CP)
}

fn long_10(ctx: &mut TestCtx) {
    seed_position_account(ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
}

/// Sets ALICE's `amount` (and the matching entry value and margin) through `save_position`, the
/// same door every fill comes through — `amount = 0` writes a flat position the way a close
/// leaves it.
fn set_amount(ctx: &mut TestCtx, amount: i64) {
    let mut pos = storage::load_position(ctx, ALICE, MARKET_ID).unwrap();
    pos.amount = amount;
    pos.v_quote_balance = -amount * (ENTRY_VALUE / QTY);
    pos.margin = if amount == 0 { 0 } else { MARGIN };
    storage::save_position(ctx, ALICE, MARKET_ID, &pos, AccountUpdateReason::Adjustment)
        .unwrap();
}

fn generation(ctx: &mut TestCtx) -> u64 {
    storage::load_position(ctx, ALICE, MARKET_ID).unwrap().generation
}

fn market_order(ctx: &mut TestCtx, user: Address, side: Side, qty: u64) {
    run_place_order(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: side as u8,
            price: 0,
            quantity: qty,
            orderType: 1,
            tif: 0,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        }
        .abi_encode(),
        user,
        ctx,
    )
    .unwrap();
}

fn err_text(e: PerpError) -> String {
    format!("{e:?}")
}

// ── flags ──────────────────────────────────────────────────────────────────────────────────

/// Binance: reduceOnly "cannot be sent with closePosition=true". Each bit alone is accepted.
#[test]
fn reduce_only_and_close_position_together_are_refused() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    long_10(&mut ctx);
    let e = try_arm(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 9_000, 0, 0, RO | CP)
        .unwrap_err();
    assert!(
        err_text(e).contains("placeAlgoOrder: reduceOnly cannot be sent with closePosition"),
        "refused by name"
    );
    assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());

    try_arm(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 9_000, 0, 0, CP).unwrap();
    try_arm(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 9_000, 0, QTY as u64, RO).unwrap();
}

// ── "would immediately trigger" ────────────────────────────────────────────────────────────

/// Every (type, side) pair, both the `_MARKET` and the limit kind, against the stored $100.00
/// mark: AT the mark is refused (the sweep's predicate is inclusive, so equality WOULD fire),
/// one tick past it is refused, one tick short of it is accepted. Nothing is written for a
/// refusal — not even the order nonce.
#[test]
fn an_order_that_would_fire_on_the_stored_mark_is_refused() {
    // (kind, side, fires upward)
    for (kind, side, up) in [
        (AlgoOrderType::StopMarket, Side::Buy, true),
        (AlgoOrderType::StopMarket, Side::Sell, false),
        (AlgoOrderType::TakeProfitMarket, Side::Buy, false),
        (AlgoOrderType::TakeProfitMarket, Side::Sell, true),
        (AlgoOrderType::Stop, Side::Buy, true),
        (AlgoOrderType::Stop, Side::Sell, false),
        (AlgoOrderType::TakeProfit, Side::Buy, false),
        (AlgoOrderType::TakeProfit, Side::Sell, true),
    ] {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let mark = ENTRY_PRICE;
        let price = if kind.is_market() { 0 } else { mark };
        // An UP trigger the mark has already passed sits BELOW it; a down one above it.
        let (passed, short_of) = if up { (mark - 1, mark + 1) } else { (mark + 1, mark - 1) };
        for trigger in [mark, passed] {
            let nonce = storage::load_user_nonce(&mut ctx, ALICE).unwrap();
            let writes = JournalTr::perp_write_count(ctx.journal_mut());
            let e = try_arm(&mut ctx, side, kind, trigger, price, QTY as u64, RO).unwrap_err();
            assert!(
                err_text(e).contains("placeAlgoOrder: order would immediately trigger"),
                "{kind:?} {side:?} at trigger {trigger}"
            );
            assert_eq!(JournalTr::perp_write_count(ctx.journal_mut()), writes, "write-clean");
            assert_eq!(storage::load_user_nonce(&mut ctx, ALICE).unwrap(), nonce);
        }
        try_arm(&mut ctx, side, kind, short_of, price, QTY as u64, RO)
            .unwrap_or_else(|e| panic!("{kind:?} {side:?} at {short_of}: {e:?}"));
    }
}

/// The refusal reads the mark the SWEEP will see, not the one the market was created with: move
/// the mark with an oracle tick and a trigger that was fine before is refused after.
#[test]
fn the_refusal_tracks_the_current_mark() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    try_arm(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 9_950, 0, QTY as u64, RO).unwrap();
    oracle_tick(&mut ctx, 9_900, 31);
    let e = try_arm(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 9_950, 0, QTY as u64, RO)
        .unwrap_err();
    assert!(err_text(e).contains("would immediately trigger"));
}

/// With NO stored mark there is nothing to compare against, so the check is skipped — the
/// convention the price band uses for the same case. Unreachable through the ABI (`addMarket`
/// refuses `initialMarkPrice == 0`), which is why only a hand-built market can get here.
#[test]
fn with_no_stored_mark_there_is_no_reference_and_no_refusal() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_mark_price(&mut ctx, MARKET_ID, 0).unwrap();
    // Both directions, at a trigger either of which a non-zero mark could have refused.
    try_arm(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 1, 0, QTY as u64, RO).unwrap();
    try_arm(&mut ctx, Side::Buy, AlgoOrderType::StopMarket, 1, 0, QTY as u64, RO).unwrap();
}

// ── position binding ───────────────────────────────────────────────────────────────────────

/// closePosition needs an open position, and its side must be that position's CLOSING side:
/// flat is refused, the opening side is refused, the closing side is accepted — long and short.
#[test]
fn close_position_requires_an_open_position_on_the_closing_side() {
    let refused = |ctx: &mut TestCtx, side: Side, trigger: u64| {
        let writes = JournalTr::perp_write_count(ctx.journal_mut());
        let e = try_close_position_stop(ctx, side, trigger).unwrap_err();
        assert!(
            err_text(e).contains(
                "placeAlgoOrder: closePosition requires an open position on the closing side"
            ),
            "{side:?}"
        );
        assert_eq!(JournalTr::perp_write_count(ctx.journal_mut()), writes, "write-clean");
    };

    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // Flat: neither side.
    refused(&mut ctx, Side::Sell, 9_000);
    refused(&mut ctx, Side::Buy, 11_000);

    // Long: SELL closes, BUY would open.
    long_10(&mut ctx);
    refused(&mut ctx, Side::Buy, 11_000);
    let id = try_close_position_stop(&mut ctx, Side::Sell, 9_000).unwrap();
    let o = storage::load_order(&mut ctx, &id).unwrap().unwrap();
    assert!(o.is_position_bound() && o.close_position && !o.reduce_only);
    assert_eq!(o.position_generation, generation(&mut ctx), "bound to the life open now");

    // Short: the mirror.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, -QTY, ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    refused(&mut ctx, Side::Sell, 9_000);
    try_close_position_stop(&mut ctx, Side::Buy, 11_000).unwrap();
}

/// A plain reduce-only conditional is NOT bound — it needs no position at placement (unchanged)
/// and stays live across a close and a reopen, where it reduces whatever is open. Binding is
/// the closePosition (`GTE_GTC`) behaviour only.
#[test]
fn a_quantity_reduce_only_conditional_is_not_bound() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let id = try_arm(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 9_900, 0, 4, RO).unwrap();
    assert!(!storage::load_order(&mut ctx, &id).unwrap().unwrap().is_position_bound());
    long_10(&mut ctx);
    set_amount(&mut ctx, 0);
    set_amount(&mut ctx, QTY);
    place_order(&mut ctx, MAKER, Side::Buy as u8, 9_850, QTY as u64);

    oracle_tick(&mut ctx, 9_900, 31);

    assert_eq!(position(&mut ctx, ALICE).amount, QTY - 4, "it reduced the position open now");
}

/// `PerpPosition::generation` moves exactly when a life ENDS — `amount` to zero, or across it —
/// and only `save_position` sets it: a stale or forged value in the caller's struct is ignored.
#[test]
fn the_generation_counts_closes_and_flips_and_nothing_else() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    assert_eq!(generation(&mut ctx), 0, "never opened");
    for (amount, want, why) in [
        (QTY, 0, "open"),
        (4, 0, "shrink"),
        (12, 0, "grow"),
        (0, 1, "close"),
        (0, 1, "flat to flat"),
        (-3, 1, "reopen the other way"),
        (5, 2, "flip"),
        (-5, 3, "flip back"),
        (0, 4, "close"),
    ] {
        set_amount(&mut ctx, amount);
        assert_eq!(generation(&mut ctx), want, "{why} -> {amount}");
    }

    let mut pos = storage::load_position(&mut ctx, ALICE, MARKET_ID).unwrap();
    pos.generation = 0;
    storage::save_position(&mut ctx, ALICE, MARKET_ID, &pos, AccountUpdateReason::Adjustment)
        .unwrap();
    assert_eq!(generation(&mut ctx), 4, "owned by save_position, not by the caller's copy");
}

/// Everything one oracle tick says about a conditional order.
struct Tick {
    triggered: bool,
    /// `OrderPlaced.quantity` of the order it became, if any.
    placed: Option<u64>,
    cancelled: Option<u8>,
}

fn tick(ctx: &mut TestCtx, index: u64, id: [u8; 32]) -> Tick {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    oracle_tick(ctx, index, 31);
    let logs = JournalTr::take_logs(ctx.journal_mut());
    Tick {
        triggered: decode_events::<AlgoOrderTriggered>(&logs).iter().any(|e| e.algoId.0 == id),
        placed: decode_events::<OrderPlaced>(&logs)
            .into_iter()
            .find(|e| e.orderId.0 == id)
            .map(|e| e.quantity),
        cancelled: decode_events::<AlgoOrderCancelled>(&logs)
            .into_iter()
            .find(|e| e.algoId.0 == id)
            .map(|e| e.reason),
    }
}

/// THE case binding exists for, through REAL trades: the closePosition stop was placed for one
/// long; ALICE closes it and opens a NEW long the same size. Unbound, the old stop would now
/// close the new position at a level chosen for the old one. Bound, it fires, finds its
/// position gone and is spent with `PositionClosed` — the new long is untouched.
#[test]
fn a_close_position_stop_expires_once_its_position_closed_even_if_another_opened() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // 3x (the default tier's cap, so the reopening fill is admitted) and a wallet that can fund
    // the new long's margin.
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 3, 1_000_000_000);
    let id = try_close_position_stop(&mut ctx, Side::Sell, 9_900).unwrap();

    // Close the bound long by trading out of it, then open a new long the same way.
    place_order(&mut ctx, MAKER, Side::Buy as u8, ENTRY_PRICE, QTY as u64);
    market_order(&mut ctx, ALICE, Side::Sell, QTY as u64);
    assert_eq!(position(&mut ctx, ALICE).amount, 0, "fixture: closed by a fill");
    place_order(&mut ctx, MAKER, Side::Sell as u8, ENTRY_PRICE, QTY as u64);
    market_order(&mut ctx, ALICE, Side::Buy, QTY as u64);
    assert_eq!(position(&mut ctx, ALICE).amount, QTY, "fixture: a NEW long, same size");
    assert_eq!(generation(&mut ctx), 1, "the fill path ended the first life");

    // Liquidity the stop COULD close into, so only the binding stands in its way.
    place_order(&mut ctx, MAKER, Side::Buy as u8, 9_850, QTY as u64);
    let t = tick(&mut ctx, 9_900, id);

    assert!(t.triggered, "the trigger did fire — the check is at fire time");
    assert_eq!(t.placed, None, "and placed nothing");
    assert_eq!(t.cancelled, Some(CancelReason::PositionClosed as u8));
    assert_eq!(position(&mut ctx, ALICE).amount, QTY, "the NEW long is untouched");
    assert!(storage::load_order(&mut ctx, &id).unwrap().is_none(), "spent");
    assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
}

/// A flip ends the life too. Unbound, this was already safe — the SELL is now the opening side,
/// so it expired as 6 — but it is the bound position that is gone, and 11 says so.
#[test]
fn a_close_position_stop_expires_after_a_flip() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    long_10(&mut ctx);
    let id = try_close_position_stop(&mut ctx, Side::Sell, 9_900).unwrap();
    set_amount(&mut ctx, -5);

    let t = tick(&mut ctx, 9_900, id);

    assert!(t.triggered);
    assert_eq!(t.placed, None);
    assert_eq!(t.cancelled, Some(CancelReason::PositionClosed as u8));
    assert_eq!(position(&mut ctx, ALICE).amount, -5);
}

/// A position that only SHRINKS (or grows — `close_position_takes_the_size_the_position_has_
/// when_it_fires`) is the same life, so the stop still fires, for whatever is open.
#[test]
fn a_close_position_stop_survives_a_partial_close() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    long_10(&mut ctx);
    let id = try_close_position_stop(&mut ctx, Side::Sell, 9_900).unwrap();
    set_amount(&mut ctx, 4);
    place_order(&mut ctx, MAKER, Side::Buy as u8, 9_850, QTY as u64);

    let t = tick(&mut ctx, 9_900, id);

    assert_eq!(t.placed, Some(4), "the whole of what is left");
    assert_eq!(t.cancelled, None);
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
}
