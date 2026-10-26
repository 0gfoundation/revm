//! Margin type (cross P1): the gate and the three reporting surfaces — every call driven through
//! the full shell (`run_perp_dex_call`), so gas, the EOA gate and the commit-only #23 tripwire (a
//! `debug_assert` on any write-then-revert) are in the loop.
use alloy_sol_types::{SolCall, SolError, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, Log, U256};

use ed25519_dalek::{Signer, SigningKey};

use crate::{
    interface::IPerpDex::{
        asSubAccountCall, cancelAlgoOrderCall, cancelOrderCall, createSubAccountCall,
        getAccountCall, getPositionRiskCall, getSymbolConfigCall, placeAlgoOrderCall,
        placeOrderCall, setLeverageCall, setMarginTypeCall, setMarginTypeSignedCall,
        AccountBalanceChanged, AccountPosition, MarginTypeChanged, PositionChanged,
    },
    run_perp_dex_call, storage,
    types::{AccountUpdateReason, AlgoOrderType, ApiKey, MarginTiers, Market},
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
            cumulative_funding_index: 0,
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

/// Gated off (the gate forced off, as before the flip): every surface reports ISOLATED, whatever
/// the record stores — the default 0 (CROSSED, Binance's default) included — and a fill's
/// `PositionChanged` too.
#[test]
fn gated_off_every_surface_reports_isolated_whatever_is_stored() {
    let mut ctx = make_ctx();
    let _off = crate::margin_type::test_gate::disable();
    assert!(!crate::margin_type::cross_margin_enabled(), "forced off");
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

/// Enabled (the real gate): every surface reports the STORED choice — CROSSED for a record that
/// never chose, ISOLATED once stored — and they agree with each other and with the event.
#[test]
fn with_cross_enabled_every_surface_reports_the_stored_choice() {
    let mut ctx = make_ctx();
    assert!(crate::margin_type::cross_margin_enabled());
    assert_eq!(reported_type(&mut ctx, ALICE), CROSSED, "Binance's default");

    store_type(&mut ctx, BOB, ISOLATED);
    assert_eq!(reported_type(&mut ctx, BOB), ISOLATED);

    // ALICE's CROSSED short is seeded (see `seed_cross`); the fill CLOSES it against BOB's opening
    // ISOLATED short, so both rows of one fill carry their own type.
    seed_cross(&mut ctx, ALICE, -(QTY as i64));
    assert_eq!(reported_type(&mut ctx, ALICE), CROSSED, "an open CROSSED position");
    trade(&mut ctx, ALICE, BOB, 0);
    let rows = position_rows(&JournalTr::take_logs(ctx.journal_mut()));
    let of = |u: Address| rows.iter().find(|r| r.user == u).unwrap().marginType;
    assert_eq!((of(ALICE), of(BOB)), (CROSSED, ISOLATED));
    assert_eq!(reported_type(&mut ctx, ALICE), CROSSED);
    assert_eq!(reported_type(&mut ctx, BOB), ISOLATED);
}

/// The gate overrides are per-thread, scoped to their guards, and nest: each guard restores what
/// was in force before it, and with none alive the production gate is back.
#[test]
fn the_test_overrides_end_with_their_guards() {
    use crate::margin_type::{cross_margin_enabled, test_gate};
    let mut ctx = make_ctx();
    let production = crate::types::CROSS_MARGIN_ENABLED;
    let production_type = if production { CROSSED } else { ISOLATED };
    assert_eq!(cross_margin_enabled(), production);
    {
        let _off = test_gate::disable();
        assert_eq!(reported_type(&mut ctx, ALICE), ISOLATED);
        {
            let _on = test_gate::enable();
            assert_eq!(reported_type(&mut ctx, ALICE), CROSSED);
        }
        assert_eq!(reported_type(&mut ctx, ALICE), ISOLATED, "the inner guard restored the outer");
    }
    assert_eq!(cross_margin_enabled(), production);
    assert_eq!(reported_type(&mut ctx, ALICE), production_type);
}

/// The fixture hook `isolated_by_default` moves the STORED byte of a never-written record, and
/// nothing else: the gate is untouched, a record already written keeps what it stores, and a
/// position opened under it stores ISOLATED — what `setMarginType(ISOLATED)` on the flat market
/// would have left.
#[test]
fn the_isolated_by_default_hook_moves_only_the_never_set_byte() {
    use crate::margin_type::{cross_margin_enabled, test_gate};
    let mut ctx = make_ctx();
    let _on = test_gate::enable();
    store_type(&mut ctx, BOB, CROSSED); // written: CROSSED by choice
    assert_eq!(stored(&mut ctx, ALICE).margin_type, CROSSED, "Binance's default");

    test_gate::isolated_by_default();
    assert!(cross_margin_enabled(), "the gate is untouched");
    assert_eq!(stored(&mut ctx, ALICE).margin_type, ISOLATED, "a never-written record");
    assert_eq!(reported_type(&mut ctx, ALICE), ISOLATED);
    assert_eq!(reported_type(&mut ctx, BOB), CROSSED, "a written record keeps its byte");

    trade(&mut ctx, ALICE, BOB, 0);
    assert_eq!(stored(&mut ctx, ALICE).margin_type, ISOLATED, "stored by the opening fill");
    assert!(stored(&mut ctx, ALICE).margin > 0, "an isolated silo");
    assert_eq!(stored(&mut ctx, BOB).margin, 0, "BOB's short is CROSSED: no silo");
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

// ── setMarginType: helpers ────────────────────────────────────────────────────

const E_NO_NEED: &str = "setMarginType: -4046 No need to change margin type.";
const E_OPEN_ORDERS: &str =
    "setMarginType: -4047 Margin type cannot be changed if there exists open orders.";
const E_POSITION: &str =
    "setMarginType: -4048 Margin type cannot be changed if there exists position.";
const E_GATE: &str = "setMarginType: CROSSED margin is not enabled yet";
const E_INVALID: &str = "setMarginType: invalid marginType (0 = CROSSED, 1 = ISOLATED)";
const E_UNKNOWN_MARKET: &str = "setMarginType: unknown market";

/// The chain id `CfgEnv::default()` reports — every signed message is bound to it.
const CHAIN_ID: u64 = 1;
/// == the default block timestamp.
const SIGNED_TS: u64 = 1;
const SIGNED_RECV: u64 = 60;

fn set_type(market_id: u64, margin_type: u8) -> Vec<u8> {
    setMarginTypeCall { marketId: market_id, marginType: margin_type }.abi_encode()
}

/// A rejected call: reverts with exactly `expect`, wrote NOTHING, logged nothing (commit-only #23,
/// asserted directly as well as by the shell's tripwire).
fn rejects(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) -> PerpOutput {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let writes = JournalTr::perp_write_count(ctx.journal_mut());
    let out = call(ctx, caller, input);
    assert!(out.reverted, "expected {expect:?}, the call succeeded");
    assert_eq!(revert_reason(&out), expect);
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes,
        "a rejected call wrote perp state ({expect})"
    );
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty(), "a rejected call logged");
    out
}

/// A successful switch: exactly ONE perp write (the position record) and exactly ONE log, the
/// `MarginTypeChanged` — no `AccountBalanceChanged`, no `PositionChanged`.
fn switches(ctx: &mut TestCtx, caller: Address, user: Address, input: &[u8], to: u8) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let writes = JournalTr::perp_write_count(ctx.journal_mut());
    let leverage = stored(ctx, user).leverage;
    let out = ok(ctx, caller, input);
    assert_eq!(out.gas_used, 30_000 + if caller == user { 0 } else { 5_000 });
    assert_eq!(JournalTr::perp_write_count(ctx.journal_mut()) - writes, 1, "one write");
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(logs.len(), 1, "MarginTypeChanged and nothing else");
    let e = MarginTypeChanged::decode_raw_log(logs[0].data.topics(), &logs[0].data.data).unwrap();
    assert_eq!((e.user, e.marketId, e.marginType), (user, MARKET_ID, to));
    assert_eq!(stored(ctx, user).margin_type, to, "the stored choice");
    assert_eq!(stored(ctx, user).leverage, leverage, "leverage is left alone");
    assert_eq!(reported_type(ctx, user), to, "and, enabled, the type in force");
}

