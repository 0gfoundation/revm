//! The market's position-size bound (Q7 residual): every opening fill holds a position to a value
//! of at most `i64::MAX` quote units at `max(maxPrice, mark)` (`math::position_fits_market`),
//! `updateMarket` refuses a `maxPrice` raise an open position cannot carry, and no fill executes
//! above `maxPrice` (`math::market_band_bounds`).
//!
//! The halts the Q7 review reproduced all started from a position past that bound, seeded straight
//! into storage at a `maxPrice` of $9,000,000 — long 2,000,000 valued at the mark as an ADL winner
//! or a liquidation maker, short 2,000,000 closed through asks above the mark. These tests show that
//! position can no longer be BUILT, by any route, and that the largest one that can liquidates
//! cleanly at the top of the price range.

use super::*;
use crate::interface::IPerpDex::{updateMarketCall, OrderCancelled, Trade};
use crate::types::TimeInForce;

/// The taker's `-2027` refusal, pinned as the wire string (`trading::POSITION_LIMIT_REJECT`).
const POSITION_LIMIT_REJECT: &str =
    "placeOrder: -2027 position exceeds the market's maximum notional";

const M2: Address = address!("5200000000000000000000000000000000000002");
/// $9,000,000 at `PRICE_DECIMALS = 2`: a `maxPrice` `updateMarket` accepts (one `maxQuantity`
/// order there is worth `9e18 <= i64::MAX`).
const HIGH_MAX_PRICE: u64 = 900_000_000;
/// `floor(i64::MAX / calc_value(HIGH_MAX_PRICE, 1))`: the largest position a $9M `maxPrice` admits.
const BOUND_AT_HIGH_MAX_PRICE: i64 = 1_024_819;
/// $1,000,000,000 in quote units — room for any margin these tests need.
const RICH: i64 = 1_000_000_000_000_000;

fn update_market(ctx: &mut TestCtx, max_price: u64) -> Result<Bytes, PerpError> {
    crate::risk::run_update_market(
        &updateMarketCall {
            marketId: MARKET_ID,
            tickSize: 1,
            stepSize: 1,
            minQuantity: 1,
            maxQuantity: 1_000_000,
            maxPrice: max_price,
            priceUpdateInterval: 15,
            active: true,
            fundingInterval: 0,
            interestRate: 0,
            liquidationFeeRateBps: 0,
            priceBandBps: 0,
        }
        .abi_encode(),
        ADMIN,
        ctx,
    )
}

fn fund(ctx: &mut TestCtx, users: &[Address]) {
    for &u in users {
        storage::save_account(
            ctx,
            u,
            UserAccount { perp_wallet_balance: RICH, ..UserAccount::default() },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
    }
}

fn place(
    ctx: &mut TestCtx,
    user: Address,
    side: Side,
    price: u64,
    qty: u64,
    tif: TimeInForce,
) -> Result<Bytes, PerpError> {
    run_place_order(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: side as u8,
            price,
            quantity: qty,
            orderType: 0,
            tif: tif as u8,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        }
        .abi_encode(),
        user,
        ctx,
    )
}

/// The bound at the fixture's scale, pinned on the helper every guard calls: at a $9M `maxPrice`
/// a position of 1,024,819 fits and 1,024,820 does not — whichever side, and whatever the mark
/// below `maxPrice`. A stored mark ABOVE `maxPrice` (an `updateMarket` lowered it, no oracle update
/// since) is the ceiling instead.
#[test]
fn the_position_bound_is_the_i64_value_at_max_price() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    update_market(&mut ctx, HIGH_MAX_PRICE).unwrap();
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    let per_contract = calc_value(HIGH_MAX_PRICE, 1, 0, PRICE_DECIMALS).unwrap();
    assert_eq!(BOUND_AT_HIGH_MAX_PRICE as u64, i64::MAX as u64 / per_contract);
    for sign in [1, -1] {
        for mark in [ENTRY_PRICE, HIGH_MAX_PRICE] {
            let fits = |a: i64| crate::math::position_fits_market(&market, mark, a);
            assert!(fits(sign * BOUND_AT_HIGH_MAX_PRICE), "{sign} at {mark}");
            assert!(!fits(sign * (BOUND_AT_HIGH_MAX_PRICE + 1)), "{sign} at {mark}");
        }
    }
    // A mark above `maxPrice` raises the ceiling to itself.
    assert!(!crate::math::position_fits_market(
        &market,
        HIGH_MAX_PRICE * 2,
        BOUND_AT_HIGH_MAX_PRICE
    ));
}

