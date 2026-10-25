//! Cross margin P2a — the accounting core and its read surfaces.
//!
//! * the funding-index MIRROR on `Market` (`storage::save_funding_state` is its one door);
//! * `UserAccount::open_position_count` (maintained by `storage::save_position`) and
//!   `UserAccount::liq_flags` (defined; P3 sets it), both carried over by `storage::save_account`;
//! * the invariant `CROSSED in force ⇒ pos.margin == 0`, asserted at `save_position`.
//!
//! Cross margin is gated off in production; the tests that need a position CROSSED in force take
//! the `#[cfg(test)]` override (`margin_type::test_gate`). Until the fill path gains its cross
//! branch, cross positions are SEEDED here (margin 0, through `save_position`), never opened by a
//! fill.
use alloy_sol_types::{SolCall, SolError};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::{
    host::PerpHost,
    interface::IPerpDex::{placeOrderCall, updateIndexPriceCall},
    run_perp_dex_call, storage,
    types::{
        AccountUpdateReason, FundingState, MarginTiers, MarginType, Market, PerpPosition,
        UserAccount, LIQ_FLAG_IN_LIQUIDATION,
    },
    PerpOutput, PERP_DEX_ADDRESS, USDC_ADDRESS,
};

// ── Fixture ───────────────────────────────────────────────────────────────────

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

/// Two markets on one grid: base 0 decimals, price 2 decimals, so one contract at price `P`
/// (cents) is worth `P · 10^4` quote units — every number below is exact.
const M1: u64 = 1;
const M2: u64 = 2;
/// $100.00.
const P100: u64 = 10_000;
/// One dollar in quote units.
const USD: i64 = 1_000_000;
/// $1e6 perp wallet: margin never interferes with the fills these tests drive.
const WALLET: i64 = 1_000_000 * USD;

const BUY: u8 = 0;
const SELL: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;

fn market(id: u64) -> Market {
    Market {
        market_id: id,
        base_decimals: 0,
        price_decimals: 2,
        tick_size: 1,
        step_size: 1,
        min_quantity: 1,
        max_quantity: 1_000_000,
        max_price: 100_000_000,
        price_update_interval: 15,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 0,
        price_band_bps: 0,
        mark_price: P100,
        tiers: MarginTiers::default(),
        basis_mode: 0,
        deferred_work: 0,
        cumulative_funding_index: 0,
    }
}

fn make_ctx() -> TestCtx {
    let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
    for a in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), a).unwrap();
    }
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    for id in [M1, M2] {
        storage::save_market(&mut ctx, &market(id)).unwrap();
    }
    for u in [ALICE, BOB] {
        let a = UserAccount { perp_wallet_balance: WALLET, ..UserAccount::default() };
        storage::save_account(&mut ctx, u, a, AccountUpdateReason::Adjustment).unwrap();
    }
    storage::flush_account_snapshots(&mut ctx).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());
    ctx
}

fn call(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, 30_000_000, caller, U256::ZERO, false, ctx)
        .expect("a perp call must not hard-fail")
}

fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    let out = call(ctx, caller, input);
    assert!(
        !out.reverted,
        "call reverted: {:?}",
        alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason)
    );
    out
}

fn order(market: u64, side: u8, price: u64, qty: u64, tif: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: market,
        side,
        price,
        quantity: qty,
        orderType: 0,
        tif,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode()
}

/// `taker` buys (`side` 0) or sells (1) `qty` contracts from `maker`'s resting GTC at `price`.
fn trade(ctx: &mut TestCtx, market: u64, taker: Address, maker: Address, side: u8, qty: u64) {
    ok(ctx, maker, &order(market, 1 - side, P100, qty, GTC));
    ok(ctx, taker, &order(market, side, P100, qty, IOC));
}

fn amount(ctx: &mut TestCtx, user: Address, market: u64) -> i64 {
    storage::load_position(ctx, user, market).unwrap().amount
}

fn count(ctx: &mut TestCtx, user: Address) -> u8 {
    storage::load_account(ctx, user).unwrap().open_position_count
}

fn funding_index(ctx: &mut TestCtx, market: u64) -> (i128, i128) {
    let mirror = storage::load_market_ref(ctx, market).unwrap().unwrap().cumulative_funding_index;
    let truth = storage::load_funding_state(ctx, market).unwrap().cumulative_funding_index;
    (mirror, truth)
}

fn tick(ctx: &mut TestCtx, market: u64, index_price: u64, ts: u64) {
    ok(
        ctx,
        ADMIN,
        &updateIndexPriceCall { marketId: market, indexPrice: index_price, timestamp: ts }
            .abi_encode(),
    );
}