fn stored(ctx: &mut TestCtx, user: Address) -> crate::types::PerpPosition {
    storage::load_position(ctx, user, MARKET_ID).unwrap()
}

/// Seeds `user` OFF-BOOK with `amount` (signed, base units) entered at `PRICE` and NO isolated
/// margin, keeping the record's stored settings — a CROSSED position as cross margin (P2) defines
/// one: its collateral is the cross wallet, so `pos.margin == 0` (`save_position` debug-asserts it
/// for every position CROSSED in force).
///
/// Gate-on tests open their CROSSED positions HERE and let real fills only CLOSE them: an OPENING
/// fill on a record CROSSED in force would still fund an isolated silo until the fill path gains
/// its cross branch, which is exactly the state that assertion forbids. Through the production
/// door (`save_position`), so the registry, the per-user market index and the open-position count
/// follow.
fn seed_cross(ctx: &mut TestCtx, user: Address, amount: i64) {
    let lots = amount / QTY as i64;
    let pos = crate::types::PerpPosition {
        amount,
        v_quote_balance: -lots * USDC as i64,
        margin: 0,
        ..stored(ctx, user)
    };
    storage::save_position(ctx, user, MARKET_ID, &pos, AccountUpdateReason::Adjustment).unwrap();
    // A planted fixture, not an event under test.
    storage::clear_account_snapshot_mark(ctx, user);
    let _ = JournalTr::take_logs(ctx.journal_mut());
}