/// **A taker open past the bound is refused `-2027`, write-free**, on either side. ALICE opens
/// 1,000,000 against MAKER — inside the bound — and an order for 100,000 more is refused before any
/// write: the reviewer's 2,000,000 position cannot be built at this `maxPrice`. Closing is never
/// refused, so she can still take the position back down.
#[test]
fn a_taker_open_past_the_position_bound_is_refused_write_free() {
    for (open, close) in [(Side::Buy, Side::Sell), (Side::Sell, Side::Buy)] {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        update_market(&mut ctx, HIGH_MAX_PRICE).unwrap();
        fund(&mut ctx, &[ALICE, MAKER, M2]);
        place(&mut ctx, MAKER, close, ENTRY_PRICE, 1_000_000, TimeInForce::Gtc).unwrap();
        place(&mut ctx, ALICE, open, ENTRY_PRICE, 1_000_000, TimeInForce::Gtc).unwrap();
        let sign = if open == Side::Buy { 1 } else { -1 };
        assert_eq!(position(&mut ctx, ALICE).amount, sign * 1_000_000);
        place(&mut ctx, M2, close, ENTRY_PRICE, 100_000, TimeInForce::Gtc).unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let (pos, writes) = (position(&mut ctx, ALICE), ctx.perp_write_count());
        let err = place(&mut ctx, ALICE, open, ENTRY_PRICE, 100_000, TimeInForce::Gtc)
            .unwrap_err();
        assert_eq!(err.to_string(), POSITION_LIMIT_REJECT, "{open:?}");
        assert_eq!(ctx.perp_write_count(), writes, "{open:?}: a refused open wrote");
        assert_eq!(position(&mut ctx, ALICE), pos);
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
        assert_eq!(storage::book_order_count(&mut ctx, M2, MARKET_ID).unwrap(), 1, "M2 untouched");

        // Up to the bound itself is fine, and a close always is (MAKER's liquidity for it sits
        // a dollar outside M2's rest so the two do not cross).
        let room = (BOUND_AT_HIGH_MAX_PRICE - 1_000_000) as u64;
        place(&mut ctx, ALICE, open, ENTRY_PRICE, room, TimeInForce::Ioc).unwrap();
        assert_eq!(position(&mut ctx, ALICE).amount, sign * BOUND_AT_HIGH_MAX_PRICE);
        let away = if open == Side::Buy { ENTRY_PRICE - 100 } else { ENTRY_PRICE + 100 };
        place(&mut ctx, MAKER, open, away, 1_000, TimeInForce::Gtc).unwrap();
        place(&mut ctx, ALICE, close, away, 1_000, TimeInForce::Ioc).unwrap();
        assert_eq!(position(&mut ctx, ALICE).amount, sign * (BOUND_AT_HIGH_MAX_PRICE - 1_000));
    }
}

/// **A maker fill past the bound is refused like a K9 maker, never an `Err`.** ALICE's bid of
/// 1,000,000 was inside the bound when it rested (she was flat); she then bought 100,000 as a
/// taker, so the bid's fill would take her to 1,100,000. MAKER's sell into it does not fail: ALICE's
/// order is cancelled `MakerInsolvent` (reason 2) with no trade, her position stays at 100,000, and
/// MAKER's order rests in its place. Inside a liquidation's book leg the same `Err` used to halt
/// the sweep.
#[test]
fn a_maker_fill_past_the_position_bound_is_cancelled_not_an_err() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    update_market(&mut ctx, HIGH_MAX_PRICE).unwrap();
    fund(&mut ctx, &[ALICE, MAKER, M2]);
    let bid = ENTRY_PRICE - 100;
    place(&mut ctx, ALICE, Side::Buy, bid, 1_000_000, TimeInForce::Gtc).unwrap();
    place(&mut ctx, M2, Side::Sell, ENTRY_PRICE, 100_000, TimeInForce::Gtc).unwrap();
    place(&mut ctx, ALICE, Side::Buy, ENTRY_PRICE, 100_000, TimeInForce::Ioc).unwrap();
    assert_eq!(position(&mut ctx, ALICE).amount, 100_000);
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, MARKET_ID).unwrap(), 1);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    place(&mut ctx, MAKER, Side::Sell, bid, 1_000_000, TimeInForce::Gtc)
        .expect("a maker the bound refuses must not fail the taker");
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert!(decode_events::<Trade>(&logs).is_empty());
    let cancels: Vec<(Address, u8)> =
        decode_events::<OrderCancelled>(&logs).iter().map(|c| (c.user, c.reason)).collect();
    assert_eq!(cancels, [(ALICE, CancelReason::MakerInsolvent as u8)]);
    assert_eq!(position(&mut ctx, ALICE).amount, 100_000);
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, MARKET_ID).unwrap(), 0);
    assert_eq!(position(&mut ctx, MAKER).amount, 0);
    assert_eq!(storage::book_order_count(&mut ctx, MAKER, MARKET_ID).unwrap(), 1, "rested");
}

