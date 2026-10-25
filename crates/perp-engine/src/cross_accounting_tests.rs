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