/// `user` rests a GTC buy one tick under `PRICE` (no ask to cross); returns its id.
fn rest_bid(ctx: &mut TestCtx, user: Address) -> [u8; 32] {
    let out = ok(
        ctx,
        user,
        &placeOrderCall {
            marketId: MARKET_ID,
            side: 0,
            price: PRICE - TICK,
            quantity: QTY,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        }
        .abi_encode(),
    );
    out.bytes[..32].try_into().unwrap()
}

fn cancel(ctx: &mut TestCtx, user: Address, id: [u8; 32]) {
    ok(ctx, user, &cancelOrderCall { orderId: id.into(), marketId: MARKET_ID }.abi_encode());
}

/// A reduce-only conditional SELL: a STOP_MARKET under the mark, or a TRAILING_STOP_MARKET whose
/// activation price is above it (callback 1%). Returns the algo id.
fn place_conditional(ctx: &mut TestCtx, user: Address, trailing: bool) -> [u8; 32] {
    let (order_type, trigger, price) = if trailing {
        (AlgoOrderType::TrailingStopMarket as u8, PRICE + 10 * TICK, 100)
    } else {
        (AlgoOrderType::StopMarket as u8, PRICE - 10 * TICK, 0)
    };
    let out = ok(
        ctx,
        user,
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: 1,
            orderType: order_type,
            triggerPrice: trigger,
            price,
            quantity: QTY,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: 1, // reduceOnly
        }
        .abi_encode(),
    );
    out.bytes[..32].try_into().unwrap()
}

fn cancel_conditional(ctx: &mut TestCtx, user: Address, id: [u8; 32]) {
    ok(ctx, user, &cancelAlgoOrderCall { algoId: id.into(), marketId: MARKET_ID }.abi_encode());
}

fn register_key(ctx: &mut TestCtx, user: Address, sk: &SigningKey) {
    storage::save_api_key(ctx, user, 0, ApiKey { pubkey: sk.verifying_key().to_bytes(), expiry: 0 })
        .unwrap();
}

/// `setMarginTypeSigned` for ALICE, signed over the canonical 75-byte message.
fn signed(sk: &SigningKey, market_id: u64, margin_type: u8) -> Vec<u8> {
    signed_at(sk, market_id, margin_type, SIGNED_TS)
}

/// [`signed`] with the message's `timestamp`.
fn signed_at(sk: &SigningKey, market_id: u64, margin_type: u8, ts: u64) -> Vec<u8> {
    let mut msg = [0u8; 75];
    let mut c = crate::trading::write_signed_header(&mut msg, b"perpdex_v1_margintype", CHAIN_ID);
    c = crate::trading::put(&mut msg, c, ALICE.as_slice());
    c = crate::trading::put(&mut msg, c, &market_id.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[margin_type]);
    c = crate::trading::put(&mut msg, c, &ts.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[0]);
    assert_eq!(c, msg.len());
    setMarginTypeSignedCall {
        account: ALICE,
        marketId: market_id,
        marginType: margin_type,
        timestamp: ts,
        recvWindow: SIGNED_RECV,
        keyId: 0,
        signature: sk.sign(&msg).to_bytes().to_vec().into(),
    }
    .abi_encode()
}