/// Writes `pos` for `user` in `market` through the production door, as a planted fixture: its
/// snapshot mark and logs are dropped.
fn plant(ctx: &mut TestCtx, user: Address, market: u64, pos: PerpPosition) {
    storage::save_position(ctx, user, market, &pos, AccountUpdateReason::Adjustment).unwrap();
    storage::clear_account_snapshot_mark(ctx, user);
    let _ = JournalTr::take_logs(ctx.journal_mut());
}

// ── The funding-index mirror ──────────────────────────────────────────────────

/// Every oracle tick leaves the `Market`'s mirrored funding index equal to `FundingState`'s —
/// through an epoch start, two boundaries (the index moves) and the between-epoch ticks (it does
/// not) — and the mirror really moves with it.
#[test]
fn the_funding_index_mirror_follows_every_oracle_tick() {
    let mut ctx = make_ctx();
    let mut m = market(M1);
    m.funding_interval = 15;
    m.interest_rate = 100; // a non-zero rate even with mark == index
    storage::save_market(&mut ctx, &m).unwrap();

    let mut seen = Vec::new();
    for ts in [16, 31, 46, 61, 76, 91] {
        tick(&mut ctx, M1, P100, ts);
        let (mirror, truth) = funding_index(&mut ctx, M1);
        assert_eq!(mirror, truth, "tick {ts}: the mirror must equal FundingState");
        seen.push(truth);
    }
    assert_eq!(seen[0], 0, "the first tick only starts the epoch");
    assert_ne!(seen[1], 0, "the first boundary moved the index (and the mirror with it)");
    assert!(seen.windows(2).any(|w| w[0] != w[1] && w[0] != 0), "a later boundary accumulates");
    // M2 never ticked: both still at genesis.
    assert_eq!(funding_index(&mut ctx, M2), (0, 0));
}

/// `save_funding_state` is the one door of the index, and it writes the `Market` only when the
/// index actually moved: an unchanged index (the oracle's between-epoch bookkeeping save) costs
/// exactly the one `FundingState` write, a moved one adds the `Market`.
#[test]
fn save_funding_state_rewrites_the_market_only_when_the_index_moves() {
    let mut ctx = make_ctx();
    let mut fs =
        FundingState { last_funding_rate: 7, next_funding_ts: 30, cumulative_funding_index: 0 };

    let w = ctx.perp_write_count();
    storage::save_funding_state(&mut ctx, M1, &fs).unwrap();
    assert_eq!(ctx.perp_write_count() - w, 1, "an unchanged index leaves the Market alone");

    fs.cumulative_funding_index = -123_456_789_012_345_678_901;
    let w = ctx.perp_write_count();
    storage::save_funding_state(&mut ctx, M1, &fs).unwrap();
    assert_eq!(ctx.perp_write_count() - w, 2, "a moved index also writes the mirror");
    let moved = fs.cumulative_funding_index;
    assert_eq!(funding_index(&mut ctx, M1), (moved, moved));
    // Only the mirror field moved on the Market.
    let stored = storage::load_market_ref(&mut ctx, M1).unwrap().unwrap();
    assert_eq!(
        Market { cumulative_funding_index: 0, ..(*stored).clone() },
        market(M1),
        "nothing else on the Market moves"
    );

    // No Market to mirror into: the FundingState is still written, nothing else.
    let w = ctx.perp_write_count();
    storage::save_funding_state(&mut ctx, 99, &fs).unwrap();
    assert_eq!(ctx.perp_write_count() - w, 1);
    assert!(storage::load_market_ref(&mut ctx, 99).unwrap().is_none());
}

/// The settlement path cross-checks the mirror against the source of truth in debug builds: a
/// `Market` whose mirror was clobbered behind `save_funding_state`'s back is caught the first time
/// a position's funding is settled.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "mirrors cumulative funding index")]
fn a_diverged_mirror_is_caught_at_settlement() {
    let mut ctx = make_ctx();
    let fs =
        FundingState { last_funding_rate: 0, next_funding_ts: 0, cumulative_funding_index: 5 };
    storage::save_funding_state(&mut ctx, M1, &fs).unwrap();
    // Clobber the mirror with a whole-blob write of a stale copy.
    storage::save_market(&mut ctx, &market(M1)).unwrap();
    trade(&mut ctx, M1, ALICE, BOB, BUY, 1);
}

// ── open_position_count ───────────────────────────────────────────────────────

