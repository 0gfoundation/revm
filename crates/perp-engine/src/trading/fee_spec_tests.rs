//! **Trading fees and maker rebates through the full call shell**: the rate surface
//! (`setUserFeeRates` in ppm with a signed maker rate, the rebate bound `R` and its setter), how the
//! ONE taker fee an order pays is attributed to its fills (`Trade.takerFee`), and the maker rebate —
//! per fill, capped by that fill's taker-fee share, netted off the fee recipient's credit.
//!
//! Every call goes through `run_perp_dex_call`, so the gas charge, the commit-only #23 tripwire, the
//! debug event guards in `crate::events` and the end-of-call `AccountBalanceChanged` drain are all in
//! the loop — and so are the two production `debug_assert`s this suite leans on:
//! `TakerSettlement::record_fill` (the share recorded is the share priced) and `finalize_core`
//! (Σ shares == the fee charged).
//!
//! The market is chosen so fees TRUNCATE: base 0 / price 6 decimals, tick 1, step 1, so a fill of
//! `q` at `p` has notional `n = p·q` micro-USDC exactly, and a level `5_000` micro-dollars off a
//! round price leaves half a unit of fee on the floor per fill. That is what makes the cumulative
//! split observably different from an independent per-fill floor.

use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::call::PerpOutput;
use crate::host::PerpHost;
use crate::interface::IPerpDex::{
    asSubAccountCall, batchPlaceOrdersCall, createSubAccountCall, getMaxMakerRebateCall,
    getUserFeeRatesCall, placeAlgoOrderCall, placeOrderCall, setMaxMakerRebateCall,
    setUserFeeRatesCall, updateIndexPriceCall, Liquidation, MaxMakerRebateUpdated, OrderCancelled,
    PlaceItem, Trade, UserFeeRatesUpdated,
};
use crate::types::{derive_sub_account, AccountUpdateReason, MarginTiers, Market, PerpPosition};
use crate::{run_perp_dex_call, storage, PERP_DEX_ADDRESS, USDC_ADDRESS};

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

/// The taker in every scenario.
const ALICE: Address = address!("1111111111111111111111111111111111111111");
/// Makers.
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
const DAVE: Address = address!("4444444444444444444444444444444444444444");
/// The fee recipient.
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const MARKET_ID: u64 = 1;
/// $100.000000 at price_decimals 6.
const MARK: u64 = 100_000_000;
/// $1e9 — margin never interferes unless a test sets the wallet itself.
const WALLET: i64 = 1_000_000_000_000_000;
const GAS: u64 = 30_000_000;

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MKT: u8 = 1;
const GTC: u8 = 0;

// ── Fixture ──────────────────────────────────────────────────────────────────────────────────

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    // Isolated margin, explicitly: every position this suite opens is ISOLATED unless a test
    // switches it (`margin_type::test_gate::isolated_by_default`), under the real gate.
    crate::margin_type::test_gate::isolated_by_default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, CAROL, DAVE, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

/// Market 1 at a $100 mark (base 0 / price 6 decimals, tick 1, step 1, no funding, the default
/// 10% fill-time band), a $1e9 perp wallet for ALICE, BOB, CAROL and DAVE, and an EMPTY
/// fee-recipient wallet, so every unit the admin holds afterwards is a fee. The rebate bound `R`
/// is at its default, 0. Incidental logs discarded.
fn fresh() -> TestCtx {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(
        &mut ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 0,
            price_decimals: 6,
            tick_size: 1,
            step_size: 1,
            min_quantity: 1,
            max_quantity: 1_000_000,
            max_price: 1_000_000_000_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 0,
            mark_price: 0,
            tiers: MarginTiers::default(),
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
        },
    )
    .unwrap();
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK).unwrap();
    for u in [ALICE, BOB, CAROL, DAVE] {
        set_wallet(&mut ctx, u, WALLET);
    }
    let _ = take_logs(&mut ctx);
    ctx
}