/// A signed call refused AFTER its signature burn: reverts with `expect`, keeps ONLY the burn's
/// writes (sanctioned), logs nothing — and the same bytes are then a duplicate.
fn rejects_after_burn(ctx: &mut TestCtx, input: &[u8], expect: &str) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let writes = JournalTr::perp_write_count(ctx.journal_mut());
    let out = call(ctx, BOB, input);
    assert!(out.reverted, "expected {expect:?}, the call succeeded");
    assert_eq!(revert_reason(&out), expect);
    assert!(JournalTr::perp_write_count(ctx.journal_mut()) > writes, "the signature is spent");
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty(), "a rejected call logged");
    assert_eq!(stored(ctx, ALICE).margin_type, CROSSED, "and nothing else moved");
    rejects(
        ctx,
        BOB,
        input,
        "setMarginTypeSigned: duplicate signature (already submitted)",
    );
}

// ── setMarginType: gated off (production) ─────────────────────────────────────

/// The flip (cross P3, R8): production ships cross margin ON — no override in play — so a market
/// nobody switched is CROSSED on every surface and in its fills (no silo: the cross wallet carries
/// it), `setMarginType(CROSSED)` is `-4046`, and the switch goes to ISOLATED and back.
#[test]
fn production_ships_cross_margin_on_and_the_default_is_crossed() {
    let mut ctx = make_ctx();
    const { assert!(crate::types::CROSS_MARGIN_ENABLED) };
    assert_eq!(crate::margin_type::test_gate::forced(), None, "no override");
    assert!(crate::margin_type::cross_margin_enabled());
    assert_eq!(stored(&mut ctx, ALICE).margin_type, CROSSED, "the stored default");
    assert_eq!(reported_type(&mut ctx, ALICE), CROSSED, "a never-touched market");
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, CROSSED), E_NO_NEED);
    switches(&mut ctx, ALICE, ALICE, &set_type(MARKET_ID, ISOLATED), ISOLATED);
    switches(&mut ctx, ALICE, ALICE, &set_type(MARKET_ID, CROSSED), CROSSED);

    // A fill on two never-switched records: both rows CROSSED, neither position holds a silo.
    trade(&mut ctx, BOB, ALICE, 0);
    let rows = position_rows(&JournalTr::take_logs(ctx.journal_mut()));
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.marginType == CROSSED));
    for u in [ALICE, BOB] {
        assert_eq!(reported_type(&mut ctx, u), CROSSED);
        assert_ne!(stored(&mut ctx, u).amount, 0);
        assert_eq!(stored(&mut ctx, u).margin, 0, "a cross position has no silo");
    }
}

/// Gated off, the switch can change nothing: ISOLATED is already in force for every stored value
/// (`-4046`), CROSSED meets the gate — and every refusal writes nothing.
#[test]
fn gated_off_the_switch_refuses_every_request_and_writes_nothing() {
    let mut ctx = make_ctx();
    let _off = crate::margin_type::test_gate::disable();
    assert_eq!(stored(&mut ctx, ALICE).margin_type, CROSSED, "the stored default");
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, ISOLATED), E_NO_NEED);
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, CROSSED), E_GATE);
    store_type(&mut ctx, BOB, ISOLATED);
    rejects(&mut ctx, BOB, &set_type(MARKET_ID, ISOLATED), E_NO_NEED);
    rejects(&mut ctx, BOB, &set_type(MARKET_ID, CROSSED), E_GATE);
    // The gate is the LAST refusal: an open order or a position is reported first, as it will be
    // once the gate lifts.
    let bid = rest_bid(&mut ctx, ALICE);
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, CROSSED), E_OPEN_ORDERS);
    cancel(&mut ctx, ALICE, bid);
    trade(&mut ctx, ALICE, BOB, 0);
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, CROSSED), E_POSITION);
    assert_eq!(reported_type(&mut ctx, ALICE), ISOLATED);
}

/// Refusal 0 (the code) and refusal 1 (the market), in that order, before any load matters.
#[test]
fn an_invalid_code_and_an_unknown_market_are_refused_first() {
    let mut ctx = make_ctx();
    for bad in [2u8, 3, 255] {
        rejects(&mut ctx, ALICE, &set_type(MARKET_ID, bad), E_INVALID);
        rejects(&mut ctx, ALICE, &set_type(99, bad), E_INVALID);
    }
    rejects(&mut ctx, ALICE, &set_type(99, ISOLATED), E_UNKNOWN_MARKET);
    rejects(&mut ctx, ALICE, &set_type(99, CROSSED), E_UNKNOWN_MARKET);
    // Malformed calldata.
    rejects(&mut ctx, ALICE, &setMarginTypeCall::SELECTOR, "setMarginType: invalid calldata");
}