/// `+1` on an open from flat, `−1` on a close to flat, nothing on a flip, an add or a partial close
/// — across two markets, for both sides of every fill. (Liquidation and ADL: see
/// `risk::tests::the_open_position_count_follows_a_liquidation_and_an_adl`.) Every published
/// snapshot also cross-checks the stored count against a walk of the index in debug builds.
#[test]
fn the_open_position_count_follows_open_close_and_flip() {
    let mut ctx = make_ctx();
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, BOB)), (0, 0));

    trade(&mut ctx, M1, ALICE, BOB, BUY, 2); // ALICE +2, BOB −2: both open
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, BOB)), (1, 1));
    trade(&mut ctx, M1, ALICE, BOB, BUY, 1); // both add
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, BOB)), (1, 1));
    trade(&mut ctx, M2, BOB, ALICE, BUY, 1); // a second market, roles swapped
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, BOB)), (2, 2));
    trade(&mut ctx, M1, ALICE, BOB, SELL, 1); // partial close
    assert_eq!((amount(&mut ctx, ALICE, M1), count(&mut ctx, ALICE)), (2, 2));
    trade(&mut ctx, M1, ALICE, BOB, SELL, 5); // FLIP: +2 → −3 crosses no zero
    assert_eq!((amount(&mut ctx, ALICE, M1), amount(&mut ctx, BOB, M1)), (-3, 3));
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, BOB)), (2, 2));
    trade(&mut ctx, M1, ALICE, BOB, BUY, 3); // both close M1
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, BOB)), (1, 1));
    trade(&mut ctx, M2, BOB, ALICE, SELL, 1); // both close M2
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, BOB)), (0, 0));
    trade(&mut ctx, M2, BOB, ALICE, SELL, 1); // and reopen
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, BOB)), (1, 1));
}

/// A whole-blob `save_account` from a copy loaded BEFORE a position write (or before the flag was
/// set) cannot roll either cross field back: both are carried over from the store, like
/// `total_position_margin`.
#[test]
fn a_stale_account_copy_cannot_roll_back_the_count_or_the_flags() {
    let mut ctx = make_ctx();
    let stale = storage::load_account(&mut ctx, ALICE).unwrap();
    let pos =
        PerpPosition { amount: 1, v_quote_balance: -USD, margin: USD, ..PerpPosition::default() };
    plant(&mut ctx, ALICE, M1, pos);
    storage::mutate_account(&mut ctx, ALICE, |a| a.liq_flags = LIQ_FLAG_IN_LIQUIDATION).unwrap();
    storage::save_account(
        &mut ctx,
        ALICE,
        UserAccount { perp_wallet_balance: 7, ..stale },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let a = storage::load_account(&mut ctx, ALICE).unwrap();
    assert_eq!(a.perp_wallet_balance, 7, "the wallet leg IS the caller's");
    assert_eq!((a.open_position_count, a.liq_flags), (1, LIQ_FLAG_IN_LIQUIDATION));
    assert!(a.in_liquidation());
    assert_eq!(a.total_position_margin, USD);
}

// ── The invariant: CROSSED in force ⇒ pos.margin == 0 ─────────────────────────

/// A cross position holds no silo: with cross margin in force, writing a CROSSED position that
/// carries isolated margin trips the `save_position` assertion…
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "is CROSSED in force but holds isolated margin")]
fn a_crossed_position_holding_isolated_margin_is_refused_at_the_door() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    let pos =
        PerpPosition { amount: 1, v_quote_balance: -USD, margin: 1, ..PerpPosition::default() };
    assert_eq!(pos.margin_type, MarginType::Crossed.code());
    let _ = storage::save_position(&mut ctx, ALICE, M1, &pos, AccountUpdateReason::Adjustment);
}

/// …while the same bytes are an ordinary ISOLATED position when the gate is off (production), or
/// when the record stores ISOLATED, and a margin-free CROSSED position is the normal cross shape.
#[test]
fn the_invariant_binds_only_the_type_in_force() {
    let mut ctx = make_ctx();
    let silo =
        PerpPosition { amount: 1, v_quote_balance: -USD, margin: 1, ..PerpPosition::default() };
    plant(&mut ctx, ALICE, M1, silo.clone()); // gated off: ISOLATED in force

    let _cross = crate::margin_type::test_gate::enable();
    let mut flat = storage::load_position(&mut ctx, BOB, M1).unwrap();
    flat.margin_type = MarginType::Isolated.code();
    storage::save_position_margin_type_only(&mut ctx, BOB, M1, flat).unwrap();
    let isolated = PerpPosition { margin_type: MarginType::Isolated.code(), ..silo.clone() };
    plant(&mut ctx, BOB, M1, isolated);
    plant(&mut ctx, BOB, M2, PerpPosition { margin: 0, ..silo });
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, BOB)), (1, 2));
}

// ── The cross fold against an independent model ──────────────────────────────

/// A third market for the multi-market folds.
const M3: u64 = 3;

/// One leg of a fixture account: a position entered at `entry` (cents) plus resting orders,
/// `(price, qty)` each. `cross == false` stores ISOLATED (switched while flat, like
/// `setMarginType`).
#[derive(Clone)]
struct Leg {
    market: u64,
    amount: i64,
    entry: u64,
    leverage: u64,
    cross: bool,
    /// The isolated silo (`pos.margin`); must be 0 for a cross leg.
    silo: i64,
    buys: Vec<(u64, u64)>,
    sells: Vec<(u64, u64)>,
}