/// **An opening REST past the bound is refused `-2027` at placement**, like the fill it would make.
/// ALICE is long 1,000,000 at a $9M `maxPrice`; a bid of 100,000 more would leave 1,100,000, past
/// the bound, so it cannot rest — it used to rest and be cancelled at its first touch (the test
/// above shows that path, for a bid that fitted when it rested). A bid up to the bound rests, and
/// so does a pure close of any size.
#[test]
fn an_opening_rest_past_the_position_bound_is_refused() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    update_market(&mut ctx, HIGH_MAX_PRICE).unwrap();
    fund(&mut ctx, &[ALICE, MAKER]);
    place(&mut ctx, MAKER, Side::Sell, ENTRY_PRICE, 1_000_000, TimeInForce::Gtc).unwrap();
    place(&mut ctx, ALICE, Side::Buy, ENTRY_PRICE, 1_000_000, TimeInForce::Ioc).unwrap();
    assert_eq!(position(&mut ctx, ALICE).amount, 1_000_000);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let writes = ctx.perp_write_count();
    let err = place(&mut ctx, ALICE, Side::Buy, ENTRY_PRICE - 100, 100_000, TimeInForce::Gtc)
        .unwrap_err();
    assert_eq!(err.to_string(), POSITION_LIMIT_REJECT);
    assert_eq!(ctx.perp_write_count(), writes, "a refused rest wrote");
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, MARKET_ID).unwrap(), 0);

    let room = (BOUND_AT_HIGH_MAX_PRICE - 1_000_000) as u64;
    place(&mut ctx, ALICE, Side::Buy, ENTRY_PRICE - 100, room, TimeInForce::Gtc).unwrap();
    place(&mut ctx, ALICE, Side::Sell, ENTRY_PRICE + 100, 1_000_000, TimeInForce::Gtc).unwrap();
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, MARKET_ID).unwrap(), 2);
}

/// **`updateMarket` refuses a `maxPrice` raise an open position cannot carry.** ALICE is long
/// 2,000,000 — well inside the bound at the fixture's $10,000 `maxPrice` — so raising it to $9M,
/// where she is worth `1.8e19`, is refused with nothing written (the reviewer's 2,000,000 position
/// at a $9M `maxPrice`, by the other route). $4.5M, where she is worth `9e18`, is accepted; so is
/// any lowering.
#[test]
fn update_market_refuses_a_max_price_raise_an_open_position_cannot_carry() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund(&mut ctx, &[ALICE, MAKER, M2]);
    for maker in [MAKER, M2] {
        place(&mut ctx, maker, Side::Sell, ENTRY_PRICE, 1_000_000, TimeInForce::Gtc).unwrap();
        place(&mut ctx, ALICE, Side::Buy, ENTRY_PRICE, 1_000_000, TimeInForce::Ioc).unwrap();
    }
    assert_eq!(position(&mut ctx, ALICE).amount, 2_000_000);
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let writes = ctx.perp_write_count();
    let err = update_market(&mut ctx, HIGH_MAX_PRICE).unwrap_err();
    assert_eq!(err.to_string(), crate::risk::UPDATE_MARKET_POSITION_LIMIT_REJECT);
    assert_eq!(ctx.perp_write_count(), writes, "a refused updateMarket wrote");
    assert_eq!(storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap(), market);
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());

    let half = HIGH_MAX_PRICE / 2;
    assert_eq!(calc_value(half, 2_000_000, 0, PRICE_DECIMALS).unwrap(), 9_000_000_000_000_000_000);
    update_market(&mut ctx, half).unwrap();
    update_market(&mut ctx, 1_000_000).unwrap();
}