// ── setMarginType: enabled (the real gate) ────────────────────────────────────

/// The success path, both directions: one write, `MarginTypeChanged` only, the stored choice and
/// the type in force follow, leverage stays. Asking for the type in force again is `-4046`.
#[test]
fn with_cross_enabled_the_switch_succeeds_both_ways() {
    let mut ctx = make_ctx();
    ok(&mut ctx, ALICE, &setLeverageCall { marketId: MARKET_ID, leverage: 2 }.abi_encode());
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, CROSSED), E_NO_NEED);
    switches(&mut ctx, ALICE, ALICE, &set_type(MARKET_ID, ISOLATED), ISOLATED);
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, ISOLATED), E_NO_NEED);
    switches(&mut ctx, ALICE, ALICE, &set_type(MARKET_ID, CROSSED), CROSSED);
    assert_eq!(stored(&mut ctx, ALICE).leverage, 2);
    // A flat, order-free market the user switched in is still not in their index.
    assert!(account_rows(&mut ctx, ALICE).is_empty());
}

/// `-4047` for EVERY kind of open order — a resting book order, an untriggered conditional, and a
/// trailing stop left behind by a closed position (its expiry is lazy) — and each lifts once the
/// order is gone.
#[test]
fn with_cross_enabled_any_open_order_blocks_the_switch() {
    let mut ctx = make_ctx();
    let to_iso = set_type(MARKET_ID, ISOLATED);
    // The counterparty opens on every fill below; see `seed_cross`.
    store_type(&mut ctx, BOB, ISOLATED);

    let bid = rest_bid(&mut ctx, ALICE);
    rejects(&mut ctx, ALICE, &to_iso, E_OPEN_ORDERS);
    cancel(&mut ctx, ALICE, bid);

    let stop = place_conditional(&mut ctx, ALICE, false);
    rejects(&mut ctx, ALICE, &to_iso, E_OPEN_ORDERS);
    cancel_conditional(&mut ctx, ALICE, stop);

    // A trailing stop is position-bound: it needs ALICE's long to exist when placed. Seeded
    // CROSSED (see `seed_cross`); the fill below closes it.
    seed_cross(&mut ctx, ALICE, QTY as i64);
    let trailing = place_conditional(&mut ctx, ALICE, true);
    rejects(&mut ctx, ALICE, &to_iso, E_OPEN_ORDERS);
    trade(&mut ctx, ALICE, BOB, 1);
    assert_eq!(stored(&mut ctx, ALICE).amount, 0, "flat again");
    rejects(&mut ctx, ALICE, &to_iso, E_OPEN_ORDERS);
    cancel_conditional(&mut ctx, ALICE, trailing);

    switches(&mut ctx, ALICE, ALICE, &to_iso, ISOLATED);
}

/// Binance's order: `-4046` before `-4047` before `-4048`; a position alone is `-4048`.
#[test]
fn with_cross_enabled_the_refusals_come_in_binance_order() {
    let mut ctx = make_ctx();
    // A CROSSED long (see `seed_cross`).
    seed_cross(&mut ctx, ALICE, QTY as i64);
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, ISOLATED), E_POSITION);
    let bid = rest_bid(&mut ctx, ALICE);
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, ISOLATED), E_OPEN_ORDERS);
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, CROSSED), E_NO_NEED);
    cancel(&mut ctx, ALICE, bid);
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, ISOLATED), E_POSITION);
}