impl Leg {
    fn new(market: u64, amount: i64, entry: u64, leverage: u64, cross: bool, silo: i64) -> Self {
        Leg { market, amount, entry, leverage, cross, silo, buys: vec![], sells: vec![] }
    }
}

/// A market's mark and tier table (`(lowerBound, maxLeverage, mmrPpm)`, `0` = derived).
fn set_market(ctx: &mut TestCtx, id: u64, mark: u64, tiers: &[(u64, u32, u32)]) {
    let mut m = storage::load_market(ctx, id).unwrap().unwrap_or_else(|| market(id));
    m.mark_price = mark;
    let rows: Vec<crate::types::MarginTier> = tiers
        .iter()
        .map(|&(lower_bound_notional, max_leverage, maint_margin_rate_ppm)| {
            crate::types::MarginTier { lower_bound_notional, max_leverage, maint_margin_rate_ppm }
        })
        .collect();
    m.tiers = MarginTiers::from_tiers(&rows).unwrap();
    storage::save_market(ctx, &m).unwrap();
}

fn set_funding_index(ctx: &mut TestCtx, market: u64, index: i128) {
    let fs =
        FundingState { last_funding_rate: 0, next_funding_ts: 0, cumulative_funding_index: index };
    storage::save_funding_state(ctx, market, &fs).unwrap();
}

fn set_wallet(ctx: &mut TestCtx, user: Address, w: i64) {
    storage::mutate_account(ctx, user, |a| a.perp_wallet_balance = w).unwrap();
}

/// `trunc(price · qty · 10^6 / 10^(0 + 2))` — the fixture grid's value, written out.
fn value(price: u64, qty: u64) -> u64 {
    (price as u128 * qty as u128 * 10_000) as u64
}

/// Plants `leg` for `user` through the production doors: the type switch while flat, the order
/// lists with their maintained aggregates (each entry frozen at its Assuming Price — a buy's is its
/// limit, a sell's `max(T, limit)`), then the position.
fn plant_leg(ctx: &mut TestCtx, user: Address, leg: &Leg) {
    if !leg.cross {
        let mut flat = storage::load_position(ctx, user, leg.market).unwrap();
        flat.margin_type = MarginType::Isolated.code();
        storage::save_position_margin_type_only(ctx, user, leg.market, flat).unwrap();
    }
    let m = storage::load_market_ref(ctx, leg.market).unwrap().unwrap();
    let floor = crate::margin_view::assuming_price_floor(ctx, leg.market, &m).unwrap();
    let entry = |i: usize, price: u64, qty: u64, assuming: u64| {
        let mut id = [0u8; 32];
        id[0] = leg.market as u8;
        id[31] = i as u8;
        crate::types::OrderEntry {
            order_id: id,
            price,
            amount: qty,
            maker_fee_ppm: 0,
            assuming_price: assuming,
            reduce_only: false,
            stp: Default::default(),
        }
    };
    let buys: std::collections::VecDeque<_> =
        leg.buys.iter().enumerate().map(|(i, &(p, q))| entry(i, p, q, p)).collect();
    let sells: std::collections::VecDeque<_> = leg
        .sells
        .iter()
        .enumerate()
        .map(|(i, &(p, q))| entry(100 + i, p, q, p.max(floor)))
        .collect();
    storage::save_buy_orders(ctx, user, leg.market, &buys).unwrap();
    storage::save_sell_orders(ctx, user, leg.market, &sells).unwrap();
    let pos = PerpPosition {
        amount: leg.amount,
        v_quote_balance: -leg.amount * value(leg.entry, 1) as i64,
        margin: leg.silo,
        leverage: leg.leverage,
        total_buy_qty: buys.iter().map(|e| e.amount).sum(),
        total_buy_notional: buys.iter().map(|e| value(e.assuming_price, e.amount)).sum(),
        total_sell_qty: sells.iter().map(|e| e.amount).sum(),
        total_sell_notional: sells.iter().map(|e| value(e.assuming_price, e.amount)).sum(),
        ..storage::load_position(ctx, user, leg.market).unwrap()
    };
    plant(ctx, user, leg.market, pos);
}

/// The model's view of one market: mark, tiers and the cumulative funding index.
struct ModelMarket {
    mark: u64,
    tiers: Vec<(u64, u32, u32)>,
    index: i128,
}

fn ceil_div(n: i128, d: i128) -> i128 {
    (n + d - 1) / d
}

