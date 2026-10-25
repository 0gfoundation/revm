//! Margin type (cross P1): the gate and the three reporting surfaces — every call driven through
//! the full shell (`run_perp_dex_call`), so gas, the EOA gate and the commit-only #23 tripwire (a
//! `debug_assert` on any write-then-revert) are in the loop.
use alloy_sol_types::{SolCall, SolError, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, Log, U256};

use crate::{
    interface::IPerpDex::{
        getAccountCall, getPositionRiskCall, getSymbolConfigCall, placeOrderCall, AccountPosition,
        PositionChanged,
    },
    run_perp_dex_call, storage,
    types::{AccountUpdateReason, MarginTiers, Market},
    PerpOutput, PERP_DEX_ADDRESS, USDC_ADDRESS,
};

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

const MARKET_ID: u64 = 1;
/// 1 tick = $1 at price_decimals = 9.
const TICK: u64 = 1_000_000_000;
const PRICE: u64 = 100 * TICK;
/// 0.01 BTC at base_decimals = 8; `calc_value(PRICE, QTY)` = 1 USDC.
const QTY: u64 = 1_000_000;
const USDC: u64 = 1_000_000;

const CROSSED: u8 = 0;
const ISOLATED: u8 = 1;

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

// ── Fixture ───────────────────────────────────────────────────────────────────

fn make_ctx() -> TestCtx {
    let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(
        &mut ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 8,
            price_decimals: 9,
            tick_size: TICK,
            step_size: QTY,
            min_quantity: QTY,
            max_quantity: QTY * 1_000,
            max_price: PRICE * 1_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 0,
            mark_price: PRICE,
            tiers: MarginTiers::default(),
            basis_mode: 0,
            deferred_work: 0,
        },
    )
    .unwrap();
    for user in [ALICE, BOB] {
        let mut a = storage::load_account(&mut ctx, user).unwrap();
        a.perp_wallet_balance = (100 * USDC) as i64;
        storage::save_account(&mut ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
    }
    storage::flush_account_snapshots(&mut ctx).unwrap();
    // Fixture writes are not under test: drop their logs.
    let _ = JournalTr::take_logs(ctx.journal_mut());
    ctx
}

fn call(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, 10_000_000, caller, U256::ZERO, false, ctx)
        .expect("a perp call must not hard-fail")
}

fn revert_reason(out: &PerpOutput) -> String {
    alloy_sol_types::Revert::abi_decode(&out.bytes)
        .map(|r| r.reason)
        .unwrap_or_else(|_| format!("<undecodable {:?}>", out.bytes))
}

fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    let out = call(ctx, caller, input);
    assert!(!out.reverted, "call reverted: {}", revert_reason(&out));
    out
}

fn order(side: u8, tif: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side,
        price: PRICE,
        quantity: QTY,
        orderType: 0,
        tif,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode()
}

/// `taker` buys (`side` 0) or sells (1) one lot from `maker`'s resting GTC at `PRICE`.
fn trade(ctx: &mut TestCtx, taker: Address, maker: Address, side: u8) {
    ok(ctx, maker, &order(1 - side, 0));
    ok(ctx, taker, &order(side, 1));
}

/// Every `PositionChanged` in `logs`, decoded.
fn position_rows(logs: &[Log]) -> Vec<PositionChanged> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&PositionChanged::SIGNATURE_HASH))
        .map(|l| PositionChanged::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .collect()
}

fn symbol_config_type(ctx: &mut TestCtx, user: Address) -> u8 {
    let out = ok(ctx, ADMIN, &getSymbolConfigCall { user, marketId: MARKET_ID }.abi_encode());
    getSymbolConfigCall::abi_decode_returns(&out.bytes).unwrap().marginType
}

fn risk_row(ctx: &mut TestCtx, user: Address) -> AccountPosition {
    let out = ok(ctx, ADMIN, &getPositionRiskCall { user, marketId: MARKET_ID }.abi_encode());
    getPositionRiskCall::abi_decode_returns(&out.bytes).unwrap()
}

fn account_rows(ctx: &mut TestCtx, user: Address) -> Vec<AccountPosition> {
    let out = ok(ctx, ADMIN, &getAccountCall { user }.abi_encode());
    getAccountCall::abi_decode_returns(&out.bytes).unwrap().positions
}

/// Stores `margin_type` on `user`'s (flat, order-free) record through the type-only route, with
/// no selector in between — the reporting is under test here, not the setter.
fn store_type(ctx: &mut TestCtx, user: Address, margin_type: u8) {
    let mut p = storage::load_position(ctx, user, MARKET_ID).unwrap();
    p.margin_type = margin_type;
    storage::save_position_margin_type_only(ctx, user, MARKET_ID, p).unwrap();
}