/// Set once, the choice outlives the position like `leverage` does: it holds through an open, a
/// close to flat and a reopen; switching is refused while the position is open and allowed again
/// once it is flat.
#[test]
fn with_cross_enabled_the_choice_persists_across_close_and_reopen() {
    let mut ctx = make_ctx();
    switches(&mut ctx, ALICE, ALICE, &set_type(MARKET_ID, ISOLATED), ISOLATED);
    // The counterparty opens on two of the four fills below; see `seed_cross`.
    store_type(&mut ctx, BOB, ISOLATED);

    trade(&mut ctx, ALICE, BOB, 0);
    let rows = position_rows(&JournalTr::take_logs(ctx.journal_mut()));
    assert_eq!(rows.iter().find(|r| r.user == ALICE).unwrap().marginType, ISOLATED);
    rejects(&mut ctx, ALICE, &set_type(MARKET_ID, CROSSED), E_POSITION);

    trade(&mut ctx, ALICE, BOB, 1);
    assert_eq!(stored(&mut ctx, ALICE).amount, 0);
    assert_eq!(reported_type(&mut ctx, ALICE), ISOLATED, "kept across flat");
    trade(&mut ctx, ALICE, BOB, 0);
    assert_eq!(reported_type(&mut ctx, ALICE), ISOLATED, "kept by the reopen");
    trade(&mut ctx, ALICE, BOB, 1);
    switches(&mut ctx, ALICE, ALICE, &set_type(MARKET_ID, CROSSED), CROSSED);
    // BOB took the other side of every fill and kept his own choice through all of them; a record
    // that never chose is still Binance's default.
    assert_eq!(reported_type(&mut ctx, BOB), ISOLATED);
    assert_eq!(reported_type(&mut ctx, ADMIN), CROSSED);
}

// ── setMarginTypeSigned ───────────────────────────────────────────────────────

/// Enabled: the signed switch lands for `account` (any relayer may submit it) with the same event,
/// and its signature is single-use.
#[test]
fn the_signed_switch_succeeds_once_for_the_signing_account() {
    let mut ctx = make_ctx();
    let sk = SigningKey::from_bytes(&[0x5a; 32]);
    register_key(&mut ctx, ALICE, &sk);
    let input = signed(&sk, MARKET_ID, ISOLATED);

    let _ = JournalTr::take_logs(ctx.journal_mut());
    let out = ok(&mut ctx, BOB, &input);
    assert_eq!(out.gas_used, 30_000);
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(logs.len(), 1);
    let e = MarginTypeChanged::decode_raw_log(logs[0].data.topics(), &logs[0].data.data).unwrap();
    assert_eq!((e.user, e.marketId, e.marginType), (ALICE, MARKET_ID, ISOLATED));
    assert_eq!(stored(&mut ctx, ALICE).margin_type, ISOLATED);
    assert_eq!(stored(&mut ctx, BOB).margin_type, CROSSED, "the relayer is not the subject");

    rejects(&mut ctx, BOB, &input, "setMarginTypeSigned: duplicate signature (already submitted)");
}

/// Refusals that are permanent for the message come BEFORE the burn — nothing written, and the
/// signature stays unspent (resubmitting it is the same refusal, not a duplicate).
#[test]
fn signed_refusals_permanent_for_the_message_precede_the_burn() {
    let mut ctx = make_ctx();
    let sk = SigningKey::from_bytes(&[0x5a; 32]);
    register_key(&mut ctx, ALICE, &sk);

    for _ in 0..2 {
        rejects(&mut ctx, BOB, &signed(&sk, MARKET_ID, 2), E_INVALID);
        rejects(&mut ctx, BOB, &signed(&sk, 99, ISOLATED), E_UNKNOWN_MARKET);
    }
    let stranger = SigningKey::from_bytes(&[0x77; 32]);
    rejects(
        &mut ctx,
        BOB,
        &signed(&stranger, MARKET_ID, ISOLATED),
        "setMarginTypeSigned: signature verification failed",
    );
    // An account with no key at all.
    let mut no_key = setMarginTypeSignedCall::abi_decode(&signed(&sk, MARKET_ID, ISOLATED)).unwrap();
    no_key.account = BOB;
    rejects(
        &mut ctx,
        ALICE,
        &no_key.abi_encode(),
        "setMarginTypeSigned: no api key registered for account",
    );
}

/// Refusals that read state or the gate come AFTER the burn, in the direct path's order and with
/// its strings: the signature is spent, nothing else moves, and a replay is a duplicate.
#[test]
fn signed_state_refusals_follow_the_burn_with_the_direct_strings() {
    // Gated off: ISOLATED is -4046, CROSSED is the gate.
    let sk = SigningKey::from_bytes(&[0x5a; 32]);
    {
        let _off = crate::margin_type::test_gate::disable();
        let mut ctx = make_ctx();
        register_key(&mut ctx, ALICE, &sk);
        rejects_after_burn(&mut ctx, &signed(&sk, MARKET_ID, ISOLATED), E_NO_NEED);
        rejects_after_burn(&mut ctx, &signed(&sk, MARKET_ID, CROSSED), E_GATE);
    }

    // Enabled: -4047, then -4048 — each from a fresh context so each signature is new.
    let mut ctx = make_ctx();
    register_key(&mut ctx, ALICE, &sk);
    rest_bid(&mut ctx, ALICE);
    rejects_after_burn(&mut ctx, &signed(&sk, MARKET_ID, ISOLATED), E_OPEN_ORDERS);

    let mut ctx = make_ctx();
    register_key(&mut ctx, ALICE, &sk);
    // Still enabled: a CROSSED long (see `seed_cross`).
    seed_cross(&mut ctx, ALICE, QTY as i64);
    rejects_after_burn(&mut ctx, &signed(&sk, MARKET_ID, ISOLATED), E_POSITION);
}