/// Slice-form maintenance margin, written from the spec: each band `[lb_i, lb_{i+1})` charges
/// `⌊width · rate_i⌋`, `rate_i = ppm_i / 1e6`, or `1 / (2 · maxLev_i)` when the column is 0.
fn model_mm(tiers: &[(u64, u32, u32)], n: i128) -> i128 {
    let mut mm = 0;
    for (i, &(lb, lev, ppm)) in tiers.iter().enumerate() {
        let hi = tiers.get(i + 1).map(|t| t.0 as i128).unwrap_or(i128::MAX);
        let width = n.min(hi) - lb as i128;
        if width <= 0 {
            break;
        }
        mm += if ppm == 0 { width / (2 * lev as i128) } else { width * ppm as i128 / 1_000_000 };
    }
    mm
}

/// The expected [`CrossFold`] for `legs` and wallet `w`, from the formulas alone (cross report
/// §3.2): `N = trunc(amount · mark · 1e4)` on this grid, `uPnL = N + vQuote` (0 at mark 0),
/// `PIM = ⌈|N|/L⌉`, `ooIM = ⌈max(|N+Bid|, |N−Ask|)/L⌉ − PIM`, `PF = −trunc(amount · index / 1e2)`.
fn model(legs: &[Leg], markets: &[(u64, ModelMarket)], w: i64) -> crate::margin_view::CrossFold {
    let mm_of = |id: u64| &markets.iter().find(|(m, _)| *m == id).unwrap().1;
    let (mut pf, mut upnl, mut pim, mut mm, mut oo, mut iso_oo) = (0i128, 0, 0, 0, 0, 0);
    let mut has_cross = false;
    for leg in legs {
        let mk = mm_of(leg.market);
        let n = leg.amount as i128 * mk.mark as i128 * 10_000;
        let l = leg.leverage.max(1) as i128;
        let bid: i128 = leg.buys.iter().map(|&(p, q)| value(p, q) as i128).sum();
        let ask: i128 =
            leg.sells.iter().map(|&(p, q)| value(p.max(mk.mark), q) as i128).sum();
        let leg_pim = ceil_div(n.abs(), l);
        let leg_oo = ceil_div((n + bid).abs().max((n - ask).abs()), l) - leg_pim;
        oo += leg_oo;
        if leg.cross {
            let vq = -(leg.amount as i128) * value(leg.entry, 1) as i128;
            upnl += if mk.mark == 0 { 0 } else { n + vq };
            pim += leg_pim;
            mm += model_mm(&mk.tiers, n.abs());
            // calc_funding_payment on this grid: −(amount · Δ · 1e6) / (1e2 · 1e0 · 1e6).
            pf += -(leg.amount as i128 * mk.index / 100);
            has_cross |= leg.amount != 0;
        } else {
            iso_oo += leg_oo;
        }
    }
    let cwb = w as i128 + pf;
    let a = cwb + upnl - pim - oo;
    crate::margin_view::CrossFold {
        w: w as i128,
        pending_funding_cross: pf,
        cwb,
        cross_upnl: upnl,
        cross_pim: pim,
        cross_mm: mm,
        oo_im_all: oo,
        iso_oo_im: iso_oo,
        e: cwb + upnl,
        a,
        x: (cwb - iso_oo - mm).min(a).max(0),
        has_cross,
        faulted_legs: 0,
    }
}

fn fold(ctx: &mut TestCtx, user: Address) -> crate::margin_view::CrossFold {
    let total = crate::margin_view::cross_fold(ctx, user, None, None, None).unwrap();
    let checked = crate::margin_view::cross_fold_checked(ctx, user, None, None, None).unwrap();
    assert_eq!(total, checked, "the total and the checked folds agree on a sound account");
    total
}

/// The multi-market fixture: two CROSSED legs (a long in profit with resting orders on both
/// sides and pending funding; a short at a loss whose notional crosses into the third bracket of
/// an explicit-rate table, also with funding pending) and an ISOLATED long at a loss holding a
/// silo, resting orders on both sides and funding pending of its own.
fn mixed_account(ctx: &mut TestCtx) -> (Vec<Leg>, Vec<(u64, ModelMarket)>) {
    let usd = USD as u64;
    let t2 = vec![(0, 50, 10_000), (1_000 * usd, 20, 25_000), (1_500 * usd, 10, 50_000)];
    set_market(ctx, M1, 10_000, &[(0, 3, 0)]);
    set_market(ctx, M2, 25_000, &t2);
    set_market(ctx, M3, 4_000, &[(0, 3, 0)]);
    // Index deltas: M1 0.75% of $100 (a long PAYS), M2 −0.5% of $250 (a short PAYS), M3 1%.
    set_funding_index(ctx, M1, 10_000 * 7_500);
    set_funding_index(ctx, M2, -25_000 * 5_000);
    set_funding_index(ctx, M3, 4_000 * 10_000);

    let mut long = Leg::new(M1, 10, 9_000, 5, true, 0); // +$100 at $100
    long.buys = vec![(9_500, 4), (9_000, 2)];
    long.sells = vec![(10_500, 3)];
    let mut short = Leg::new(M2, -7, 23_000, 10, true, 0); // −$140, |N| = $1,750 (bracket 3)
    short.sells = vec![(26_000, 1)];
    let mut iso = Leg::new(M3, 20, 4_500, 4, false, 200 * USD); // −$100 at $40
    iso.buys = vec![(3_900, 5)];
    iso.sells = vec![(4_200, 5)];
    let legs = vec![long, short, iso];
    for leg in &legs {
        plant_leg(ctx, ALICE, leg);
    }
    let markets = vec![
        (M1, ModelMarket { mark: 10_000, tiers: vec![(0, 3, 0)], index: 10_000 * 7_500 }),
        (M2, ModelMarket { mark: 25_000, tiers: t2, index: -25_000 * 5_000 }),
        (M3, ModelMarket { mark: 4_000, tiers: vec![(0, 3, 0)], index: 4_000 * 10_000 }),
    ];
    (legs, markets)
}