/// The type every surface reports for `user`: getSymbolConfig, getPositionRisk, and (when the user
/// has a row there) getAccount — asserted equal to each other.
fn reported_type(ctx: &mut TestCtx, user: Address) -> u8 {
    let t = symbol_config_type(ctx, user);
    assert_eq!(risk_row(ctx, user).marginType, t, "getPositionRisk vs getSymbolConfig");
    for row in account_rows(ctx, user) {
        assert_eq!(row.marginType, t, "getAccount row vs getSymbolConfig");
    }
    t
}

// ── The gate and the reporting surfaces ───────────────────────────────────────

/// Gated off (production): every surface reports ISOLATED, whatever the record stores — the
/// default 0 (CROSSED, Binance's default) included — and a fill's `PositionChanged` too.
#[test]
fn gated_off_every_surface_reports_isolated_whatever_is_stored() {
    let mut ctx = make_ctx();
    assert!(!crate::margin_type::cross_margin_enabled(), "production ships cross margin OFF");
    assert_eq!(storage::load_position(&mut ctx, ALICE, MARKET_ID).unwrap().margin_type, CROSSED);
    assert_eq!(reported_type(&mut ctx, ALICE), ISOLATED, "a never-touched market");
    store_type(&mut ctx, BOB, ISOLATED);
    assert_eq!(reported_type(&mut ctx, BOB), ISOLATED);

    // ALICE stores CROSSED (the default), BOB ISOLATED: both report ISOLATED, on every surface.
    trade(&mut ctx, ALICE, BOB, 0);
    let rows = position_rows(&JournalTr::take_logs(ctx.journal_mut()));
    assert_eq!(rows.len(), 2, "one row per participant");
    assert!(rows.iter().all(|r| r.marginType == ISOLATED));
    assert_eq!(account_rows(&mut ctx, ALICE).len(), 1, "an open position has a getAccount row");
    assert_eq!(reported_type(&mut ctx, ALICE), ISOLATED, "an open position");
    assert_eq!(reported_type(&mut ctx, BOB), ISOLATED);
}

/// Enabled (the test override): every surface reports the STORED choice — CROSSED for a record
/// that never chose, ISOLATED once stored — and they agree with each other and with the event.
#[test]
fn with_cross_enabled_every_surface_reports_the_stored_choice() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    assert!(crate::margin_type::cross_margin_enabled());
    assert_eq!(reported_type(&mut ctx, ALICE), CROSSED, "Binance's default");

    store_type(&mut ctx, BOB, ISOLATED);
    assert_eq!(reported_type(&mut ctx, BOB), ISOLATED);

    trade(&mut ctx, ALICE, BOB, 0);
    let rows = position_rows(&JournalTr::take_logs(ctx.journal_mut()));
    let of = |u: Address| rows.iter().find(|r| r.user == u).unwrap().marginType;
    assert_eq!((of(ALICE), of(BOB)), (CROSSED, ISOLATED));
    assert_eq!(reported_type(&mut ctx, ALICE), CROSSED);
    assert_eq!(reported_type(&mut ctx, BOB), ISOLATED);
}

/// The override is per-thread and scoped to its guard: dropping it restores the production gate.
#[test]
fn the_test_override_ends_with_its_guard() {
    let mut ctx = make_ctx();
    {
        let _cross = crate::margin_type::test_gate::enable();
        assert_eq!(reported_type(&mut ctx, ALICE), CROSSED);
    }
    assert!(!crate::margin_type::cross_margin_enabled());
    assert_eq!(reported_type(&mut ctx, ALICE), ISOLATED);
}

/// A setting, like `leverage`: no fill writes it, so it survives opening, closing to flat and
/// reopening — the stored byte is carried through every `save_position`.
#[test]
fn the_stored_type_survives_open_close_and_reopen() {
    let mut ctx = make_ctx();
    store_type(&mut ctx, ALICE, ISOLATED);
    let stored = |ctx: &mut TestCtx| storage::load_position(ctx, ALICE, MARKET_ID).unwrap();

    trade(&mut ctx, ALICE, BOB, 0);
    assert_eq!(stored(&mut ctx).amount, QTY as i64);
    assert_eq!(stored(&mut ctx).margin_type, ISOLATED, "kept by the opening fill");
    trade(&mut ctx, ALICE, BOB, 1);
    assert_eq!(stored(&mut ctx).amount, 0);
    assert_eq!(stored(&mut ctx).margin_type, ISOLATED, "kept across flat");
    trade(&mut ctx, ALICE, BOB, 0);
    assert_eq!(stored(&mut ctx).margin_type, ISOLATED, "kept by the reopen");
    // BOB never chose: still the stored default through the same three fills.
    assert_eq!(storage::load_position(&mut ctx, BOB, MARKET_ID).unwrap().margin_type, CROSSED);
}