/// Plants `perp` as `user`'s perp wallet, keeping every other account field.
fn set_wallet(ctx: &mut TestCtx, user: Address, perp: i64) {
    let mut a = storage::load_account(ctx, user).unwrap();
    a.perp_wallet_balance = perp;
    storage::save_account(ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
}

/// Seeds `user` with `amount` contracts entered at the $100 mark, OFF-BOOK (so `last_traded`
/// stays 0 and an oracle tick sets `mark == index` exactly).
fn seed_pos(ctx: &mut TestCtx, user: Address, amount: i64, leverage: u64) {
    storage::save_position(
        ctx,
        user,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance: -amount * MARK as i64,
            margin: amount.abs() * MARK as i64 / leverage as i64,
            leverage,
            ..crate::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let _ = take_logs(ctx);
}

// ── Shell helpers ────────────────────────────────────────────────────────────────────────────

fn take_logs(ctx: &mut TestCtx) -> Vec<primitives::Log> {
    JournalTr::take_logs(ctx.journal_mut())
}

/// The `Error(string)` reason of a reverted shell output.
fn revert_reason(out: &PerpOutput) -> String {
    assert!(out.reverted, "the call did not revert");
    let b = &out.bytes;
    assert_eq!(&b[..4], &[0x08, 0xc3, 0x79, 0xa0], "Error(string) selector");
    let len = U256::from_be_slice(&b[36..68]).to::<usize>();
    String::from_utf8(b[68..68 + len].to_vec()).unwrap()
}

/// One SUCCESSFUL shell call; returns the output and exactly this call's logs.
fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> (PerpOutput, Vec<primitives::Log>) {
    let _ = take_logs(ctx);
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(!out.reverted, "unexpected revert: {}", revert_reason(&out));
    let logs = take_logs(ctx);
    (out, logs)
}

/// A REJECTED shell call: the exact reason, no perp write, no log.
fn rejected(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
    let _ = take_logs(ctx);
    let writes = ctx.perp_write_count();
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(out.reverted, "expected the revert `{expect}`, but the call SUCCEEDED");
    assert_eq!(revert_reason(&out), expect);
    assert_eq!(ctx.perp_write_count(), writes, "[{expect}] the rejected call wrote perp state");
    assert!(take_logs(ctx).is_empty(), "[{expect}] the rejected call logged");
}

fn evs<E: SolEvent>(logs: &[primitives::Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

fn place(side: u8, price: u64, qty: u64, order_type: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif: GTC,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode()
}

/// A resting GTC that crossed nothing.
fn rest(ctx: &mut TestCtx, user: Address, side: u8, price: u64, qty: u64) {
    let (_, logs) = ok(ctx, user, &place(side, price, qty, LIMIT));
    assert!(evs::<Trade>(&logs).is_empty(), "{user} {side}@{price} traded while resting");
}

/// A market order; returns its `Trade` rows.
fn take(ctx: &mut TestCtx, user: Address, side: u8, qty: u64) -> Vec<Trade> {
    let (_, logs) = ok(ctx, user, &place(side, 0, qty, MKT));
    evs::<Trade>(&logs)
}

fn rates_input(user: Address, maker_ppm: i64, taker_ppm: u64) -> Vec<u8> {
    setUserFeeRatesCall {
        user,
        makerFeePpm: maker_ppm,
        takerFeePpm: taker_ppm,
    }
    .abi_encode()
}

fn set_fees(ctx: &mut TestCtx, user: Address, maker_ppm: i64, taker_ppm: u64) {
    ok(ctx, ADMIN, &rates_input(user, maker_ppm, taker_ppm));
}

/// `getUserFeeRates(user)` through the shell, as `(makerFeePpm, takerFeePpm)`.
fn rates(ctx: &mut TestCtx, user: Address) -> (i64, u64) {
    let out = run_perp_dex_call(
        &getUserFeeRatesCall { user }.abi_encode(),
        GAS,
        BOB,
        U256::ZERO,
        true,
        ctx,
    )
    .unwrap();
    assert!(!out.reverted);
    let r = getUserFeeRatesCall::abi_decode_returns(&out.bytes).unwrap();
    (r.makerFeePpm, r.takerFeePpm)
}

/// The fee recipient's signed perp wallet.
fn admin_wallet(ctx: &mut TestCtx) -> i64 {
    storage::load_account(ctx, ADMIN).unwrap().perp_wallet_balance
}

fn fee_total(ctx: &mut TestCtx) -> u64 {
    storage::load_market_fee_total(ctx, MARKET_ID).unwrap()
}

fn wallet(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_account(ctx, user).unwrap().perp_wallet_balance
}

fn position(ctx: &mut TestCtx, user: Address) -> PerpPosition {
    storage::load_position(ctx, user, MARKET_ID).unwrap()
}

/// The GROSS perp wallet `wb = cross wallet + this market's silo` — what a fee or a rebate moves
/// and an opening margin draw does not.
fn wb(ctx: &mut TestCtx, user: Address) -> i64 {
    wallet(ctx, user) + position(ctx, user).margin
}

/// `Σ wb` over every party plus the insurance fund. Fees and rebates only move value between
/// these, and an OPENING fill at the fill price realises nothing, so across any match that only
/// opens positions this is constant to the unit.
fn money(ctx: &mut TestCtx) -> i128 {
    [ALICE, BOB, CAROL, DAVE, ADMIN]
        .into_iter()
        .map(|u| wb(ctx, u) as i128)
        .sum::<i128>()
        + storage::load_insurance_fund(ctx).unwrap() as i128
}

/// Sets the rebate bound `R` through the shell; returns the call's logs.
fn set_bound(ctx: &mut TestCtx, ppm: u64) -> Vec<primitives::Log> {
    let (out, logs) = ok(
        ctx,
        ADMIN,
        &setMaxMakerRebateCall {
            maxMakerRebatePpm: ppm,
        }
        .abi_encode(),
    );
    assert_eq!(out.gas_used, 30_000, "the flat setMaxMakerRebate price");
    logs
}

/// `getMaxMakerRebate()` through the shell.
fn bound(ctx: &mut TestCtx) -> u64 {
    let out = run_perp_dex_call(
        &getMaxMakerRebateCall {}.abi_encode(),
        GAS,
        BOB,
        U256::ZERO,
        true,
        ctx,
    )
    .unwrap();
    assert!(!out.reverted);
    assert_eq!(out.gas_used, 5_000, "the scalar-getter tier");
    getMaxMakerRebateCall::abi_decode_returns(&out.bytes).unwrap()
}

/// A maker at `maker_ppm` (raising `R` to cover it first) — the usual fixture line.
fn set_maker(ctx: &mut TestCtx, user: Address, maker_ppm: i64) {
    if maker_ppm < 0 && bound(ctx) < maker_ppm.unsigned_abs() {
        set_bound(ctx, maker_ppm.unsigned_abs());
    }
    set_fees(ctx, user, maker_ppm, 0);
}

/// The oracle tick, through the shell, at `ts = 31`. It must SUCCEED.
fn tick(ctx: &mut TestCtx, index: u64) -> Vec<primitives::Log> {
    let (_, logs) = ok(
        ctx,
        ADMIN,
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: index,
            timestamp: 31,
        }
        .abi_encode(),
    );
    logs
}

/// `calc_value(p, q)` on this market: `p·q·1e6 / (1e6·1e0)`.
fn notional(p: u64, q: u64) -> u64 {
    p * q
}

/// A non-negative rate's fee, recomputed from the ABI doc: `floor(n · ppm / 1e6)`.
fn fee(n: u64, ppm: u64) -> u64 {
    (n as u128 * ppm as u128 / 1_000_000) as u64
}

/// The taker rate the attribution tests run at: 100 ppm = 1 bp.
const TAKER_PPM: u64 = 100;

/// The maker levels a `taker_side` order crosses, touch first: three levels each `5_000`
/// micro-dollars off a round price, so every fill's 100 ppm (1 bp) fee is `x.5` units before the floor.
fn levels(taker_side: u8) -> [u64; 3] {
    if taker_side == BUY {
        [MARK + 5_000, MARK + 15_000, MARK + 25_000]
    } else {
        [MARK - 5_000, MARK - 15_000, MARK - 25_000]
    }
}

// ── Rates: units, bounds, events ─────────────────────────────────────────────────────────────

/// 100_000 ppm = 10%, the ceiling for either rate (`math::MAX_FEE_PPM`).
const MAX_FEE: u64 = 100_000;

const E_TAKER_MAX: &str = "setUserFeeRates: taker fee exceeds 100000 ppm (10%)";
const E_MAKER_MAX: &str = "setUserFeeRates: maker fee exceeds 100000 ppm (10%)";
const E_NOT_ADMIN: &str = "not authorised: caller is not admin";
const E_BOUND_CEILING: &str = "setMaxMakerRebate: exceeds 10000 ppm (1%)";

/// The reason a maker rate below `−R` is refused with, `R` as read at the call.
fn e_rebate(r: u64) -> String {
    format!("setUserFeeRates: maker rebate exceeds maxMakerRebate ({r} ppm)")
}

/// `0 ≤ taker ≤ 100_000` and `−R ≤ maker ≤ 100_000` (here at the default `R = 0`), both ends
/// inclusive; one past either end is refused write-free with its own reason, and a refusal leaves
/// the previous rates in place. Each
/// accepted call publishes `UserFeeRatesUpdated(user, int64 makerFeePpm, uint64 takerFeePpm)` with
/// exactly the stored pair, and `getUserFeeRates` returns it.
#[test]
fn fee_rates_are_ppm_bounded_at_ten_percent_and_published_as_set() {
    let mut ctx = fresh();
    for (m, t) in [(0i64, 0u64), (1, 1), (MAX_FEE as i64, MAX_FEE), (0, MAX_FEE), (MAX_FEE as i64, 0), (200, 500)] {
        let (out, logs) = ok(&mut ctx, ADMIN, &rates_input(ALICE, m, t));
        assert_eq!(out.gas_used, 80_000, "the flat setUserFeeRates price");
        let ev = evs::<UserFeeRatesUpdated>(&logs);
        assert_eq!(
            ev.iter().map(|e| (e.user, e.makerFeePpm, e.takerFeePpm)).collect::<Vec<_>>(),
            vec![(ALICE, m, t)]
        );
        assert_eq!(rates(&mut ctx, ALICE), (m, t));
    }
    for (m, t, reason) in [
        (0i64, MAX_FEE + 1, E_TAKER_MAX),
        (0, u64::MAX, E_TAKER_MAX),
        (MAX_FEE as i64 + 1, 0, E_MAKER_MAX),
        (i64::MAX, 0, E_MAKER_MAX),
        (-1, 0, &*e_rebate(0)),
        (i64::MIN, 0, &*e_rebate(0)),
    ] {
        rejected(&mut ctx, ADMIN, &rates_input(ALICE, m, t), reason);
        assert_eq!(rates(&mut ctx, ALICE), (200, 500), "a refusal keeps the stored rates");
    }
    rejected(&mut ctx, BOB, &rates_input(ALICE, 1, 1), E_NOT_ADMIN);
    // The default is zero for an address nobody configured.
    assert_eq!(rates(&mut ctx, CAROL), (0, 0));
}

/// Fees are charged at `floor(n · ppm / 1e6)`, and for a positive rate that is EXACTLY the bps
/// fee the same schedule charged before the unit change (`floor(n · bps / 1e4)` at `ppm = 100·bps`).
/// A maker at 200 ppm (2 bps) and a taker at 500 ppm (5 bps) on a fill whose fees truncate.
#[test]
fn a_fill_is_charged_its_ppm_rates_exactly_as_the_bps_rates_were() {
    for side in [BUY, SELL] {
        let mut ctx = fresh();
        set_fees(&mut ctx, ALICE, 0, 500);
        set_fees(&mut ctx, BOB, 200, 0);
        let p = levels(side)[0];
        rest(&mut ctx, BOB, 1 - side, p, 3);
        let admin_before = admin_wallet(&mut ctx);
        let fee_total_before = fee_total(&mut ctx);

        let trades = take(&mut ctx, ALICE, side, 3);

        let n = notional(p, 3);
        let (taker_fee, maker_fee) = (fee(n, 500), fee(n, 200));
        assert_eq!((taker_fee, maker_fee), (n * 5 / 10_000, n * 2 / 10_000), "ppm == bps × 100");
        assert_ne!(n * 5 % 10_000, 0, "fixture: the taker fee truncates");
        assert_eq!(trades.len(), 1);
        assert_eq!((trades[0].takerFee, trades[0].makerFee), (taker_fee, maker_fee as i64));
        assert_eq!(admin_wallet(&mut ctx) - admin_before, (taker_fee + maker_fee) as i64);
        assert_eq!(fee_total(&mut ctx) - fee_total_before, taker_fee + maker_fee);
    }
}

/// `R` defaults to 0 — rebates disabled — and is admin-only, `0 ≤ R ≤ 10_000` ppm (1%). Every
/// accepted set publishes `MaxMakerRebateUpdated(R)` and is read back by `getMaxMakerRebate`; one
/// past the ceiling and a non-admin caller are refused write-free.
#[test]
fn the_rebate_bound_is_admin_only_capped_at_one_percent_and_defaults_to_zero() {
    let mut ctx = fresh();
    assert_eq!(bound(&mut ctx), 0, "rebates are disabled until the admin raises R");
    for r in [0u64, 1, 110, 10_000, 0] {
        let logs = set_bound(&mut ctx, r);
        let ev = evs::<MaxMakerRebateUpdated>(&logs);
        assert_eq!(ev.iter().map(|e| e.maxMakerRebatePpm).collect::<Vec<_>>(), vec![r]);
        assert_eq!(logs.len(), 1, "a parameter, not a balance: no AccountBalanceChanged");
        assert_eq!(bound(&mut ctx), r);
    }
    set_bound(&mut ctx, 250);
    for r in [10_001u64, u64::MAX] {
        rejected(
            &mut ctx,
            ADMIN,
            &setMaxMakerRebateCall { maxMakerRebatePpm: r }.abi_encode(),
            E_BOUND_CEILING,
        );
    }
    rejected(&mut ctx, BOB, &setMaxMakerRebateCall { maxMakerRebatePpm: 1 }.abi_encode(), E_NOT_ADMIN);
    assert_eq!(bound(&mut ctx), 250, "a refusal keeps the bound");
}

/// The maker floor is `−R` AS OF THE CALL, inclusive, with no rule tying it to any taker rate (a
/// taker rate of 0 next to the deepest rebate is accepted). Lowering `R` afterwards does NOT touch
/// a rate already set below the new floor: it stays stored and readable, and only a NEW call is
/// held to the new floor.
#[test]
fn the_maker_floor_is_minus_r_at_call_time_and_lowering_r_keeps_existing_rates() {
    let mut ctx = fresh();
    set_bound(&mut ctx, 150);
    for (m, t) in [(-150i64, 0u64), (-150, MAX_FEE), (-1, 1), (MAX_FEE as i64, 0)] {
        let (_, logs) = ok(&mut ctx, ADMIN, &rates_input(ALICE, m, t));
        let ev = evs::<UserFeeRatesUpdated>(&logs);
        assert_eq!(
            ev.iter().map(|e| (e.user, e.makerFeePpm, e.takerFeePpm)).collect::<Vec<_>>(),
            vec![(ALICE, m, t)],
            "the event carries the signed rate"
        );
        assert_eq!(rates(&mut ctx, ALICE), (m, t));
    }
    rejected(&mut ctx, ADMIN, &rates_input(ALICE, -151, 0), &e_rebate(150));
    set_fees(&mut ctx, BOB, -150, 300);

    set_bound(&mut ctx, 50);
    assert_eq!(rates(&mut ctx, BOB), (-150, 300), "lowering R re-validates nothing");
    rejected(&mut ctx, ADMIN, &rates_input(CAROL, -150, 0), &e_rebate(50));
    rejected(&mut ctx, ADMIN, &rates_input(BOB, -51, 300), &e_rebate(50));
    assert_eq!(rates(&mut ctx, BOB), (-150, 300), "…and a refused re-set keeps it");
    set_fees(&mut ctx, CAROL, -50, 0);
    assert_eq!(rates(&mut ctx, CAROL), (-50, 0));
}

/// A master's NEGATIVE maker rate is pushed down to its subs exactly like a positive one: one
/// write and one `UserFeeRatesUpdated` (signed) per family member, and a sub is still refused
/// directly.
#[test]
fn a_negative_maker_rate_is_pushed_down_to_the_sub_accounts() {
    let mut ctx = fresh();
    let (_, _) = ok(&mut ctx, BOB, &createSubAccountCall { name: "a".into() }.abi_encode());
    let (_, _) = ok(&mut ctx, BOB, &createSubAccountCall { name: "b".into() }.abi_encode());
    let (s0, s1) = (derive_sub_account(BOB, 0), derive_sub_account(BOB, 1));
    set_bound(&mut ctx, 100);
    let (_, logs) = ok(&mut ctx, ADMIN, &rates_input(BOB, -100, 300));
    assert_eq!(
        evs::<UserFeeRatesUpdated>(&logs)
            .iter()
            .map(|e| (e.user, e.makerFeePpm, e.takerFeePpm))
            .collect::<Vec<_>>(),
        vec![(BOB, -100, 300), (s0, -100, 300), (s1, -100, 300)]
    );
    for u in [BOB, s0, s1] {
        assert_eq!(rates(&mut ctx, u), (-100, 300));
    }
    rejected(
        &mut ctx,
        ADMIN,
        &rates_input(s0, -100, 300),
        "setUserFeeRates: user is a sub-account; set its master's rates",
    );
    // A sub created later inherits the signed rate.
    let (_, _) = ok(&mut ctx, BOB, &createSubAccountCall { name: "c".into() }.abi_encode());
    assert_eq!(rates(&mut ctx, derive_sub_account(BOB, 2)), (-100, 300));
}

// ── Taker fee attribution ────────────────────────────────────────────────────────────────────

/// `Trade.takerFee` is the fill's share of the ONE fee the taker pays, split by differences of
/// cumulative floors, so the shares sum to that fee EXACTLY — where the old independent per-fill
/// floor dropped up to `N − 1` units.
///
/// Fixture: three fills of notional `100_005_000`, `100_015_000`, `100_025_000` (or the bid-side
/// mirror) at a 100 ppm (1 bp) taker rate, i.e. `10_000.5`, `10_001.5`, `10_002.5` units before the floor.
///
/// ```text
/// C_k          floor(C_k · 100/1e6)    share_k
/// 100_005_000         10_000            10_000
/// 200_020_000         20_002            10_002
/// 300_045_000         30_004            10_002     Σ = 30_004 = F
/// ```
///
/// The independent floors give `10_000 + 10_001 + 10_002 = 30_003` — one unit short of what the
/// taker was charged — so the fixture DISCRIMINATES between the two splits (asserted).
#[test]
fn taker_fee_shares_telescope_to_the_one_fee_charged() {
    for side in [BUY, SELL] {
        let mut ctx = fresh();
        set_fees(&mut ctx, ALICE, 0, TAKER_PPM);
        let maker_side = 1 - side;
        let lv = levels(side);
        rest(&mut ctx, BOB, maker_side, lv[0], 1);
        rest(&mut ctx, CAROL, maker_side, lv[1], 1);
        rest(&mut ctx, BOB, maker_side, lv[2], 1);
        let admin_before = admin_wallet(&mut ctx);
        let fee_total_before = fee_total(&mut ctx);

        let trades = take(&mut ctx, ALICE, side, 3);

        assert_eq!(trades.len(), 3, "side {side}: three fills");
        assert_eq!(
            trades.iter().map(|t| t.price).collect::<Vec<_>>(),
            lv.to_vec(),
            "side {side}: the walk takes the touch first"
        );
        let charged = fee(lv.iter().map(|&p| notional(p, 1)).sum(), TAKER_PPM);
        assert_eq!(charged, if side == BUY { 30_004 } else { 29_995 });
        let shares: Vec<u64> = trades.iter().map(|t| t.takerFee).collect();
        assert_eq!(
            shares,
            if side == BUY {
                vec![10_000, 10_002, 10_002]
            } else {
                vec![9_999, 9_999, 9_997]
            },
            "side {side}: differences of cumulative floors"
        );
        assert_eq!(shares.iter().sum::<u64>(), charged, "side {side}: Σ shares == F");
        // Makers pay 0, so the admin's delta and the fee total are the taker fee alone.
        assert_eq!(admin_wallet(&mut ctx) - admin_before, charged as i64);
        assert_eq!(fee_total(&mut ctx) - fee_total_before, charged);

        // The fixture discriminates: the independent per-fill floor loses a unit.
        let independent: u64 = lv.iter().map(|&p| fee(notional(p, 1), TAKER_PPM)).sum();
        assert_eq!(independent + 1, charged, "side {side}: fixture must truncate");
    }
}

/// A fill whose maker is K9-rejected is never recorded, so it consumes NO share: the next accepted
/// fill's share is taken against the running total of ACCEPTED fills only, and the shares still
/// sum to the fee charged on the accepted notional.
#[test]
fn a_rejected_maker_consumes_no_taker_fee_share() {
    let mut ctx = fresh();
    set_fees(&mut ctx, ALICE, 0, TAKER_PPM);
    // CAROL rests the touch with a wallet that cannot fund it at all; the mark then moves so far
    // against her that the fill would open her below maintenance → K9 cancels her order.
    let lv = levels(BUY);
    rest(&mut ctx, CAROL, SELL, lv[0], 1);
    rest(&mut ctx, BOB, SELL, lv[1], 1);
    rest(&mut ctx, BOB, SELL, lv[2], 1);
    let mut a = storage::load_account(&mut ctx, CAROL).unwrap();
    a.perp_wallet_balance = 0;
    storage::save_account(&mut ctx, CAROL, a, AccountUpdateReason::Adjustment).unwrap();
    let _ = take_logs(&mut ctx);
    let admin_before = admin_wallet(&mut ctx);

    let trades = take(&mut ctx, ALICE, BUY, 2);

    assert_eq!(
        trades.iter().map(|t| t.maker).collect::<Vec<_>>(),
        vec![BOB, BOB],
        "CAROL's unfunded opening short is K9-cancelled, the taker walks on"
    );
    // Shares over the ACCEPTED fills (lv[1], lv[2]) only:
    //   C1 = 100_015_000 → 10_001        C2 = 200_040_000 → 20_004 → share 10_003
    let shares: Vec<u64> = trades.iter().map(|t| t.takerFee).collect();
    assert_eq!(shares, vec![10_001, 10_003]);
    let charged = fee(notional(lv[1], 1) + notional(lv[2], 1), TAKER_PPM);
    assert_eq!(shares.iter().sum::<u64>(), charged);
    assert_eq!(admin_wallet(&mut ctx) - admin_before, charged as i64);
}

// ── Maker rebates ────────────────────────────────────────────────────────────────────────────

/// The cumulative-floor taker shares of fills with notionals `ns` at `ppm`, recomputed here.
fn shares(ns: &[u64], ppm: u64) -> Vec<u64> {
    let (mut c, mut prev) = (0u64, 0u64);
    ns.iter()
        .map(|&n| {
            c += n;
            let f = fee(c, ppm);
            let s = f - prev;
            prev = f;
            s
        })
        .collect()
}

/// `(ALICE, BOB, CAROL, DAVE, ADMIN)` gross wallets, the fee total and [`money`], before an act.
struct Before {
    wb: [i64; 5],
    fee_total: u64,
    money: i128,
}

const PARTIES: [Address; 5] = [ALICE, BOB, CAROL, DAVE, ADMIN];

fn before(ctx: &mut TestCtx) -> Before {
    Before {
        wb: PARTIES.map(|u| wb(ctx, u)),
        fee_total: fee_total(ctx),
        money: money(ctx),
    }
}

impl Before {
    /// `Δwb` per party, in `PARTIES` order.
    fn deltas(&self, ctx: &mut TestCtx) -> [i64; 5] {
        let now = PARTIES.map(|u| wb(ctx, u));
        core::array::from_fn(|i| now[i] - self.wb[i])
    }
}

/// `t ≥ |m|`: every rebate is paid IN FULL, per fill, at its own floor `floor(n_i·|m|/1e6)` — the cap
/// does not bind. A positive-rate maker in the same sweep pays as before. The fee recipient gets
/// `F − Σ rebates + Σ maker charges`, which is also exactly what the fee total grows by; the taker
/// pays `F`; and no unit is created or destroyed.
#[test]
fn a_rebate_is_paid_in_full_per_fill_when_the_taker_fee_covers_it() {
    for side in [BUY, SELL] {
        let mut ctx = fresh();
        set_fees(&mut ctx, ALICE, 0, 300);
        set_maker(&mut ctx, BOB, -100);
        set_maker(&mut ctx, CAROL, 200);
        let lv = levels(side);
        rest(&mut ctx, BOB, 1 - side, lv[0], 1);
        rest(&mut ctx, CAROL, 1 - side, lv[1], 1);
        rest(&mut ctx, BOB, 1 - side, lv[2], 1);
        let b = before(&mut ctx);

        let trades = take(&mut ctx, ALICE, side, 3);

        let n = lv.map(|p| notional(p, 1));
        let s = shares(&n, 300);
        let f = fee(n.iter().sum(), 300);
        let (r0, c1, r2) = (fee(n[0], 100), fee(n[1], 200), fee(n[2], 100));
        assert!(r0 < s[0] && r2 < s[2], "side {side}: fixture — the cap does not bind");
        assert_eq!(trades.iter().map(|t| t.takerFee).collect::<Vec<_>>(), s);
        assert_eq!(
            trades.iter().map(|t| (t.maker, t.makerFee)).collect::<Vec<_>>(),
            vec![(BOB, -(r0 as i64)), (CAROL, c1 as i64), (BOB, -(r2 as i64))],
            "side {side}: Trade.makerFee is signed — negative = the rebate credited"
        );
        let net = f - r0 - r2 + c1;
        assert_eq!(
            b.deltas(&mut ctx),
            [-(f as i64), (r0 + r2) as i64, -(c1 as i64), 0, net as i64],
            "side {side}: Δwb (ALICE, BOB, CAROL, DAVE, ADMIN)"
        );
        assert_eq!(fee_total(&mut ctx) - b.fee_total, net, "side {side}: the fee total is NET");
        assert_eq!(money(&mut ctx), b.money, "side {side}: conservation");
    }
}

/// `|m| = t`: the rebate is still each fill's own floor, so the fee recipient keeps exactly the
/// truncation residue `F − Σ floor(n_i·t/1e6)` — here 1 unit — and never less than 0.
#[test]
fn at_equal_rates_the_fee_recipient_keeps_only_the_truncation_residue() {
    for side in [BUY, SELL] {
        let mut ctx = fresh();
        set_fees(&mut ctx, ALICE, 0, 100);
        set_maker(&mut ctx, BOB, -100);
        let lv = levels(side);
        for p in lv {
            rest(&mut ctx, BOB, 1 - side, p, 1);
        }
        let b = before(&mut ctx);
        let trades = take(&mut ctx, ALICE, side, 3);
        let n = lv.map(|p| notional(p, 1));
        let rebates: Vec<i64> = n.iter().map(|&x| -(fee(x, 100) as i64)).collect();
        assert_eq!(trades.iter().map(|t| t.makerFee).collect::<Vec<_>>(), rebates);
        let d = b.deltas(&mut ctx);
        assert_eq!(d[4], 1, "side {side}: the admin keeps the one unit the floors dropped");
        assert_eq!(fee_total(&mut ctx) - b.fee_total, 1);
        assert_eq!(money(&mut ctx), b.money);
    }
}

/// `t < |m|`: the cap BINDS — each rebate is exactly its own fill's taker-fee share, so the maker
/// receives part of its rate and the fee recipient nets nothing from those fills. A positive-rate
/// maker's fill in between is untouched by the cap: its share stays with the fee recipient, on top
/// of the commission that maker pays. No path debits the fee recipient.
#[test]
fn the_cap_binds_when_the_taker_fee_is_smaller_than_the_rebate() {
    for side in [BUY, SELL] {
        let mut ctx = fresh();
        set_fees(&mut ctx, ALICE, 0, 100);
        set_maker(&mut ctx, BOB, -300);
        set_maker(&mut ctx, DAVE, 200);
        set_maker(&mut ctx, CAROL, -300);
        let lv = levels(side);
        rest(&mut ctx, BOB, 1 - side, lv[0], 1);
        rest(&mut ctx, DAVE, 1 - side, lv[1], 1);
        rest(&mut ctx, CAROL, 1 - side, lv[2], 1);
        let b = before(&mut ctx);

        let trades = take(&mut ctx, ALICE, side, 3);

        let n = lv.map(|p| notional(p, 1));
        let s = shares(&n, 100);
        let c1 = fee(n[1], 200);
        assert!(fee(n[0], 300) > s[0] && fee(n[2], 300) > s[2], "fixture: the cap binds");
        assert_eq!(
            trades.iter().map(|t| (t.maker, t.makerFee)).collect::<Vec<_>>(),
            vec![(BOB, -(s[0] as i64)), (DAVE, c1 as i64), (CAROL, -(s[2] as i64))],
            "side {side}: rebate_i = share_i"
        );
        let f: u64 = s.iter().sum();
        assert_eq!(
            b.deltas(&mut ctx),
            [-(f as i64), s[0] as i64, s[2] as i64, -(c1 as i64), (s[1] + c1) as i64],
            "side {side}: Δwb (ALICE, BOB, CAROL, DAVE, ADMIN)"
        );
        assert_eq!(fee_total(&mut ctx) - b.fee_total, s[1] + c1);
        assert_eq!(money(&mut ctx), b.money);
    }
}

/// A zero-fee taker funds no rebate: every share is 0, so a negative-rate maker it trades with
/// receives 0 (`Trade.makerFee == 0`) while a positive-rate maker still pays.
#[test]
fn a_zero_fee_taker_earns_its_makers_no_rebate() {
    for side in [BUY, SELL] {
        let mut ctx = fresh();
        set_maker(&mut ctx, BOB, -100);
        set_maker(&mut ctx, CAROL, 200);
        let lv = levels(side);
        rest(&mut ctx, BOB, 1 - side, lv[0], 1);
        rest(&mut ctx, CAROL, 1 - side, lv[1], 1);
        let b = before(&mut ctx);
        let trades = take(&mut ctx, ALICE, side, 2);
        let c1 = fee(notional(lv[1], 1), 200);
        assert_eq!(
            trades.iter().map(|t| (t.takerFee, t.makerFee)).collect::<Vec<_>>(),
            vec![(0, 0), (0, c1 as i64)]
        );
        assert_eq!(b.deltas(&mut ctx), [0, 0, -(c1 as i64), 0, c1 as i64]);
        assert_eq!(money(&mut ctx), b.money);
    }
}

/// A LIQUIDATION close waives its taker fee, so every share in it is 0 and every rebate with it:
/// the negative-rate maker it trades with gets 0, the positive-rate maker still pays, and the fee
/// recipient's intake is that commission alone.
#[test]
fn a_liquidation_close_pays_no_rebate_while_positive_makers_still_pay() {
    let mut ctx = fresh();
    // DAVE long 2 at $100, 5x ($40 margin): under maintenance at an $85 mark.
    seed_pos(&mut ctx, DAVE, 2, 5);
    set_fees(&mut ctx, DAVE, 0, 300); // a real taker rate — the liquidation close waives it
    set_maker(&mut ctx, BOB, -100);
    set_maker(&mut ctx, CAROL, 200);
    let (p1, p2) = (85_005_000, 84_995_000);
    rest(&mut ctx, BOB, BUY, p1, 1);
    rest(&mut ctx, CAROL, BUY, p2, 1);
    let b = before(&mut ctx);

    let logs = tick(&mut ctx, 85_000_000);

    assert_eq!(
        evs::<Liquidation>(&logs).iter().map(|e| e.user).collect::<Vec<_>>(),
        vec![DAVE],
        "the tick liquidates DAVE"
    );
    let trades = evs::<Trade>(&logs);
    let c = fee(notional(p2, 1), 200);
    assert_eq!(
        trades.iter().map(|t| (t.taker, t.maker, t.takerFee, t.makerFee)).collect::<Vec<_>>(),
        vec![(DAVE, BOB, 0, 0), (DAVE, CAROL, 0, c as i64)],
        "no taker fee, so no rebate; CAROL's commission is unchanged"
    );
    assert_eq!(position(&mut ctx, DAVE).amount, 0);
    let d = b.deltas(&mut ctx);
    assert_eq!((d[1], d[2], d[4]), (0, -(c as i64), c as i64), "Δwb (BOB, CAROL, ADMIN)");
    assert_eq!(fee_total(&mut ctx) - b.fee_total, c);
}

/// A rebate on a PURE FULL CLOSE lands in the maker's perp wallet: the position ends flat with
/// zero margin (a margin credit would strand money on a flat position — `save_position` asserts it
/// cannot happen), and the wallet receives released margin + realized PnL + the rebate.
#[test]
fn a_rebate_on_a_pure_full_close_lands_in_the_wallet() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, BOB, -1, 1); // short 1 at $100, $100 margin
    set_fees(&mut ctx, ALICE, 0, 300);
    set_maker(&mut ctx, BOB, -100);
    let p = levels(SELL)[0]; // 99_995_000: BOB's bid closes his short $0.005 below entry
    rest(&mut ctx, BOB, BUY, p, 1);
    let w0 = wallet(&mut ctx, BOB);
    let admin0 = admin_wallet(&mut ctx);

    let trades = take(&mut ctx, ALICE, SELL, 1);

    let r = fee(notional(p, 1), 100);
    assert_eq!((trades[0].maker, trades[0].makerFee, trades[0].makerRealizedPnl), (BOB, -(r as i64), 5_000));
    let pos = position(&mut ctx, BOB);
    assert_eq!((pos.amount, pos.margin), (0, 0), "flat, and no margin left on it");
    assert_eq!(wallet(&mut ctx, BOB) - w0, MARK as i64 + 5_000 + r as i64);
    assert_eq!(admin_wallet(&mut ctx) - admin0, (fee(notional(p, 1), 300) - r) as i64);
}

/// The rebate is credited only AFTER the fill is decided, so it is neither cash at hand for the
/// fill's M1 cap nor an input to the K9 verdict.
///
/// BOB rests a $100.005 sell (an opening short at leverage 1) and his wallet then falls to `W`.
/// At the $100 mark his K9 value is `5_000 + margin` against a maintenance margin of
/// `floor(1e8 / 6) = 16_666_666`, and M1 funds `margin = W`:
/// * `W = 16_661_666` → exactly at maintenance: the fill happens, the silo holds ALL of `W` and
///   not a unit more, and the wallet holds exactly the rebate (10_000) — had the rebate counted
///   as cash at hand the silo would hold `W + 10_000`;
/// * `W = 16_661_665` → one unit short: K9 cancels the maker and nothing is paid — had the
///   rebate counted toward the margin, the fill would have passed.
#[test]
fn a_rebate_is_neither_cash_at_hand_for_the_m1_cap_nor_an_input_to_k9() {
    let p = levels(BUY)[0];
    let r = fee(notional(p, 1), 100);
    assert_eq!(r, 10_000);
    for (w, fills) in [(16_661_666i64, true), (16_661_665, false)] {
        let mut ctx = fresh();
        set_fees(&mut ctx, ALICE, 0, 300);
        set_maker(&mut ctx, BOB, -100);
        rest(&mut ctx, BOB, SELL, p, 1);
        set_wallet(&mut ctx, BOB, w);
        let admin0 = admin_wallet(&mut ctx);

        let (_, logs) = ok(&mut ctx, ALICE, &place(BUY, 0, 1, MKT));

        let pos = position(&mut ctx, BOB);
        if fills {
            assert_eq!(evs::<Trade>(&logs)[0].makerFee, -(r as i64));
            assert_eq!((pos.amount, pos.margin), (-1, w), "M1 funded the silo with W exactly");
            assert_eq!(wallet(&mut ctx, BOB), r as i64, "the wallet holds only the rebate");
        } else {
            assert!(evs::<Trade>(&logs).is_empty(), "K9 refused the fill");
            let c = evs::<OrderCancelled>(&logs);
            assert_eq!((c.len(), c[0].user, c[0].reason), (1, BOB, 2), "MakerInsolvent");
            assert_eq!((pos.amount, pos.margin), (0, 0));
            assert_eq!(wallet(&mut ctx, BOB), w, "no rebate without a fill");
            assert_eq!(admin_wallet(&mut ctx), admin0);
        }
    }
}

/// A partially filled order is rebated PER FILL at its own floor — not by the whole-order difference
/// a positive rate uses, which can hand a later fill the earlier fills' truncation (one unit more
/// than its own floor). One $100.005 order of 3, taken 1 at a time by three separate orders:
/// a −100 ppm maker gets `10_000` on each fill; a +100 ppm maker pays `10_000, 10_001, 10_000`
/// (the difference form, unchanged).
#[test]
fn a_partially_filled_order_is_rebated_per_fill_on_its_own_floor() {
    let p = levels(BUY)[0];
    for (rate, want) in [(-100i64, [-10_000i64, -10_000, -10_000]), (100, [10_000, 10_001, 10_000])] {
        let mut ctx = fresh();
        set_fees(&mut ctx, ALICE, 0, 300);
        set_maker(&mut ctx, BOB, rate);
        rest(&mut ctx, BOB, SELL, p, 3);
        let got: Vec<i64> = (0..3).map(|_| take(&mut ctx, ALICE, BUY, 1)[0].makerFee).collect();
        assert_eq!(got, want.to_vec(), "maker rate {rate}");
    }
}

/// A POSITIVE maker rate keeps the whole-order DIFFERENCE (`entry_fill_plan`, unchanged by the
/// rebate work), measured in balances, not only in `Trade.makerFee`. The golden `BusinessSnapshot`
/// cannot catch a regression here: none of its positive-rate partial fills truncates differently
/// under the two forms. This test is the guard. One order of 3 at the touch, +100 ppm, taken 1 lot
/// at a time by three separate takers:
///   * buy side, $100.005: whole-order fees 10_000 / 20_001 / 30_001 at 1 / 2 / 3 lots, so the
///     fills are charged `10_000, 10_001, 10_000` and their sum, 30_001, is the order's fee;
///   * sell side, $99.995: 9_999 / 19_999 / 29_998, so `9_999, 10_000, 9_999`, summing to 29_998.
/// A per-fill floor would charge 30_000 / 29_997 and lose one unit of the order's fee. The maker's
/// gross wallet falls by exactly the order's fee. The fee recipient and the fee total each grow by
/// the takers' fees plus that fee, and no unit is created or destroyed.
#[test]
fn a_positive_maker_pays_the_whole_order_difference_across_partial_fills() {
    for side in [BUY, SELL] {
        let p = levels(side)[0];
        let mut ctx = fresh();
        set_fees(&mut ctx, ALICE, 0, 300);
        set_maker(&mut ctx, BOB, 100);
        rest(&mut ctx, BOB, 1 - side, p, 3);
        let b = before(&mut ctx);

        let trades: Vec<Trade> = (0..3)
            .map(|_| {
                let t = take(&mut ctx, ALICE, side, 1);
                assert_eq!(t.len(), 1, "side {side}: one fill per take");
                t.into_iter().next().unwrap()
            })
            .collect();

        let whole = |q: u64| fee(notional(p, q), 100);
        let want = [whole(3) - whole(2), whole(2) - whole(1), whole(1)];
        let (order_fee, per_fill_floor) = (whole(3), 3 * fee(notional(p, 1), 100));
        assert_eq!(order_fee, per_fill_floor + 1, "side {side}: fixture — the two forms disagree");
        assert_eq!(
            want,
            if side == BUY { [10_000, 10_001, 10_000] } else { [9_999, 10_000, 9_999] },
            "side {side}: the hand-worked difference"
        );
        assert_eq!(
            trades.iter().map(|t| (t.maker, t.makerFee)).collect::<Vec<_>>(),
            want.iter().map(|&c| (BOB, c as i64)).collect::<Vec<_>>(),
            "side {side}: each fill pays the whole-order difference"
        );
        let taker_fees: u64 = trades.iter().map(|t| t.takerFee).sum();
        assert_eq!(taker_fees, 3 * fee(notional(p, 1), 300), "side {side}: single-fill takers");
        let net = taker_fees + order_fee;
        assert_eq!(
            b.deltas(&mut ctx),
            [-(taker_fees as i64), -(order_fee as i64), 0, 0, net as i64],
            "side {side}: Δwb (ALICE, BOB, CAROL, DAVE, ADMIN)"
        );
        assert_eq!(fee_total(&mut ctx) - b.fee_total, net, "side {side}: the fee total");
        assert_eq!(money(&mut ctx), b.money, "side {side}: conservation");
    }
}

/// A FOK taker still self-trades (STP does not apply to it), so one account is both legs: it pays
/// the taker fee and is credited its own rebate, and the fee recipient nets `F − rebate ≥ 0`.
#[test]
fn a_fok_self_trade_nets_the_fee_recipient_the_taker_fee_minus_the_rebate() {
    let mut ctx = fresh();
    set_bound(&mut ctx, 100);
    set_fees(&mut ctx, ALICE, -100, 300);
    let p = levels(BUY)[0];
    rest(&mut ctx, ALICE, SELL, p, 1);
    let b = before(&mut ctx);
    let fok = placeOrderCall {
        marketId: MARKET_ID,
        side: BUY,
        price: p,
        quantity: 1,
        orderType: LIMIT,
        tif: 2,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode();
    let (_, logs) = ok(&mut ctx, ALICE, &fok);
    let t = &evs::<Trade>(&logs)[0];
    let (f, r) = (fee(notional(p, 1), 300), fee(notional(p, 1), 100));
    assert_eq!((t.taker, t.maker, t.takerFee, t.makerFee), (ALICE, ALICE, f, -(r as i64)));
    assert_eq!(position(&mut ctx, ALICE).margin, 0);
    let d = b.deltas(&mut ctx);
    assert_eq!((d[0], d[4]), (-((f - r) as i64), (f - r) as i64));
    assert_eq!(money(&mut ctx), b.money);
}

/// STP is by address only, so a master and its sub-account trade with each other normally. They
/// share rates (push-down), and each fill's net protocol fee is `F − rebate ≥ 0` — exactly 0 at
/// `|m| = t` on a fill whose two floors agree, which is the documented free-self-dealing edge.
#[test]
fn a_master_and_its_sub_trade_at_a_non_negative_net_fee() {
    let p = levels(BUY)[0];
    for (m, t) in [(-100i64, 300u64), (-100, 100)] {
        let mut ctx = fresh();
        ok(&mut ctx, BOB, &createSubAccountCall { name: "mm".into() }.abi_encode());
        let sub = derive_sub_account(BOB, 0);
        set_wallet(&mut ctx, sub, WALLET);
        set_bound(&mut ctx, 100);
        set_fees(&mut ctx, BOB, m, t);
        let inner = place(SELL, p, 1, LIMIT);
        ok(
            &mut ctx,
            BOB,
            &asSubAccountCall {
                subAccount: sub,
                data: inner.into(),
            }
            .abi_encode(),
        );
        let admin0 = admin_wallet(&mut ctx);
        let family0 = wb(&mut ctx, BOB) + wb(&mut ctx, sub);

        let trades = take(&mut ctx, BOB, BUY, 1);

        let (f, r) = (fee(notional(p, 1), t), fee(notional(p, 1), m.unsigned_abs()));
        assert_eq!(
            (trades[0].taker, trades[0].maker, trades[0].takerFee, trades[0].makerFee),
            (BOB, sub, f, -(r as i64))
        );
        let net = admin_wallet(&mut ctx) - admin0;
        assert_eq!(net, (f - r) as i64);
        assert!(net >= 0);
        assert_eq!(wb(&mut ctx, BOB) + wb(&mut ctx, sub) - family0, -net, "(m, t) = ({m}, {t})");
    }
}

/// The cap is PER MATCH. Each batch item and each fired conditional is its own `match_order`, so
/// its rebates are capped by ITS OWN taker shares — a later item's single fill gets its own floor
/// as its share, not a difference against an earlier item's notional.
#[test]
fn each_batch_item_and_each_fired_conditional_is_capped_by_its_own_match() {
    // Batch: two market buys, one fill each, against a −300 maker with a 100 ppm taker → the cap
    // binds, so each rebate IS its item's share: floor(n_k · 100 / 1e6), 10_000 and 10_001. A
    // cumulative share across the batch would give the second 10_002.
    let mut ctx = fresh();
    set_fees(&mut ctx, ALICE, 0, 100);
    set_maker(&mut ctx, BOB, -300);
    let lv = levels(BUY);
    rest(&mut ctx, BOB, SELL, lv[0], 1);
    rest(&mut ctx, BOB, SELL, lv[1], 1);
    let b = before(&mut ctx);
    let item = || PlaceItem {
        marketId: MARKET_ID,
        side: BUY,
        price: 0,
        quantity: 1,
        orderType: MKT,
        tif: GTC,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    };
    let (_, logs) = ok(&mut ctx, ALICE, &batchPlaceOrdersCall { orders: vec![item(), item()] }.abi_encode());
    let own = [fee(notional(lv[0], 1), 100), fee(notional(lv[1], 1), 100)];
    assert_eq!(own, [10_000, 10_001]);
    assert_eq!(
        evs::<Trade>(&logs).iter().map(|t| (t.takerFee, t.makerFee)).collect::<Vec<_>>(),
        vec![(own[0], -(own[0] as i64)), (own[1], -(own[1] as i64))]
    );
    assert_eq!(b.deltas(&mut ctx)[4], 0, "the fee recipient nets nothing; is never debited");
    assert_eq!(money(&mut ctx), b.money);

    // A fired conditional: ALICE's reduce-only STOP_MARKET buy (closing her short — this build
    // takes only reduce-only / closePosition conditionals) fires on the tick and takes BOB's
    // rebate order.
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, -1, 1);
    set_fees(&mut ctx, ALICE, 0, 100);
    set_maker(&mut ctx, BOB, -300);
    let p = 107_005_000;
    rest(&mut ctx, BOB, SELL, p, 1);
    ok(
        &mut ctx,
        ALICE,
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: BUY,
            orderType: 1, // STOP_MARKET
            triggerPrice: 105_000_000,
            price: 0,
            quantity: 1,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: 0b0001, // reduceOnly
        }
        .abi_encode(),
    );
    let b = before(&mut ctx);
    let logs = tick(&mut ctx, 106_000_000);
    let s = fee(notional(p, 1), 100);
    assert_eq!(
        evs::<Trade>(&logs).iter().map(|t| (t.taker, t.takerFee, t.makerFee)).collect::<Vec<_>>(),
        vec![(ALICE, s, -(s as i64))],
        "the fired order's own share caps the rebate"
    );
    assert_eq!(position(&mut ctx, ALICE).amount, 0, "the conditional closed ALICE's short");
    assert_eq!(b.deltas(&mut ctx)[4], 0);
}

/// A resting order keeps the maker rate it SNAPSHOTTED: lowering `R` to 0 and then moving its
/// owner's rate to a positive one changes neither — it is still rebated at the old rate (and still
/// capped by its fill's share) — while the owner's NEXT order pays the new rate.
#[test]
fn a_resting_order_keeps_its_snapshotted_rebate_after_r_and_the_rate_change() {
    let mut ctx = fresh();
    set_fees(&mut ctx, ALICE, 0, 300);
    set_maker(&mut ctx, BOB, -150);
    let lv = levels(BUY);
    rest(&mut ctx, BOB, SELL, lv[0], 1);
    set_bound(&mut ctx, 0);
    set_fees(&mut ctx, BOB, 200, 0);
    rest(&mut ctx, BOB, SELL, lv[1], 1);

    let trades = take(&mut ctx, ALICE, BUY, 2);

    let (n0, n1) = (notional(lv[0], 1), notional(lv[1], 1));
    assert_eq!(
        trades.iter().map(|t| t.makerFee).collect::<Vec<_>>(),
        vec![-(fee(n0, 150) as i64), fee(n1, 200) as i64]
    );
}