/// The headline: every [`CrossFold`] field of a mixed cross + isolated account equals the
/// independent model — positive and negative cross uPnL, pending funding on both cross legs (and
/// NOT on the isolated one), open orders on both kinds, a maintenance table crossed into its third
/// bracket — and the walk's per-leg output is the model's leg by leg.
#[test]
fn the_cross_fold_matches_an_independent_model_on_a_mixed_account() {
    let mut ctx = make_ctx();
    storage::save_market(&mut ctx, &market(M3)).unwrap();
    let _cross = crate::margin_type::test_gate::enable();
    let (legs, markets) = mixed_account(&mut ctx);
    set_wallet(&mut ctx, ALICE, 5_000 * USD);

    let got = fold(&mut ctx, ALICE);
    let want = model(&legs, &markets, 5_000 * USD);
    assert_eq!(got, want);
    // The fixture really exercises what it claims to.
    assert!(got.has_cross && got.pending_funding_cross < 0 && got.iso_oo_im > 0);
    assert_eq!(got.cross_upnl, (100 - 140) * USD as i128);
    assert_eq!(
        model_mm(&markets[1].1.tiers, 1_750 * USD as i128),
        10 * USD as i128 + 500 * 25_000 + 250 * 50_000,
        "$1,000 at 1% + $500 at 2.5% + $250 at 5%"
    );
    assert!(!got.below_cross_maintenance());

    // Per leg, in index order.
    let mut legs_out = Vec::new();
    crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, Some(&mut legs_out)).unwrap();
    assert_eq!(legs_out.iter().map(|l| l.market_id).collect::<Vec<_>>(), vec![M1, M2, M3]);
    for (got, leg) in legs_out.iter().zip(&legs) {
        let one = model(std::slice::from_ref(leg), &markets, 0);
        assert_eq!(got.cross, leg.cross);
        assert_eq!(got.oo_im, one.oo_im_all, "market {}", leg.market);
        assert_eq!((got.upnl, got.pim, got.mm), (one.cross_upnl, one.cross_pim, one.cross_mm));
        assert_eq!(got.pending_funding, one.pending_funding_cross);
    }
}

/// Gated off, the SAME account is ISOLATED in force everywhere: every cross term is 0 and the
/// admission basis is exactly the pre-P2 `W − Σ ooIM`, with `X = max(0, A)`.
#[test]
fn gated_off_the_fold_is_the_isolated_basis_it_always_was() {
    let mut ctx = make_ctx();
    storage::save_market(&mut ctx, &market(M3)).unwrap();
    // Plant with cross on (the cross legs must be margin-free), then read with it off.
    let (legs, markets) = {
        let _cross = crate::margin_type::test_gate::enable();
        mixed_account(&mut ctx)
    };
    set_wallet(&mut ctx, ALICE, 5_000 * USD);
    let got = fold(&mut ctx, ALICE);
    let isolated: Vec<Leg> = legs.iter().map(|l| Leg { cross: false, ..l.clone() }).collect();
    assert_eq!(got, model(&isolated, &markets, 5_000 * USD));
    assert_eq!(
        (got.cwb, got.e, got.cross_upnl, got.cross_mm, got.cross_pim),
        (got.w, got.w, 0, 0, 0)
    );
    assert!(!got.has_cross);
    // The isolated basis, from the pre-P2 per-market primitive.
    let mut sum_ooim = 0i128;
    for id in [M1, M2, M3] {
        let m = storage::load_market_ref(&mut ctx, id).unwrap().unwrap();
        let p = storage::load_position_ref(&mut ctx, ALICE, id).unwrap();
        sum_ooim += crate::margin_view::position_open_order_margin(&m, &p).unwrap() as i128;
    }
    assert_eq!(got.a, 5_000 * USD as i128 - sum_ooim);
    assert_eq!(got.x, got.a.max(0));
    assert_eq!(
        crate::margin_view::derived_available_balance(&mut ctx, ALICE).unwrap(),
        got.a,
        "the gates' basis is the fold's A"
    );
}