/// The signed message covers every field: a signature over one type does not verify for the
/// other, nor for another market.
#[test]
fn the_signed_message_binds_type_and_market() {
    let mut ctx = make_ctx();
    let sk = SigningKey::from_bytes(&[0x5a; 32]);
    register_key(&mut ctx, ALICE, &sk);
    let mut forged = setMarginTypeSignedCall::abi_decode(&signed(&sk, MARKET_ID, CROSSED)).unwrap();
    forged.marginType = ISOLATED;
    rejects(
        &mut ctx,
        BOB,
        &forged.abi_encode(),
        "setMarginTypeSigned: signature verification failed",
    );
    assert_eq!(stored(&mut ctx, ALICE).margin_type, CROSSED);
}

// ── Sub-accounts ──────────────────────────────────────────────────────────────

/// `setMarginType` is on the `asSubAccount` whitelist: the master switches the SUB's own record,
/// billed inner + 5_000, with the sub's own refusals.
#[test]
fn a_master_switches_its_sub_accounts_margin_type() {
    let mut ctx = make_ctx();
    let out = ok(&mut ctx, ALICE, &createSubAccountCall { name: "desk".into() }.abi_encode());
    let sub = createSubAccountCall::abi_decode_returns(&out.bytes).unwrap();
    let wrap = |margin_type| {
        asSubAccountCall { subAccount: sub, data: set_type(MARKET_ID, margin_type).into() }
            .abi_encode()
    };
    let _ = JournalTr::take_logs(ctx.journal_mut());

    // Gated off: the sub's refusals, not the master's.
    let off = crate::margin_type::test_gate::disable();
    rejects(&mut ctx, ALICE, &wrap(ISOLATED), E_NO_NEED);
    rejects(&mut ctx, ALICE, &wrap(CROSSED), E_GATE);
    rejects(&mut ctx, BOB, &wrap(ISOLATED), "asSubAccount: caller is not the master of subAccount");
    drop(off);

    switches(&mut ctx, ALICE, sub, &wrap(ISOLATED), ISOLATED);
    assert_eq!(stored(&mut ctx, ALICE).margin_type, CROSSED, "the master's own record is untouched");
}

/// No `AccountBalanceChanged` on a switch, ever — and the reason code for it is defined anyway.
#[test]
fn the_switch_publishes_no_balance_row_and_reason_nine_is_defined() {
    let mut ctx = make_ctx();
    let _ = JournalTr::take_logs(ctx.journal_mut());
    ok(&mut ctx, ALICE, &set_type(MARKET_ID, ISOLATED));
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert!(logs
        .iter()
        .all(|l| l.data.topics().first() != Some(&AccountBalanceChanged::SIGNATURE_HASH)));
    assert_eq!(AccountUpdateReason::MarginTypeChange.code(), 9);
}

// ── The settings-only write routes ────────────────────────────────────────────

/// The leverage-only route must not carry a margin-type change past its `debug_assert`: each
/// setting has its own route, and neither marks anyone.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "save_position_leverage_only on a path that changed more than")]
fn the_leverage_only_route_refuses_a_margin_type_change() {
    let mut ctx = make_ctx();
    let mut p = stored(&mut ctx, ALICE);
    p.margin_type = ISOLATED;
    let _ = storage::save_position_leverage_only(&mut ctx, ALICE, MARKET_ID, p);
}

/// The type-only route is for a FLAT, order-free record only — what `-4047` / `-4048` guarantee.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "with a position or resting orders")]
fn the_margin_type_only_route_refuses_an_open_position() {
    let mut ctx = make_ctx();
    trade(&mut ctx, ALICE, BOB, 0);
    let mut p = stored(&mut ctx, ALICE);
    p.margin_type = ISOLATED;
    let _ = storage::save_position_margin_type_only(&mut ctx, ALICE, MARKET_ID, p);
}