/// **No fill executes above `maxPrice`** — not even off an order that rested there before an
/// `updateMarket` lowered it. MAKER bids $105 (in band at the $100 mark); `maxPrice` is then lowered
/// to $102. ALICE's IOC sell at $100 no longer trades at $105 (it used to), and the next oracle
/// update expires the stranded bid as out of band (reason 4), so it stops being the best bid.
#[test]
fn no_fill_executes_above_a_lowered_max_price() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund(&mut ctx, &[ALICE, MAKER]);
    place(&mut ctx, MAKER, Side::Buy, 10_500, 1, TimeInForce::Gtc).unwrap();
    update_market(&mut ctx, 10_200).unwrap();
    assert_eq!(storage::load_market_hot(&mut ctx, MARKET_ID).unwrap().best_bid, 10_500);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    place(&mut ctx, ALICE, Side::Sell, ENTRY_PRICE, 1, TimeInForce::Ioc).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert!(decode_events::<Trade>(&logs).is_empty(), "filled above maxPrice");
    assert_eq!(position(&mut ctx, ALICE).amount, 0);

    advance_block_to(&mut ctx, 31);
    run_update_index_price(
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: ENTRY_PRICE, timestamp: 31 }
            .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());
    let cancels: Vec<(Address, u8)> =
        decode_events::<OrderCancelled>(&logs).iter().map(|c| (c.user, c.reason)).collect();
    assert_eq!(cancels, [(MAKER, CancelReason::PriceBandExpiry as u8)]);
    assert_eq!(storage::load_market_hot(&mut ctx, MARKET_ID).unwrap().best_bid, 0);
}

/// **The largest position the bound admits liquidates through asks at `maxPrice` without a fault.**
/// The reviewer's apply-phase halt, at the edge of the bound: ALICE ISOLATED short 1,000,000 @ $4.4M
/// (margin $1e12), two longs offering 500,000 each at $9M = `maxPrice`. The tick to $8.5M puts the
/// asks in band; the close takes all 1,000,000 there — a summed closing value of `9e18`, which fits
/// `i64` because the position does. (At 2,000,000 the same close was `1.8e19` and halted the node;
/// such a position can no longer be opened, see above.)
#[test]
fn the_largest_position_the_bound_admits_liquidates_at_max_price() {
    use crate::interface::IPerpDex::Liquidation;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    update_market(&mut ctx, HIGH_MAX_PRICE).unwrap();
    storage::save_mark_price(&mut ctx, MARKET_ID, 440_000_000).unwrap();
    let entry = calc_value(440_000_000, 1_000_000, 0, PRICE_DECIMALS).unwrap() as i64;
    seed_position_account(&mut ctx, ALICE, -1_000_000, entry, 1_000_000_000_000_000_000, 5, 0);
    for m in [MAKER, M2] {
        seed_position_account(&mut ctx, m, 500_000, -entry / 2, entry / 2, 1, entry);
        place(&mut ctx, m, Side::Sell, HIGH_MAX_PRICE, 500_000, TimeInForce::Gtc).unwrap();
    }
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    assert!(crate::math::position_fits_market(&market, HIGH_MAX_PRICE, -1_000_000));
    let _ = JournalTr::take_logs(ctx.journal_mut());

    advance_block_to(&mut ctx, 31);
    run_update_index_price(
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: 850_000_000, timestamp: 31 }
            .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .expect("a position inside the bound must liquidate, not halt");
    let logs = JournalTr::take_logs(ctx.journal_mut());
    let liq: Vec<(Address, i64)> =
        decode_events::<Liquidation>(&logs).iter().map(|l| (l.user, l.amount)).collect();
    assert_eq!(liq, [(ALICE, -1_000_000)]);
    let trades: Vec<(u64, u64)> =
        decode_events::<Trade>(&logs).iter().map(|t| (t.price, t.quantity)).collect();
    assert_eq!(trades, [(HIGH_MAX_PRICE, 500_000), (HIGH_MAX_PRICE, 500_000)]);
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
}

// ── Second-round review: the maker trial is TOTAL (bound-k9-1, bound-vquote-2) ────────────────
//
// The bound keeps a position's NOTIONAL representable, but neither its equity (margin is not
// bounded by notional) nor its entry basis after a `maxPrice` lowering. The maker fill's trial used
// to `?` the faults those leave — K9's `notional + vQuote + margin` in `i64`, `vQuote` past `i64` —
// so one such maker turned every taker reaching its order into a revert, and a liquidation's book
// leg reaching it into a halt of the sweep on every tick. Both are reached here by public calls
// only, on wallets an `i64` admits (the devnet's MockUSDC mints them freely).