/// A negative cross uPnL big enough to put `E` under `Σ_C MM` is REPORTED (P2 does not act on it),
/// `A` goes negative and `X` is 0; a negative wallet alone does the same to `A` and `X`.
#[test]
fn an_underwater_cross_account_reports_below_maintenance_and_zero_withdrawable() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_market(&mut ctx, M1, 10_000, &[(0, 3, 0)]);
    plant_leg(&mut ctx, ALICE, &Leg::new(M1, 10, 14_000, 3, true, 0)); // −$400 at $100
    set_wallet(&mut ctx, ALICE, 500 * USD);
    let f = fold(&mut ctx, ALICE);
    // E = 500 − 400 = $100 < MM = ⌊$1,000 / 6⌋.
    assert_eq!((f.e, f.cross_mm), (100 * USD as i128, 166_666_666));
    assert!(f.below_cross_maintenance());
    assert!(f.a < 0 && f.x == 0);

    // At E == ΣMM exactly it is healthy (`>=`, the engine's rule).
    set_wallet(&mut ctx, ALICE, 400 * USD + 166_666_666);
    let f = fold(&mut ctx, ALICE);
    assert_eq!(f.e, f.cross_mm);
    assert!(!f.below_cross_maintenance());

    // A negative wallet with no cross position: A < 0, X = 0, nothing to liquidate.
    let mut ctx = make_ctx();
    set_wallet(&mut ctx, BOB, -5 * USD);
    let f = fold(&mut ctx, BOB);
    assert_eq!((f.a, f.x, f.has_cross), (-5 * USD as i128, 0, false));
    assert!(!f.below_cross_maintenance());
}

/// Cross unrealized PROFIT counts toward opening (`A`) but not toward withdrawal (`X`): the
/// wallet term `cWB − Σ_I ooIM − Σ_C MM` binds `X` once the profit exceeds the initial margin.
#[test]
fn cross_profit_opens_but_does_not_withdraw() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_market(&mut ctx, M1, 15_000, &[(0, 3, 0)]);
    plant_leg(&mut ctx, ALICE, &Leg::new(M1, 10, 10_000, 3, true, 0)); // +$500 at $150
    set_wallet(&mut ctx, ALICE, 100 * USD);
    let f = fold(&mut ctx, ALICE);
    // A = 100 + 500 − ⌈1,500 / 3⌉ = $100; wallet term = 100 − ⌊1,500 / 6⌋ = −$150.
    assert_eq!((f.a, f.cross_mm), (100 * USD as i128, 250 * USD as i128));
    assert_eq!(f.x, 0, "none of the profit is withdrawable, and the wallet is under MM");
    set_wallet(&mut ctx, ALICE, 400 * USD);
    let f = fold(&mut ctx, ALICE);
    assert_eq!((f.a, f.x), (400 * USD as i128, 150 * USD as i128));
    assert_eq!(f.x, f.cwb - f.cross_mm, "X is the wallet term, not A");
}

/// A cross leg in a market with NO mark (`0`, a fixture-only state): `uPnL`, `PIM` and `MM` are 0 —
/// it neither props the account up nor drags it under — while its resting orders still count.
#[test]
fn a_zero_mark_cross_leg_contributes_no_pnl_and_no_maintenance() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_market(&mut ctx, M1, 0, &[(0, 3, 0)]);
    set_market(&mut ctx, M2, 10_000, &[(0, 3, 0)]);
    let mut dark = Leg::new(M1, 10, 9_000, 2, true, 0);
    dark.buys = vec![(9_000, 1)];
    let lit = Leg::new(M2, 1, 9_000, 1, true, 0);
    for leg in [&dark, &lit] {
        plant_leg(&mut ctx, ALICE, leg);
    }
    set_wallet(&mut ctx, ALICE, 50 * USD);
    let f = fold(&mut ctx, ALICE);
    let markets = vec![
        (M1, ModelMarket { mark: 0, tiers: vec![(0, 3, 0)], index: 0 }),
        (M2, ModelMarket { mark: 10_000, tiers: vec![(0, 3, 0)], index: 0 }),
    ];
    assert_eq!(f, model(&[dark, lit], &markets, 50 * USD));
    assert_eq!(f.cross_upnl, 10 * USD as i128, "only the lit leg's +$10");
    assert_eq!(f.oo_im_all, 45 * USD as i128, "the dark leg's $90 bid at 2x still counts");
    assert_eq!(f.faulted_legs, 0);
}