/// …and it moves nothing but the type: a stale copy carrying another setting is refused.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "save_position_margin_type_only on a path that changed more than")]
fn the_margin_type_only_route_refuses_a_leverage_change() {
    let mut ctx = make_ctx();
    let mut p = stored(&mut ctx, ALICE);
    p.margin_type = ISOLATED;
    p.leverage = 2;
    let _ = storage::save_position_margin_type_only(&mut ctx, ALICE, MARKET_ID, p);
}

/// The general route (`save_position` — fills, funding, liquidation, ADL) must not move the type
/// either, in either direction: a rebuilt-from-default copy resetting ISOLATED to CROSSED is the
/// silent failure this guards against.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "moved margin_type — only setMarginType")]
fn the_general_route_refuses_a_margin_type_change() {
    let mut ctx = make_ctx();
    store_type(&mut ctx, ALICE, ISOLATED);
    let mut p = stored(&mut ctx, ALICE);
    p.margin_type = CROSSED;
    let _ = storage::save_position(&mut ctx, ALICE, MARKET_ID, &p, AccountUpdateReason::Adjustment);
}

/// …nor may the aggregates-only route (a placement or a cancel).
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "save_position_reservation_only moved margin_type")]
fn the_reservation_only_route_refuses_a_margin_type_change() {
    let mut ctx = make_ctx();
    let mut p = stored(&mut ctx, ALICE);
    p.margin_type = ISOLATED;
    let _ = storage::save_position_reservation_only(&mut ctx, ALICE, MARKET_ID, p);
}

/// `setMarginTypeSigned` runs the seen-signature GC after its burn, like every signed selector. The
/// GC collects only the ONE bucket that is exactly `SEEN_RETENTION_BUCKETS` behind the block's, so
/// a signed selector that skipped it would leave that bucket's markers in off-trie state for good
/// whenever it was the only signed traffic in the window. Here it is the only signed traffic: a
/// signature burned at `t` must be gone once a second `setMarginTypeSigned` lands in the bucket that
/// collects `t`'s, and the second one's own marker must be kept.
#[test]
fn the_signed_switch_collects_the_expired_seen_bucket() {
    use crate::storage::{SEEN_BUCKET_WIDTH_SECS as W, SEEN_RETENTION_BUCKETS as R};
    // Gated off, so every switch is refused (`-4046`) after its burn: the GC is under test here.
    let _off = crate::margin_type::test_gate::disable();
    let mut ctx = make_ctx();
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register_key(&mut ctx, ALICE, &sk);
    let sig_hash = |input: &[u8]| -> [u8; 32] {
        let call = setMarginTypeSignedCall::abi_decode(input).unwrap();
        primitives::keccak256(call.signature.as_ref()).0
    };

    let t0 = 10 * W;
    ctx.block.timestamp = U256::from(t0);
    let first = signed_at(&sk, MARKET_ID, ISOLATED, t0);
    rejects_after_burn(&mut ctx, &first, E_NO_NEED);
    assert!(storage::is_signature_seen(&mut ctx, &sig_hash(&first)).unwrap());

    // One bucket short of collection: the marker stays.
    let t1 = (t0 / W + R - 1) * W;
    ctx.block.timestamp = U256::from(t1);
    let second = signed_at(&sk, MARKET_ID, ISOLATED, t1);
    rejects_after_burn(&mut ctx, &second, E_NO_NEED);
    assert!(storage::is_signature_seen(&mut ctx, &sig_hash(&first)).unwrap(), "collected early");

    // The block's bucket is now exactly `R` behind `first`'s: this call's GC collects it.
    let t2 = (t0 / W + R) * W;
    ctx.block.timestamp = U256::from(t2);
    let third = signed_at(&sk, MARKET_ID, ISOLATED, t2);
    rejects_after_burn(&mut ctx, &third, E_NO_NEED);
    assert!(
        !storage::is_signature_seen(&mut ctx, &sig_hash(&first)).unwrap(),
        "setMarginTypeSigned did not collect the expired bucket"
    );
    assert!(storage::is_signature_seen(&mut ctx, &sig_hash(&second)).unwrap());
    assert!(storage::is_signature_seen(&mut ctx, &sig_hash(&third)).unwrap());
}