const VICTIM: Address = address!("5200000000000000000000000000000000000003");
const QUOTE_9_2E18: i64 = 9_200_000_000_000_000_000;

fn seed_wallet(ctx: &mut TestCtx, user: Address, wallet: i64) {
    storage::save_account(
        ctx,
        user,
        UserAccount { perp_wallet_balance: wallet, ..UserAccount::default() },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

fn leverage(ctx: &mut TestCtx, user: Address, leverage: u64) {
    crate::risk::run_set_leverage(
        &setLeverageCall { marketId: MARKET_ID, leverage }.abi_encode(),
        user,
        ctx,
    )
    .unwrap();
}

/// M2 ISOLATED short 1,000,000 @ $9M at 3x, opened by public calls: M2 rests the ask, ALICE (2x)
/// takes it. Returns with the mark still at $9M.
fn m2_short_one_million_at_high_max_price(ctx: &mut TestCtx, m2_wallet: i64) {
    setup_market(ctx);
    update_market(ctx, HIGH_MAX_PRICE).unwrap();
    storage::save_mark_price(ctx, MARKET_ID, HIGH_MAX_PRICE).unwrap();
    seed_wallet(ctx, M2, m2_wallet);
    seed_wallet(ctx, ALICE, 4_600_000_000_000_000_000);
    seed_wallet(ctx, MAKER, RICH * 1_000);
    leverage(ctx, M2, 3);
    leverage(ctx, ALICE, 2);
    place(ctx, M2, Side::Sell, HIGH_MAX_PRICE, 1_000_000, TimeInForce::Gtc).unwrap();
    place(ctx, ALICE, Side::Buy, HIGH_MAX_PRICE, 1_000_000, TimeInForce::Ioc).unwrap();
    let m2 = position(ctx, M2);
    assert_eq!((m2.amount, m2.v_quote_balance), (-1_000_000, 9_000_000_000_000_000_000));
}

/// The tick a liquidation's book leg runs on: `index` at `ts = 31`.
fn tick(ctx: &mut TestCtx, index: u64) {
    advance_block_to(ctx, 31);
    run_update_index_price(
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: 31 }
            .abi_encode(),
        ADMIN,
        ctx,
    )
    .expect("the sweep must not fail on the maker its liquidation's book leg reaches");
}

/// **bound-k9-1: a maker whose equity leaves `i64` is valued, and fills.** M2 short 1,000,000 @ $9M
/// adds `6e18` of margin (`addPositionMargin`, its own wallet): margin and `vQuote` both `9e18`. At a
/// $8.7M mark its equity is `≈ 9.3e18` — `calc_position_equity` cannot hold it — yet it is far above
/// maintenance. Its ask of 10,000 @ $8.7M rests (1,010,000 fits the bound at $9M).
///
/// A maker filling it used to get `Err("math: maintenance margin value overflow")` from the K9
/// check (every taker at that level reverted), and a short VICTIM liquidated through it halted the
/// sweep (`liquidation.rs` panic). Now K9 sums the value in `i128`: the order fills, for the taker
/// and for the liquidation alike. M2 itself is `Faulted` in the sweep (its equity is not an `i64`,
/// so the isolated verdict skips it, as before), which writes nothing.
#[test]
fn a_maker_whose_equity_leaves_i64_still_fills() {
    for liquidation in [false, true] {
        let mut ctx = make_ctx();
        m2_short_one_million_at_high_max_price(&mut ctx, QUOTE_9_2E18);
        crate::risk::run_add_position_margin(
            &addPositionMarginCall { marketId: MARKET_ID, amount: 6_000_000_000_000_000_000 }
                .abi_encode(),
            M2,
            &mut ctx,
        )
        .unwrap();
        let mark = 870_000_000;
        storage::save_mark_price(&mut ctx, MARKET_ID, mark).unwrap();
        let m2 = position(&mut ctx, M2);
        let equity = crate::math::calc_position_equity(
            mark,
            m2.amount,
            m2.v_quote_balance,
            m2.margin,
            0,
            PRICE_DECIMALS,
        );
        assert!(equity.is_err(), "the fixture's point: M2's equity is not an i64");
        place(&mut ctx, M2, Side::Sell, mark, 10_000, TimeInForce::Gtc).unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());

        if liquidation {
            let entry = calc_value(800_000_000, 10_000, 0, PRICE_DECIMALS).unwrap() as i64;
            seed_position_account(&mut ctx, VICTIM, -10_000, entry, 1_000_000_000_000_000, 3, 0);
            tick(&mut ctx, mark);
        } else {
            place(&mut ctx, MAKER, Side::Buy, mark, 10_000, TimeInForce::Ioc)
                .expect("a taker reaching M2's ask must not revert");
        }
        let logs = JournalTr::take_logs(ctx.journal_mut());
        let taker = if liquidation { VICTIM } else { MAKER };
        let trades: Vec<(Address, Address, u64, u64)> = decode_events::<Trade>(&logs)
            .iter()
            .map(|t| (t.taker, t.maker, t.price, t.quantity))
            .collect();
        assert_eq!(trades, [(taker, M2, mark, 10_000)], "liquidation {liquidation}");
        assert!(decode_events::<OrderCancelled>(&logs).is_empty(), "liquidation {liquidation}");
        assert_eq!(position(&mut ctx, M2).amount, -1_010_000);
        let want = if liquidation { 0 } else { 10_000 };
        assert_eq!(position(&mut ctx, taker).amount, want, "liquidation {liquidation}");
    }
}