/// An index entry naming no market: the CHECKED fold (gates, views) refuses it as an `[INVARIANT]`
/// exactly as the isolated basis always did.
#[test]
fn a_missing_market_is_refused_by_the_checked_fold() {
    let mut ctx = make_ctx();
    plant_leg(&mut ctx, ALICE, &Leg::new(M1, 1, 10_000, 1, false, 100 * USD));
    plant(&mut ctx, ALICE, 99, PerpPosition { amount: 1, margin: 1, ..PerpPosition::default() });
    let err =
        crate::margin_view::cross_fold_checked(&mut ctx, ALICE, None, None, None).unwrap_err();
    assert_eq!(
        err.to_string(),
        "[INVARIANT] derived margin: user market index holds unknown market 99"
    );
    assert_eq!(
        crate::margin_view::derived_available_balance(&mut ctx, ALICE).unwrap_err(),
        err,
        "the gates' basis refuses the same way"
    );
}

/// …and the TOTAL fold (the sweep's, P3) leaves it out — zero contribution, counted — and never
/// errs. Corrupt state, so a debug build stops on the `debug_assert` first; a release build checks
/// the numbers.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "holds unknown market 99"))]
fn a_missing_market_contributes_nothing_to_the_total_fold() {
    let mut ctx = make_ctx();
    plant_leg(&mut ctx, ALICE, &Leg::new(M1, 1, 10_000, 1, false, 100 * USD));
    let before = fold(&mut ctx, ALICE);
    plant(&mut ctx, ALICE, 99, PerpPosition { amount: 1, margin: 1, ..PerpPosition::default() });
    let after = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap();
    assert_eq!(after, crate::margin_view::CrossFold { faulted_legs: 1, ..before });
}

/// A leg whose arithmetic leaves the representable range (a notional past `i64`): refused by the
/// checked fold with the per-market error the isolated basis always raised, LEFT OUT and counted by
/// the total one — which still prices every other leg.
#[test]
fn an_overflowing_leg_is_refused_checked_and_left_out_total() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    plant_leg(&mut ctx, ALICE, &Leg::new(M1, 2, 9_000, 1, true, 0));
    let sound = fold(&mut ctx, ALICE);
    plant(&mut ctx, ALICE, M2, PerpPosition { amount: i64::MAX / 2, ..PerpPosition::default() });
    let err =
        crate::margin_view::cross_fold_checked(&mut ctx, ALICE, None, None, None).unwrap_err();
    assert_eq!(err.to_string(), "math: signed value exceeds i64");
    let total = crate::margin_view::cross_fold(&mut ctx, ALICE, None, None, None).unwrap();
    assert_eq!(total, crate::margin_view::CrossFold { faulted_legs: 1, ..sound });
}

/// The overrides price a post-trade state without touching storage: a wallet override replaces
/// `W`, a position override replaces one market's leg — or ADDS a leg for a market the user is
/// entering — and storage is left exactly as it was.
#[test]
fn the_overrides_price_a_hypothetical_post_state() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_market(&mut ctx, M1, 10_000, &[(0, 3, 0)]);
    set_market(&mut ctx, M2, 20_000, &[(0, 3, 0)]);
    let held = Leg::new(M1, 3, 9_000, 3, true, 0);
    plant_leg(&mut ctx, ALICE, &held);
    set_wallet(&mut ctx, ALICE, 1_000 * USD);
    let markets = vec![
        (M1, ModelMarket { mark: 10_000, tiers: vec![(0, 3, 0)], index: 0 }),
        (M2, ModelMarket { mark: 20_000, tiers: vec![(0, 3, 0)], index: 0 }),
    ];
    let stored = fold(&mut ctx, ALICE);

    // Replace the held leg (grown to 5) and the wallet.
    let grown = Leg::new(M1, 5, 9_200, 3, true, 0);
    let over = PerpPosition {
        amount: 5,
        v_quote_balance: -5 * value(9_200, 1) as i64,
        leverage: 3,
        ..PerpPosition::default()
    };
    let got =
        crate::margin_view::cross_fold(&mut ctx, ALICE, Some(7 * USD), Some((M1, &over)), None)
            .unwrap();
    assert_eq!(got, model(std::slice::from_ref(&grown), &markets, 7 * USD));

    // Enter M2 (not in the index yet): its leg is ADDED.
    let entering = Leg::new(M2, -2, 21_000, 2, true, 0);
    let over2 = PerpPosition {
        amount: -2,
        v_quote_balance: 2 * value(21_000, 1) as i64,
        leverage: 2,
        ..PerpPosition::default()
    };
    let got = crate::margin_view::cross_fold(&mut ctx, ALICE, None, Some((M2, &over2)), None)
        .unwrap();
    assert_eq!(got, model(&[held, entering], &markets, 1_000 * USD));

    assert_eq!(fold(&mut ctx, ALICE), stored, "pricing a hypothetical wrote nothing");
}