/// **bound-vquote-2: a maker fill that would take `vQuote` past `i64` is refused, never an `Err`.**
/// M2 short 1,000,000 @ $9M (`vQuote 9e18`); `updateMarket` LOWERS `maxPrice` to $6M — accepted:
/// the amount fits better, but the entry basis stays. At a $6M mark M2 rests an ask of 100,000
/// (1,100,000 fits the bound at $6M). Filling 60,000 of it would add `3.6e17` to a `vQuote` of `9e18`.
///
/// That used to be `Err("settlement: vQuote overflow")` out of the trial's `apply_position_fill`:
/// every taker reverted, and a VICTIM short liquidated through it halted the sweep. Now the trial is
/// total: M2's order is cancelled `MakerInsolvent` (reason 2), nothing trades with it, and the walk
/// goes on — the taker's IOC expires, the VICTIM's book leg finds no other liquidity.
#[test]
fn a_maker_fill_past_the_vquote_range_is_cancelled_not_an_err() {
    for liquidation in [false, true] {
        let mut ctx = make_ctx();
        m2_short_one_million_at_high_max_price(&mut ctx, 6_100_000_000_000_000_000);
        let low = 600_000_000;
        update_market(&mut ctx, low).expect("a lowering is always accepted");
        storage::save_mark_price(&mut ctx, MARKET_ID, low).unwrap();
        place(&mut ctx, M2, Side::Sell, low, 100_000, TimeInForce::Gtc).unwrap();
        let m2 = position(&mut ctx, M2);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        if liquidation {
            let entry = calc_value(550_000_000, 60_000, 0, PRICE_DECIMALS).unwrap() as i64;
            seed_position_account(&mut ctx, VICTIM, -60_000, entry, 1_000_000_000_000_000, 3, 0);
            tick(&mut ctx, low);
        } else {
            place(&mut ctx, MAKER, Side::Buy, low, 60_000, TimeInForce::Ioc)
                .expect("a taker reaching M2's ask must not revert");
        }
        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert!(decode_events::<Trade>(&logs).is_empty(), "liquidation {liquidation}");
        let cancels: Vec<(Address, u8)> =
            decode_events::<OrderCancelled>(&logs).iter().map(|c| (c.user, c.reason)).collect();
        assert_eq!(
            cancels,
            [(M2, CancelReason::MakerInsolvent as u8)],
            "liquidation {liquidation}"
        );
        let after = position(&mut ctx, M2);
        assert_eq!(
            (after.amount, after.v_quote_balance, after.margin),
            (m2.amount, m2.v_quote_balance, m2.margin),
            "liquidation {liquidation}: the refused maker's position did not move"
        );
        assert_eq!(storage::book_order_count(&mut ctx, M2, MARKET_ID).unwrap(), 0);
        if liquidation {
            use crate::interface::IPerpDex::Liquidation;
            let liq: Vec<(Address, i64)> =
                decode_events::<Liquidation>(&logs).iter().map(|l| (l.user, l.amount)).collect();
            assert_eq!(liq, [(VICTIM, -60_000)]);
        } else {
            assert_eq!(position(&mut ctx, MAKER).amount, 0);
        }
    }
}
