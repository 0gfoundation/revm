//! **Cross margin P0 + P1 — an independent spec suite.**
//!
//! Written from `SPEC_cross_margin.md` (owner decisions, P0 and P1 scope) and the design report
//! `CROSS_MARGIN_explore.md` (§1.3–§1.4 Binance's margin and bracket formulas, §1.8 the mode switch
//! and its error codes, §3.1 storage, §3.5 the API, §3.6 the refusal order, §5.2 Q1/Q7, §6), NOT
//! from the implementation's own tests (`margin_type_tests`, the P0 tier tests in `risk::tests` and
//! `perp_core::math`). Exact reject strings and ABI shapes come from the published ABI docs
//! (`interface.rs`); every number asserted below is either written out by hand in the comment next
//! to it or computed by the small spec models in this file ([`slice_mm`], [`reported_ppm`],
//! [`binance_cums`]), which are written from the formulas, not from the engine.
//!
//! ```text
//! Binance  MM = N · MMR_k − cum_k              for lowerBound_k ≤ N < notionalCap_k   (§1.4, F9)
//!          cum_k = cum_{k−1} + lowerBound_k · (MMR_k − MMR_{k−1}),  cum_0 = 0          (F11)
//! ours     MM = Σ_bands ⌊width_i · rate_i⌋     each band floored once ("slice form", D10)
//!          rate_i = column_i / 1e6, or 1 / (2 · maxLev_i) when the column is 0 (Q7)
//! valid    rate_i < 1 / maxLev_i (strict: IM > MM), rates non-decreasing (exact comparison)
//! switch   invalid type → unknown market → −4046 → −4047 → −4048 → (−2023, P2) → gate   (§3.6)
//! ```
//!
//! Every call goes through the full shell (`run_perp_dex_call`), so the gas table, the dispatch, the
//! commit-only #23 tripwire and the event guards are in the loop. Every SUCCESSFUL call is checked
//! for value conservation (`Σ wallet + margin + vQuote + IF`, to the unit); every REJECTED call for
//! the exact reason, zero perp writes (or, for a signed call refused after its burn, writes but no
//! business-state change), no log and a bit-identical world.
//!
//! Cross margin is gated off in production (`CROSS_MARGIN_ENABLED = false`); the spec allows a
//! `#[cfg(test)]`-only override, and the tests that need the CROSSED paths take it through
//! [`cross_on`]. Counterparties in gate-on scenarios that trade are made ISOLATED explicitly, so these
//! tests keep meaning the same thing once P2 gives CROSSED its own accounting.
//!
//! # Map (test prefix → spec item)
//!
//! * `p0_*` — the explicit maintenance-rate column: getter fields (effective rate, `cum`,
//!   `notionalCap`), the Binance identity `MM = N·MMR − cum` over many notionals for explicit, derived
//!   and mixed tables, derived-vs-explicit equivalence, every refusal of `setMarginTiers`, the
//!   16-tier ceiling, the round trip, and the column reaching `liquidate()`.
//! * `mt_*` — `setMarginType` / `setMarginTypeSigned`: the §3.6 ladder (each refusal reached only
//!   once the earlier ones pass), every Binance code, conditional and trailing orders counting as
//!   open orders, per-market / per-user scope, persistence across flat, the event and every getter,
//!   sub-accounts, the signed path's burn and replay semantics, and the gate.
//! * `gated_*` — while gated nothing else moves: an isolated scenario is byte-identical (every log,
//!   every getter) with and without refused switch attempts, whatever byte is stored, and with the
//!   gate on and every actor ISOLATED.

use std::cell::RefCell;

use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use ed25519_dalek::{Signer, SigningKey};
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::call::PerpOutput;
use crate::host::PerpHost;
use crate::interface::IPerpDex::{
    addPositionMarginCall, asSubAccountCall, cancelAlgoOrderCall, cancelOrderCall,
    createSubAccountCall, getAccountCall, getAccountReturn, getMarginTiersCall,
    getMarginTiersReturn, getOpenAlgoOrdersCall, getPositionRiskCall, getSymbolConfigCall,
    getSymbolConfigReturn, liquidateCall, placeAlgoOrderCall, placeOrderCall, registerApiKeyCall,
    removePositionMarginCall, setLeverageCall, setMarginTiersCall, setMarginTypeCall,
    setMarginTypeSignedCall, updateIndexPriceCall, AccountBalanceChanged, AccountPosition,
    Adl, Liquidation, MarginTiersUpdated, MarginTypeChanged, PositionChanged, Trade,
};
use crate::types::{AccountUpdateReason, MarginTiers, MarginType, Market, PerpPosition};
use crate::{run_perp_dex_call, storage, PERP_DEX_ADDRESS, USDC_ADDRESS};

// ── Fixture ──────────────────────────────────────────────────────────────────────────────────

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;
type Log = primitives::Log;
type Id = [u8; 32];

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
/// The market maker most scenarios trade against.
const DAVE: Address = address!("4444444444444444444444444444444444444444");
/// A bystander: relayer, keeper.
const ERIN: Address = address!("5555555555555555555555555555555555555555");
/// Admin = oracle.
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

/// Two ordinary markets (base 0, price 2 decimals: one contract at price `P` is `P·10^4` quote
/// units) and a tier-test market whose decimals make a notional exactly `amount · mark`
/// (base 4 + price 2 = the 6 quote decimals).
const M1: u64 = 1;
const M2: u64 = 2;
const TM: u64 = 3;
/// Never listed.
const NO_MARKET: u64 = 99;
const MARKETS: [u64; 3] = [M1, M2, TM];

/// $100.00.
const MARK: u64 = 10_000;
/// $1e9 perp wallet: margin never interferes.
const WALLET: i64 = 1_000_000_000_000_000;
const GAS: u64 = 30_000_000;
const INTERVAL: u64 = 15;
/// One dollar in quote units (6 dp).
const USD: u64 = 1_000_000;
/// ppm denominator.
const ONE: u128 = 1_000_000;

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MKT: u8 = 1;
const GTC: u8 = 0;
const RO: u8 = 0b0001;
const CP: u8 = 0b0010;

/// `AlgoOrderType`.
const STOP_MARKET: u8 = 1;
const TAKE_PROFIT: u8 = 2;
const TRAILING: u8 = 4;

/// Margin type wire codes (§3.1: 0 = CROSSED so a zero-initialised record is Binance's default).
const CROSSED: u8 = 0;
const ISOLATED: u8 = 1;

/// Flat gas (§3.5): `setMarginType` = `setLeverage` = 30k; `asSubAccount` adds 5k.
const MT_GAS: u64 = 30_000;
const SUB_OVERHEAD: u64 = 5_000;

// Reject reasons — the ABI docs' wording (`interface.rs`, `setMarginType` / `setMarginTiers`).
const E_TYPE: &str = "setMarginType: invalid marginType (0 = CROSSED, 1 = ISOLATED)";
const E_MARKET: &str = "setMarginType: unknown market";
const E_4046: &str = "setMarginType: -4046 No need to change margin type.";
const E_4047: &str =
    "setMarginType: -4047 Margin type cannot be changed if there exists open orders.";
const E_4048: &str = "setMarginType: -4048 Margin type cannot be changed if there exists position.";
const E_GATE: &str = "setMarginType: CROSSED margin is not enabled yet";
const E_DUP: &str = "setMarginTypeSigned: duplicate signature (already submitted)";
const E_NO_KEY: &str = "setMarginTypeSigned: no api key registered for account";
const E_BAD_SIG: &str = "setMarginTypeSigned: signature verification failed";
const E_FUTURE: &str = "setMarginTypeSigned: timestamp is in the future";
const E_STALE: &str = "setMarginTypeSigned: timestamp expired (outside recvWindow)";
const E_KEY_EXPIRED: &str = "setMarginTypeSigned: api key has expired";
const E_NOT_MASTER: &str = "asSubAccount: caller is not the master of subAccount";
const E_NOT_WHITELISTED: &str = "asSubAccount: inner selector is not allowed for a sub-account";

const ET_MARKET: &str = "setMarginTiers: unknown market";
const ET_LEN: &str = "setMarginTiers: lowerBounds and maxLeverages length mismatch";
const ET_EMPTY: &str = "setMarginTiers: at least one tier required";
const ET_MANY: &str = "setMarginTiers: at most 16 tiers allowed";
const ET_FIRST: &str = "setMarginTiers: first tier must start at 0";
const ET_BOUNDS: &str = "setMarginTiers: lowerBounds must be strictly increasing";
const ET_LEV: &str = "setMarginTiers: maxLeverage must be 1–150";
const ET_LEV_ORDER: &str = "setMarginTiers: maxLeverages must be non-increasing";
const ET_RATE_LEN: &str = "setMarginTiers: maintMarginRatesPpm and lowerBounds length mismatch";
const ET_IM_MM: &str =
    "setMarginTiers: maintMarginRatePpm must be below 1000000 / maxLeverage (IM > MM)";
const ET_RATE_ORDER: &str = "setMarginTiers: maintenance margin rates must be non-decreasing";
const E_AUTH: &str = "not authorised: caller is not admin or market manager";
const E_HEALTHY: &str = "liquidate: position is above maintenance margin";

thread_local! {
    /// Every account a test enrolled — the conservation and world snapshots run over these.
    static USERS: RefCell<Vec<Address>> = const { RefCell::new(Vec::new()) };
}

fn users() -> Vec<Address> {
    USERS.with(|u| u.borrow().clone())
}

/// Enables cross margin on this thread until the guard drops — the spec's `#[cfg(test)]` override.
fn cross_on() -> impl Drop {
    crate::margin_type::test_gate::enable()
}

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ADMIN, Address::ZERO] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

fn the_market(id: u64, base_decimals: u32, max: u64) -> Market {
    Market {
        market_id: id,
        base_decimals,
        price_decimals: 2,
        tick_size: 1,
        step_size: 1,
        min_quantity: 1,
        max_quantity: max,
        max_price: max,
        price_update_interval: INTERVAL,
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
    }
}

/// M1 and M2 at $100 (default tier `{0, 3}`, no funding, no fees, the default ±10 % band), the
/// tier market TM at mark 1, the admin set, a $1e9 perp wallet for ALICE … ERIN.
fn fresh() -> TestCtx {
    USERS.with(|u| *u.borrow_mut() = vec![ADMIN, Address::ZERO]);
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(&mut ctx, &the_market(M1, 0, 1_000_000)).unwrap();
    storage::save_market(&mut ctx, &the_market(M2, 0, 1_000_000)).unwrap();
    storage::save_market(&mut ctx, &the_market(TM, 4, 1 << 60)).unwrap();
    storage::save_mark_price(&mut ctx, M1, MARK).unwrap();
    storage::save_mark_price(&mut ctx, M2, MARK).unwrap();
    storage::save_mark_price(&mut ctx, TM, 1).unwrap();
    for u in [ALICE, BOB, CAROL, DAVE, ERIN] {
        enroll(&mut ctx, u);
    }
    let _ = take_logs(&mut ctx);
    ctx
}

fn enroll(ctx: &mut TestCtx, u: Address) {
    JournalTr::load_account(ctx.journal_mut(), u).unwrap();
    let mut a = storage::load_account(ctx, u).unwrap();
    a.perp_wallet_balance = WALLET;
    storage::save_account(ctx, u, a, AccountUpdateReason::Adjustment).unwrap();
    USERS.with(|us| {
        if !us.borrow().contains(&u) {
            us.borrow_mut().push(u);
        }
    });
    let _ = take_logs(ctx);
}

/// Seeds `user` OFF-BOOK with `amount` contracts in `market`, entered at the stored mark, margin
/// `|notional| / leverage`, keeping the record's stored margin type. Fixture only: it bypasses
/// every gate — so a test that enables cross margin switches the user to ISOLATED BEFORE seeding
/// (an isolated silo under a CROSSED record is a state P2 forbids).
fn seed(ctx: &mut TestCtx, user: Address, market: u64, amount: i64, leverage: u64) {
    let mark = storage::load_mark_price(ctx, market).unwrap() as i128;
    let scale: i128 = if market == TM { 1 } else { 10_000 };
    let n = amount as i128 * mark * scale;
    let stored = storage::load_position(ctx, user, market).unwrap().margin_type;
    storage::save_position(
        ctx,
        user,
        market,
        &PerpPosition {
            amount,
            v_quote_balance: (-n) as i64,
            margin: (n.abs() / leverage as i128) as i64,
            leverage,
            margin_type: stored,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let _ = take_logs(ctx);
}

/// [`seed`] for a position CROSSED in force: same entry at the stored mark, but NO isolated
/// margin — a cross position's collateral is the cross wallet (P2: `save_position` debug-asserts
/// `pos.margin == 0` for every position CROSSED in force). Gate-on scenarios open CROSSED
/// positions here and let real fills only CLOSE them, until the fill path gains its cross branch.
fn seed_cross(ctx: &mut TestCtx, user: Address, market: u64, amount: i64) {
    let mark = storage::load_mark_price(ctx, market).unwrap() as i128;
    let scale: i128 = if market == TM { 1 } else { 10_000 };
    let n = amount as i128 * mark * scale;
    let p = PerpPosition {
        amount,
        v_quote_balance: (-n) as i64,
        margin: 0,
        ..storage::load_position(ctx, user, market).unwrap()
    };
    storage::save_position(ctx, user, market, &p, AccountUpdateReason::Adjustment).unwrap();
    let _ = take_logs(ctx);
}

// ── Shell ────────────────────────────────────────────────────────────────────────────────────

fn take_logs(ctx: &mut TestCtx) -> Vec<Log> {
    JournalTr::take_logs(ctx.journal_mut())
}

fn revert_reason(out: &PerpOutput) -> String {
    assert!(out.reverted, "the call did not revert");
    let b = &out.bytes;
    assert_eq!(&b[..4], &[0x08, 0xc3, 0x79, 0xa0], "Error(string) selector");
    let len = U256::from_be_slice(&b[36..68]).to::<usize>();
    String::from_utf8(b[68..68 + len].to_vec()).unwrap()
}

/// `Σ (perp wallet + position margin + vQuote)` over every enrolled account and market, plus the
/// insurance fund. Nothing here charges a fee, so no call may move it.
fn value(ctx: &mut TestCtx) -> i128 {
    let mut v = storage::load_insurance_fund(ctx).unwrap() as i128;
    for u in users() {
        v += storage::load_account(ctx, u).unwrap().perp_wallet_balance as i128;
        for m in MARKETS {
            let p = storage::load_position(ctx, u, m).unwrap();
            v += p.margin as i128 + p.v_quote_balance as i128;
        }
    }
    v
}

struct Call {
    out: PerpOutput,
    logs: Vec<Log>,
}

fn run(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap()
}

/// One SUCCESSFUL shell call, value-conserving to the unit.
fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Call {
    let _ = take_logs(ctx);
    let before = value(ctx);
    let out = run(ctx, caller, input);
    assert!(!out.reverted, "unexpected revert: {}", revert_reason(&out));
    let logs = take_logs(ctx);
    assert_eq!(value(ctx), before, "value not conserved (Σ wallet + margin + vQuote + IF)");
    Call { out, logs }
}

fn enc<T: serde::Serialize>(v: &T) -> Vec<u8> {
    perp_core::codec::encode(v).unwrap()
}

/// Everything a rejected call must leave bit-identical (the signature-seen set excepted).
fn world(ctx: &mut TestCtx) -> Vec<Vec<u8>> {
    let mut w = Vec::new();
    for u in users() {
        w.push(enc(&storage::load_account(ctx, u).unwrap()));
        for m in MARKETS {
            w.push(enc(&storage::load_position(ctx, u, m).unwrap()));
            w.push(enc(&storage::load_buy_orders(ctx, u, m).unwrap()));
            w.push(enc(&storage::load_sell_orders(ctx, u, m).unwrap()));
            w.push(enc(&storage::load_user_algo_orders(ctx, u, m).unwrap()));
        }
    }
    for m in MARKETS {
        w.push(enc(&storage::load_bid_prices(ctx, m).unwrap()));
        w.push(enc(&storage::load_ask_prices(ctx, m).unwrap()));
        w.push(enc(&*storage::load_market_ref(ctx, m).unwrap().unwrap()));
        w.push(enc(&storage::load_index_price_state(ctx, m).unwrap()));
    }
    w.push(enc(&storage::load_insurance_fund(ctx).unwrap()));
    w
}

/// A REJECTED call: the exact reason, no perp write, no log, the world bit-identical.
fn rejected(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) -> PerpOutput {
    let _ = take_logs(ctx);
    let before = world(ctx);
    let writes = ctx.perp_write_count();
    let out = run(ctx, caller, input);
    assert!(out.reverted, "expected the revert `{expect}`, but the call SUCCEEDED");
    assert_eq!(revert_reason(&out), expect);
    assert_eq!(ctx.perp_write_count() - writes, 0, "[{expect}] perp writes");
    assert!(take_logs(ctx).is_empty(), "[{expect}] the rejected call logged");
    assert!(world(ctx) == before, "[{expect}] the rejected call changed stored state");
    out
}

/// A signed call refused AFTER its burn: the exact reason, at least one perp write (the burn),
/// no log, no business state moved — and the same calldata is then a duplicate, refused with zero
/// writes.
fn rejected_after_burn(ctx: &mut TestCtx, input: &[u8], expect: &str) {
    let _ = take_logs(ctx);
    let before = world(ctx);
    let writes = ctx.perp_write_count();
    let out = run(ctx, ERIN, input);
    assert!(out.reverted, "expected the revert `{expect}`, but the call SUCCEEDED");
    assert_eq!(revert_reason(&out), expect);
    assert_eq!(out.gas_used, MT_GAS, "[{expect}] flat gas");
    assert!(ctx.perp_write_count() > writes, "[{expect}] the signature was not burned");
    assert!(take_logs(ctx).is_empty(), "[{expect}] the rejected call logged");
    assert!(world(ctx) == before, "[{expect}] the refused call moved business state");
    rejected(ctx, ERIN, input, E_DUP);
}

/// A signed call refused BEFORE its burn: flat gas, zero writes, and resubmitting gives the same
/// answer.
fn rejected_before_burn(ctx: &mut TestCtx, input: &[u8], expect: &str) {
    let out = rejected(ctx, ERIN, input, expect);
    assert_eq!(out.gas_used, MT_GAS, "[{expect}] flat gas");
    rejected(ctx, ERIN, input, expect);
}

fn view(ctx: &mut TestCtx, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, GAS, ERIN, U256::ZERO, true, ctx).unwrap()
}

fn evs<E: SolEvent>(logs: &[Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

// ── Calldata and shortcuts ───────────────────────────────────────────────────────────────────

fn set_type(market: u64, t: u8) -> Vec<u8> {
    setMarginTypeCall { marketId: market, marginType: t }.abi_encode()
}

fn set_tiers(market: u64, t: &[Row]) -> Vec<u8> {
    set_tiers_capped(market, t, 0)
}

/// [`set_tiers`] with the last bracket's `notionalCap` (`0` = unbounded).
fn set_tiers_capped(market: u64, t: &[Row], cap: u64) -> Vec<u8> {
    setMarginTiersCall {
        marketId: market,
        lowerBounds: t.iter().map(|r| r.lb).collect(),
        maxLeverages: t.iter().map(|r| r.lev).collect(),
        maintMarginRatesPpm: t.iter().map(|r| r.ppm).collect(),
        notionalCap: cap,
    }
    .abi_encode()
}

fn order(market: u64, side: u8, price: u64, qty: u64, kind: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: market,
        side,
        price,
        quantity: qty,
        orderType: kind,
        tif: GTC,
        clientOrderId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

/// A GTC limit that must rest untouched.
fn rest(ctx: &mut TestCtx, user: Address, market: u64, side: u8, price: u64, qty: u64) -> Id {
    let c = ok(ctx, user, &order(market, side, price, qty, LIMIT, 0));
    assert!(evs::<Trade>(&c.logs).is_empty(), "{user} {side}@{price} traded while resting");
    placeOrderCall::abi_decode_returns(&c.out.bytes).unwrap().0
}

fn take(ctx: &mut TestCtx, user: Address, market: u64, side: u8, qty: u64) -> Call {
    let c = ok(ctx, user, &order(market, side, 0, qty, MKT, 0));
    assert!(!evs::<Trade>(&c.logs).is_empty(), "{user}'s market order did not trade");
    c
}

fn cancel(ctx: &mut TestCtx, user: Address, market: u64, id: Id) {
    ok(ctx, user, &cancelOrderCall { orderId: id.into(), marketId: market }.abi_encode());
}

/// `user` opens `qty` (signed: + long, − short) at $100 against DAVE's fresh resting liquidity.
fn open(ctx: &mut TestCtx, user: Address, market: u64, qty: i64) -> Call {
    let (maker_side, taker_side) = if qty > 0 { (SELL, BUY) } else { (BUY, SELL) };
    rest(ctx, DAVE, market, maker_side, MARK, qty.unsigned_abs());
    take(ctx, user, market, taker_side, qty.unsigned_abs())
}

/// `user` trades its whole position away against DAVE's fresh resting liquidity.
fn close(ctx: &mut TestCtx, user: Address, market: u64) -> Call {
    let a = amount(ctx, user, market);
    assert_ne!(a, 0);
    let c = open(ctx, user, market, -a);
    assert_eq!(amount(ctx, user, market), 0);
    c
}

#[allow(clippy::too_many_arguments)]
fn algo_call(
    market: u64,
    kind: u8,
    side: u8,
    trigger: u64,
    price: u64,
    qty: u64,
    tif: u8,
    flags: u8,
) -> Vec<u8> {
    placeAlgoOrderCall {
        algoType: 0,
        marketId: market,
        side,
        orderType: kind,
        triggerPrice: trigger,
        price,
        quantity: qty,
        tif,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

fn place_algo(ctx: &mut TestCtx, user: Address, input: &[u8]) -> Id {
    let c = ok(ctx, user, input);
    placeAlgoOrderCall::abi_decode_returns(&c.out.bytes).unwrap().0
}

fn cancel_algo(ctx: &mut TestCtx, user: Address, market: u64, id: Id) {
    ok(ctx, user, &cancelAlgoOrderCall { algoId: id.into(), marketId: market }.abi_encode());
}

fn open_algos(ctx: &mut TestCtx, user: Address, market: u64) -> Vec<Id> {
    let out = view(ctx, &getOpenAlgoOrdersCall { user, marketId: market }.abi_encode());
    assert!(!out.reverted);
    getOpenAlgoOrdersCall::abi_decode_returns(&out.bytes)
        .unwrap()
        .into_iter()
        .map(|b| b.0)
        .collect()
}

fn amount(ctx: &mut TestCtx, user: Address, market: u64) -> i64 {
    storage::load_position(ctx, user, market).unwrap().amount
}

fn sym(ctx: &mut TestCtx, user: Address, market: u64) -> getSymbolConfigReturn {
    let out = view(ctx, &getSymbolConfigCall { user, marketId: market }.abi_encode());
    assert!(!out.reverted, "getSymbolConfig reverted: {}", revert_reason(&out));
    getSymbolConfigCall::abi_decode_returns(&out.bytes).unwrap()
}

fn risk(ctx: &mut TestCtx, user: Address, market: u64) -> AccountPosition {
    let out = view(ctx, &getPositionRiskCall { user, marketId: market }.abi_encode());
    assert!(!out.reverted, "getPositionRisk reverted: {}", revert_reason(&out));
    getPositionRiskCall::abi_decode_returns(&out.bytes).unwrap()
}

fn account(ctx: &mut TestCtx, user: Address) -> getAccountReturn {
    let out = view(ctx, &getAccountCall { user }.abi_encode());
    assert!(!out.reverted, "getAccount reverted: {}", revert_reason(&out));
    getAccountCall::abi_decode_returns(&out.bytes).unwrap()
}

/// `getMarginTiers`' raw return data (the decoded struct has no `PartialEq`).
fn tiers_raw(ctx: &mut TestCtx, market: u64) -> Vec<u8> {
    view(ctx, &getMarginTiersCall { marketId: market }.abi_encode()).bytes.to_vec()
}

fn tiers(ctx: &mut TestCtx, market: u64) -> getMarginTiersReturn {
    let out = view(ctx, &getMarginTiersCall { marketId: market }.abi_encode());
    assert!(!out.reverted, "getMarginTiers reverted: {}", revert_reason(&out));
    getMarginTiersCall::abi_decode_returns(&out.bytes).unwrap()
}

/// The margin type every surface reports for `(user, market)`: getSymbolConfig and
/// getPositionRisk must agree (and getAccount's row, when the market is in the index).
fn reported(ctx: &mut TestCtx, user: Address, market: u64) -> u8 {
    let s = sym(ctx, user, market).marginType;
    assert_eq!(risk(ctx, user, market).marginType, s, "getPositionRisk vs getSymbolConfig");
    if let Some(row) = account(ctx, user).positions.iter().find(|r| r.marketId == market) {
        assert_eq!(row.marginType, s, "getAccount row vs getSymbolConfig");
    }
    s
}

/// A successful switch: flat gas, exactly one log — `MarginTypeChanged(user, market, to)` from the
/// precompile — no balance row, the type reported everywhere, leverage untouched, and the market
/// not pulled into the user's index.
///
/// A direct switch performs exactly ONE perp write (the position record); a signed one adds its
/// signature burn.
#[allow(clippy::too_many_arguments)]
fn switch(
    ctx: &mut TestCtx,
    caller: Address,
    user: Address,
    input: &[u8],
    market: u64,
    to: u8,
    gas: u64,
    signed: bool,
) {
    let lev = sym(ctx, user, market).leverage;
    let indexed = account(ctx, user).positions.iter().map(|r| r.marketId).collect::<Vec<_>>();
    let writes = ctx.perp_write_count();
    let c = ok(ctx, caller, input);
    assert_eq!(c.out.gas_used, gas, "flat gas");
    assert!(c.out.bytes.is_empty(), "setMarginType returns nothing");
    let w = ctx.perp_write_count() - writes;
    if signed {
        assert!(w > 1, "a signed switch writes the record and burns the signature ({w})");
    } else {
        assert_eq!(w, 1, "a switch writes the type byte's record only");
    }
    assert_eq!(c.logs.len(), 1, "a switch publishes exactly one log");
    assert_eq!(c.logs[0].address, PERP_DEX_ADDRESS);
    let e = evs::<MarginTypeChanged>(&c.logs);
    assert_eq!(e.len(), 1, "the one log is MarginTypeChanged");
    assert_eq!((e[0].user, e[0].marketId, e[0].marginType), (user, market, to));
    assert!(evs::<AccountBalanceChanged>(&c.logs).is_empty());
    assert_eq!(reported(ctx, user, market), to);
    assert_eq!(sym(ctx, user, market).leverage, lev, "the switch moved leverage");
    let after = account(ctx, user).positions.iter().map(|r| r.marketId).collect::<Vec<_>>();
    assert_eq!(after, indexed, "the switch changed the user's market index");
}

/// [`switch`] for a direct call by `user` itself.
fn switch_self(ctx: &mut TestCtx, user: Address, market: u64, to: u8) {
    switch(ctx, user, user, &set_type(market, to), market, to, MT_GAS, false);
}

/// A direct refusal, flat gas.
fn refuse(ctx: &mut TestCtx, user: Address, market: u64, t: u8, expect: &str) {
    let out = rejected(ctx, user, &set_type(market, t), expect);
    assert_eq!(out.gas_used, MT_GAS, "[{expect}] flat gas");
}

/// One oracle tick on `market` publishing `mark == index` (no funding; `last_traded` cleared so
/// the contract-price leg is pinned to the index).
fn tick(ctx: &mut TestCtx, market: u64, index: u64) -> Call {
    storage::save_last_traded_price(ctx, market, 0).unwrap();
    let ts = storage::load_index_price_state(ctx, market).unwrap().timestamp + INTERVAL;
    let c = ok(
        ctx,
        ADMIN,
        &updateIndexPriceCall { marketId: market, indexPrice: index, timestamp: ts }.abi_encode(),
    );
    assert_eq!(storage::load_mark_price(ctx, market).unwrap(), index, "mark != index");
    c
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// P0 — the explicit maintenance-rate column
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// One tier row as `setMarginTiers` takes it: `ppm == 0` is DERIVED (`1 / (2·lev)`).
#[derive(Clone, Copy, Debug)]
struct Row {
    lb: u64,
    lev: u32,
    ppm: u32,
}

const fn row(lb_usd: u64, lev: u32, ppm: u32) -> Row {
    Row { lb: lb_usd * USD, lev, ppm }
}

/// The exact maintenance rate as a fraction.
fn rate(r: Row) -> (u128, u128) {
    if r.ppm == 0 {
        (1, 2 * r.lev as u128)
    } else {
        (r.ppm as u128, ONE)
    }
}

/// Binance's `maintMarginRatio` as reported: the column, or `1/(2L)` rounded UP to whole ppm.
fn reported_ppm(r: Row) -> u32 {
    if r.ppm != 0 {
        r.ppm
    } else {
        1_000_000u32.div_ceil(2 * r.lev)
    }
}

/// Binance's `cum` column (F11): `cum_i = cum_{i−1} + lb_i · (rate_i − rate_{i−1})` over the
/// REPORTED rates, accumulated exactly and floored once.
fn binance_cums(t: &[Row]) -> Vec<u64> {
    let mut acc: i128 = 0;
    let mut out = vec![0u64];
    for i in 1..t.len() {
        acc += t[i].lb as i128 * (reported_ppm(t[i]) as i128 - reported_ppm(t[i - 1]) as i128);
        assert!(acc >= 0);
        out.push((acc / ONE as i128) as u64);
    }
    out
}

/// The spec's slice form (D10): every band the notional reaches contributes `⌊width · rate⌋`.
fn slice_mm(t: &[Row], n: u64) -> u64 {
    let mut mm: u128 = 0;
    for (i, r) in t.iter().enumerate() {
        if n <= r.lb {
            break;
        }
        let ub = t.get(i + 1).map_or(u64::MAX, |x| x.lb);
        let (a, b) = rate(*r);
        mm += (n.min(ub) - r.lb) as u128 * a / b;
    }
    mm as u64
}

/// The tier a notional sits in: the last with `lowerBound ≤ n`.
fn tier_of(t: &[Row], n: u64) -> usize {
    t.iter().rposition(|r| r.lb <= n).unwrap()
}

/// `MM` the engine reports for a notional `n` on TM (long when `long`), via `getPositionRisk`.
/// TM's decimals make `notional = amount · mark` exactly; mark 1 makes it `amount`.
fn engine_mm(ctx: &mut TestCtx, n: u64, long: bool) -> u64 {
    let a = if long { n as i64 } else { -(n as i64) };
    seed(ctx, CAROL, TM, a, 1);
    let r = risk(ctx, CAROL, TM);
    assert_eq!(r.notional, n, "TM's notional must be amount · mark");
    r.maintMargin
}

/// Binance BTCUSDT `leverageBracket` (report §1.4: cum 0 / 50 / 1,300 / 46,300 / … /
/// 101,421,300), `(floor $, initialLeverage, maintMarginRatio ppm)`, verbatim — tier 0's 125x
/// included (the leverage hard cap is 150; see `p0_structural_refusals…`).
const BTC: [Row; 10] = [
    row(0, 125, 4_000),
    row(50_000, 100, 5_000),
    row(250_000, 50, 10_000),
    row(3_000_000, 20, 25_000),
    row(15_000_000, 10, 50_000),
    row(30_000_000, 5, 100_000),
    row(80_000_000, 4, 125_000),
    row(100_000_000, 3, 150_000),
    row(200_000_000, 2, 250_000),
    row(300_000_000, 1, 500_000),
];
/// The `cum` column Binance publishes for it, in USDT.
const BTC_CUM_USD: [u64; 10] =
    [0, 50, 1_300, 46_300, 421_300, 1_921_300, 3_921_300, 6_421_300, 26_421_300, 101_421_300];

/// Notionals to probe a table at: every bound ±1, mid-band points, the origin, deterministic
/// pseudo-random points up to twice the last bound, and a far tail.
fn probes(t: &[Row]) -> Vec<u64> {
    let mut v = vec![0, 1, 2, 7, 999_999, USD, USD + 1];
    for (i, r) in t.iter().enumerate() {
        if r.lb > 0 {
            v.extend([r.lb - 1, r.lb, r.lb + 1]);
        }
        let ub = t.get(i + 1).map_or(r.lb.max(USD) * 2, |x| x.lb);
        v.extend([r.lb + (ub - r.lb) / 2, r.lb + (ub - r.lb) / 3 + 13, ub - 7]);
    }
    let top = t.last().unwrap().lb.max(USD) * 2;
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..60 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.push(x % top);
    }
    v.push(1_000_000_000 * USD + 123_457);
    v.sort_unstable();
    v.dedup();
    v
}

/// The hand-written models agree with the numbers the report quotes, before anything is asked
/// of the engine: BTC's cum column, the 3x derived rate, and the slice form = Binance's form on
/// whole-dollar explicit tables.
#[test]
fn p0_the_spec_models_reproduce_the_reports_numbers() {
    let cums = binance_cums(&BTC);
    assert_eq!(cums, BTC_CUM_USD.iter().map(|c| c * USD).collect::<Vec<_>>());
    // cum_4 = 1,300 + 3,000,000·(2.5% − 1%) = 46,300; cum_10 = 26,421,300 + 300M·25% = 101,421,300.
    assert_eq!(cums[3], 46_300 * USD);
    assert_eq!(reported_ppm(row(0, 3, 0)), 166_667);
    assert_eq!(reported_ppm(row(0, 7, 0)), 71_429);
    assert_eq!(reported_ppm(row(0, 4, 0)), 125_000);
    // $60,000 notional in tier 1: 60,000·0.5% − 50 = 250 (slice: 50,000·0.4% + 10,000·0.5%).
    assert_eq!(slice_mm(&BTC, 60_000 * USD), 250 * USD);
    for n in probes(&BTC) {
        let k = tier_of(&BTC, n);
        let binance = n as u128 * BTC[k].ppm as u128 / ONE - cums[k] as u128;
        assert_eq!(slice_mm(&BTC, n) as u128, binance, "n = {n}");
    }
}

/// A market that never had `setMarginTiers` reads back as ONE derived Binance bracket:
/// `{floor 0, 3x, column 0}`, effective `⌈1e6/6⌉ = 166,667` ppm, `cum 0`, cap `type(uint64).max`.
#[test]
fn p0_the_default_table_reads_as_one_derived_bracket() {
    let mut ctx = fresh();
    for m in MARKETS {
        let t = tiers(&mut ctx, m);
        assert_eq!(t.lowerBounds, vec![0]);
        assert_eq!(t.maxLeverages, vec![3]);
        assert_eq!(t.maintMarginRatesPpm, vec![0]);
        assert_eq!(t.effectiveMaintMarginRatesPpm, vec![166_667]);
        assert_eq!(t.cums, vec![0]);
        assert_eq!(t.notionalCaps, vec![u64::MAX]);
    }
    let out = view(&mut ctx, &getMarginTiersCall { marketId: NO_MARKET }.abi_encode());
    assert!(out.reverted, "getMarginTiers of an unknown market must revert");
}

/// Binance's BTCUSDT table loads row for row (10 brackets > the old 8): the setter's event carries
/// the three input arrays verbatim, and the getter completes each Binance row — the column as
/// the effective rate, Binance's published `cum`, `notionalCap` = the next floor (last: max).
#[test]
fn p0_the_btc_table_reads_back_as_binances_leverage_bracket() {
    let mut ctx = fresh();
    let c = ok(&mut ctx, ADMIN, &set_tiers(TM, &BTC));
    let e = evs::<MarginTiersUpdated>(&c.logs);
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].marketId, TM);
    assert_eq!(e[0].lowerBounds, BTC.iter().map(|r| r.lb).collect::<Vec<_>>());
    assert_eq!(e[0].maxLeverages, BTC.iter().map(|r| r.lev).collect::<Vec<_>>());
    assert_eq!(e[0].maintMarginRatesPpm, BTC.iter().map(|r| r.ppm).collect::<Vec<_>>());

    let t = tiers(&mut ctx, TM);
    assert_eq!(t.lowerBounds, e[0].lowerBounds);
    assert_eq!(t.maxLeverages, e[0].maxLeverages);
    assert_eq!(t.maintMarginRatesPpm, e[0].maintMarginRatesPpm);
    assert_eq!(t.effectiveMaintMarginRatesPpm, e[0].maintMarginRatesPpm);
    assert_eq!(t.cums, BTC_CUM_USD.iter().map(|c| c * USD).collect::<Vec<_>>());
    let mut caps: Vec<u64> = BTC[1..].iter().map(|r| r.lb).collect();
    caps.push(u64::MAX);
    assert_eq!(t.notionalCaps, caps);
    // Every tier: IM rate 1/L strictly above the MM rate.
    for r in BTC {
        assert!((r.ppm as u64) * (r.lev as u64) < 1_000_000);
    }
    // The other markets' tables are untouched.
    assert_eq!(tiers(&mut ctx, M1).lowerBounds, vec![0]);

    // getSymbolConfig's maxNotionalValue reads the same table backwards: the upper bound of the
    // highest tier still admitting the leverage; 0 = unbounded (the last tier).
    for (lev, cap) in [
        (125u64, 50_000 * USD),
        (100, 250_000 * USD),
        (50, 3_000_000 * USD),
        (20, 15_000_000 * USD),
        (3, 200_000_000 * USD),
        (2, 300_000_000 * USD),
        (1, 0),
    ] {
        ok(&mut ctx, ERIN, &setLeverageCall { marketId: TM, leverage: lev }.abi_encode());
        let s = sym(&mut ctx, ERIN, TM);
        assert_eq!((s.leverage, s.maxNotionalValue), (lev, cap), "leverage {lev}");
    }
}

/// The ABI moves the `setMarginTiers` / `MarginTiersUpdated` docs announce, pinned so the notes
/// cannot drift from the code: the setter's selector and the event's topic0 are those of the
/// five-field shapes, not the pre-P0 three-field ones.
#[test]
fn p0_the_announced_selector_and_topic_moves_are_the_real_ones() {
    assert_eq!(setMarginTiersCall::SELECTOR, [0x64, 0x30, 0x93, 0xa5]);
    assert_eq!(
        MarginTiersUpdated::SIGNATURE_HASH,
        primitives::b256!("3a84256764d1409d0ecec333b28e23fd56f48643efce82ed3e46ec7590500a6c")
    );
    // The pre-P0 shapes, for contrast: `setMarginTiers(uint64,uint64[],uint32[])` was 0x34364f71.
    assert_eq!(
        &primitives::keccak256("setMarginTiers(uint64,uint64[],uint32[])")[..4],
        &[0x34, 0x36, 0x4f, 0x71]
    );
    assert_eq!(
        primitives::keccak256("MarginTiersUpdated(uint64,uint64[],uint32[])"),
        primitives::b256!("3cb888268979d015fe25916506201197e8c69685683b7a04afc7ba8274213674")
    );
}

/// Binance BTCUSDT's CURRENT `leverageBracket` (live capture, 2026-10-14): `(floor $,
/// initialLeverage, maintMarginRatio ppm)`; its last bracket's `notionalCap` is [`BTC_LIVE_CAP_USD`].
const BTC_LIVE: [Row; 12] = [
    row(0, 150, 4_000),
    row(300_000, 100, 5_000),
    row(800_000, 75, 6_500),
    row(3_000_000, 50, 10_000),
    row(12_000_000, 25, 20_000),
    row(70_000_000, 20, 25_000),
    row(100_000_000, 10, 50_000),
    row(230_000_000, 5, 100_000),
    row(480_000_000, 4, 125_000),
    row(600_000_000, 3, 150_000),
    row(800_000_000, 2, 250_000),
    row(1_200_000_000, 1, 500_000),
];
const BTC_LIVE_CAP_USD: u64 = 1_800_000_000;
/// The `cum` column Binance returned with it, USDT.
const BTC_LIVE_CUM_USD: [u64; 12] = [
    0,
    300,
    1_500,
    12_000,
    132_000,
    482_000,
    2_982_000,
    14_482_000,
    26_482_000,
    41_482_000,
    121_482_000,
    421_482_000,
];

/// Binance's current BTCUSDT table loads exactly as Binance returned it — 150x first, twelve
/// brackets, and the last bracket's FINITE `notionalCap` — and every column the getter reports is
/// Binance's: the rates, `cum` and `notionalCap` (the last one $1.8B, not "unbounded"). The event
/// carries the cap; `getSymbolConfig`'s `maxNotionalValue` is Binance's at every leverage,
/// including 1x, where it is the cap.
#[test]
fn p0_binances_live_btc_table_reads_back_with_its_last_notional_cap() {
    let mut ctx = fresh();
    assert_eq!(binance_cums(&BTC_LIVE), BTC_LIVE_CUM_USD.map(|c| c * USD).to_vec());
    let c = ok(&mut ctx, ADMIN, &set_tiers_capped(TM, &BTC_LIVE, BTC_LIVE_CAP_USD * USD));
    let e = evs::<MarginTiersUpdated>(&c.logs);
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].notionalCap, BTC_LIVE_CAP_USD * USD);

    let t = tiers(&mut ctx, TM);
    assert_eq!(t.maxLeverages, BTC_LIVE.iter().map(|r| r.lev).collect::<Vec<_>>());
    assert_eq!(t.effectiveMaintMarginRatesPpm, BTC_LIVE.iter().map(|r| r.ppm).collect::<Vec<_>>());
    assert_eq!(t.cums, BTC_LIVE_CUM_USD.map(|c| c * USD).to_vec());
    let mut caps: Vec<u64> = BTC_LIVE[1..].iter().map(|r| r.lb).collect();
    caps.push(BTC_LIVE_CAP_USD * USD);
    assert_eq!(t.notionalCaps, caps);

    for (lev, cap_usd) in [
        (150u64, 300_000u64),
        (100, 800_000),
        (75, 3_000_000),
        (20, 100_000_000),
        (3, 800_000_000),
        (2, 1_200_000_000),
        (1, BTC_LIVE_CAP_USD),
    ] {
        ok(&mut ctx, ERIN, &setLeverageCall { marketId: TM, leverage: lev }.abi_encode());
        let s = sym(&mut ctx, ERIN, TM);
        assert_eq!((s.leverage, s.maxNotionalValue), (lev, cap_usd * USD), "leverage {lev}");
    }
}

/// `cum` follows Binance's recursion over the REPORTED (rounded-up) rates, exactly accumulated and
/// floored once — on a derived table with non-whole-ppm rates and on a mixed table.
#[test]
fn p0_cum_follows_the_binance_recursion_on_derived_and_mixed_tables() {
    let mut ctx = fresh();
    let derived = [row(0, 50, 0), row(100_000, 20, 0), row(1_000_000, 7, 0), row(5_000_000, 3, 0)];
    let mixed = [
        row(0, 50, 0),
        row(10_000, 40, 12_345),
        row(77_777, 9, 0),
        row(300_001, 6, 71_430),
        row(1_234_567, 3, 0),
        row(9_000_000, 1, 499_999),
    ];
    // Hand check of `derived`: rates 10,000 / 25,000 / 71,429 / 166,667 ppm;
    // cum = 0, 100,000·1.5% = 1,500, + 1,000,000·4.6429% = 47,929, + 5,000,000·9.5238% = 524,119.
    assert_eq!(binance_cums(&derived), vec![0, 1_500 * USD, 47_929 * USD, 524_119 * USD]);
    // Off-dollar floors make each step fractional: 333,333·0.1001% = 333.67 and 777,777·0.1001%
    // = 778.55 — floored ONCE the sum is 1,112; floored per step it would be 333 + 778 = 1,111.
    let odd = [
        Row { lb: 0, lev: 100, ppm: 1_000 },
        Row { lb: 333_333, lev: 100, ppm: 2_001 },
        Row { lb: 777_777, lev: 100, ppm: 3_002 },
    ];
    assert_eq!(binance_cums(&odd), vec![0, 333, 1_112]);
    for t in [&derived[..], &mixed[..], &odd[..]] {
        ok(&mut ctx, ADMIN, &set_tiers(TM, t));
        let g = tiers(&mut ctx, TM);
        assert_eq!(g.maintMarginRatesPpm, t.iter().map(|r| r.ppm).collect::<Vec<_>>());
        assert_eq!(
            g.effectiveMaintMarginRatesPpm,
            t.iter().map(|r| reported_ppm(*r)).collect::<Vec<_>>()
        );
        assert_eq!(g.cums, binance_cums(t));
        let mut caps: Vec<u64> = t[1..].iter().map(|r| r.lb).collect();
        caps.push(u64::MAX);
        assert_eq!(g.notionalCaps, caps);
    }
}

/// THE BINANCE IDENTITY, exactly: on an explicit table with whole-dollar floors (any Binance
/// table), the maintenance margin the engine enforces at notional `N` in tier `k` is
/// `⌊N · MMR_k / 1e6⌋ − cum_k` with `MMR_k` and `cum_k` read back from `getMarginTiers` — for a
/// long and a short, at every bound ±1 and across the whole range. It is also the slice form.
#[test]
fn p0_maint_margin_is_n_times_mmr_minus_cum_on_the_btc_table() {
    let mut ctx = fresh();
    ok(&mut ctx, ADMIN, &set_tiers(TM, &BTC));
    let g = tiers(&mut ctx, TM);
    let ps = probes(&BTC);
    assert!(ps.len() > 80);
    for n in ps {
        let k = tier_of(&BTC, n);
        assert!(g.lowerBounds[k] <= n && n < g.notionalCaps[k]);
        let binance =
            (n as u128 * g.effectiveMaintMarginRatesPpm[k] as u128 / ONE) as u64 - g.cums[k];
        for long in [true, false] {
            let mm = engine_mm(&mut ctx, n, long);
            assert_eq!(mm, binance, "N = {n} (tier {k}, long = {long})");
            assert_eq!(mm, slice_mm(&BTC, n), "N = {n}: slice form");
        }
    }
    // Continuity at a bound (no jump when a notional crosses into the next tier):
    // $250,000 → 250,000·1% − 1,300 = 1,200 = 250,000·0.5% − 50.
    assert_eq!(engine_mm(&mut ctx, 250_000 * USD, true), 1_200 * USD);
    assert_eq!(engine_mm(&mut ctx, 250_000 * USD - 1, true), 1_200 * USD - 1);
}

/// On DERIVED and mixed tables (rates that are not whole ppm, floors that are not whole dollars)
/// the engine enforces the slice form exactly, and Binance's `N · MMR − cum` computed from the
/// getter's reported (rounded-up) values can only OVERSTATE it — by less than one quote unit per
/// band reached, plus one for `cum`'s floor, plus `N / 1e6` for the rate's round-up.
#[test]
fn p0_maint_margin_is_the_slice_form_and_never_above_binance_on_derived_tables() {
    let mut ctx = fresh();
    let tables: [&[Row]; 4] = [
        &[row(0, 3, 0)],
        &[row(0, 50, 0), row(100_000, 20, 0), row(1_000_000, 7, 0), row(5_000_000, 3, 0)],
        &[
            row(0, 50, 0),
            Row { lb: 10_000 * USD + 333, lev: 40, ppm: 12_345 },
            row(77_777, 9, 0),
            Row { lb: 300_001 * USD + 1, lev: 6, ppm: 71_430 },
            row(1_234_567, 3, 0),
            row(9_000_000, 1, 499_999),
        ],
        &[row(0, 100, 0), row(1, 99, 0), row(2, 98, 0), row(3, 97, 7_777)],
    ];
    for t in tables {
        ok(&mut ctx, ADMIN, &set_tiers(TM, t));
        let g = tiers(&mut ctx, TM);
        for n in probes(t) {
            let mm = engine_mm(&mut ctx, n, n % 2 == 0);
            assert_eq!(mm, slice_mm(t, n), "N = {n}: slice form, table {t:?}");
            let k = tier_of(t, n);
            let binance_e6 = n as i128 * g.effectiveMaintMarginRatesPpm[k] as i128
                - g.cums[k] as i128 * ONE as i128;
            let gap = binance_e6 - mm as i128 * ONE as i128;
            assert!(gap >= 0, "N = {n}: Binance's form understates the engine by {gap}e-6");
            let bound = (k as i128 + 2) * ONE as i128 + n as i128;
            assert!(gap < bound, "N = {n}: gap {gap}e-6 ≥ {bound}e-6");
        }
    }
}

/// Derived-vs-explicit equivalence: wherever `1/(2L)` is a whole ppm, a table sent with the
/// column at 0 and the same table sent with that ppm spelled out are the SAME table — the
/// effective rates, `cum`, caps, every maintenance margin and every liquidation price agree; only
/// the stored column differs.
#[test]
fn p0_derived_and_explicit_tables_agree_where_the_rate_is_whole_ppm() {
    let mut ctx = fresh();
    let levs = [100u32, 50, 25, 20, 10, 5, 4, 2, 1];
    let floors =
        [0u64, 10_000, 50_000, 250_000, 1_000_000, 4_000_000, 20_000_000, 77_000_001, 150_000_000];
    let derived: Vec<Row> = levs.iter().zip(floors).map(|(&l, f)| row(f, l, 0)).collect();
    let explicit: Vec<Row> =
        levs.iter().zip(floors).map(|(&l, f)| row(f, l, 1_000_000 / (2 * l))).collect();
    let ns = probes(&derived);

    let mut read = |t: &[Row]| {
        ok(&mut ctx, ADMIN, &set_tiers(TM, t));
        let g = tiers(&mut ctx, TM);
        let mut mms = Vec::new();
        for &n in &ns {
            seed(&mut ctx, CAROL, TM, n as i64, 1);
            let a = risk(&mut ctx, CAROL, TM);
            seed(&mut ctx, CAROL, TM, -(n as i64), 1);
            let b = risk(&mut ctx, CAROL, TM);
            mms.push((a.maintMargin, a.liquidationPrice, b.maintMargin, b.liquidationPrice));
        }
        (g, mms)
    };
    let (gd, md) = read(&derived);
    let (ge, me) = read(&explicit);
    assert_eq!(gd.maintMarginRatesPpm, vec![0; 9]);
    assert_eq!(ge.maintMarginRatesPpm, explicit.iter().map(|r| r.ppm).collect::<Vec<_>>());
    assert_eq!(gd.effectiveMaintMarginRatesPpm, ge.maintMarginRatesPpm);
    assert_eq!(ge.effectiveMaintMarginRatesPpm, ge.maintMarginRatesPpm);
    assert_eq!(gd.cums, ge.cums);
    assert_eq!(gd.notionalCaps, ge.notionalCaps);
    assert_eq!(md, me, "maintenance margin / liquidation price differ between the spellings");
}

/// IM > MM is STRICT and EXACT: an explicit rate must sit below `1e6 / maxLev` as a real number,
/// so `333,333` passes at 3x (33.3333% < 33.3̅%), `333,334` does not; `1e6 / L` itself never does.
#[test]
fn p0_im_above_mm_is_strict_and_exact() {
    let mut ctx = fresh();
    for (lev, ok_ppm, bad_ppm) in [
        (1u32, 999_999u32, 1_000_000u32),
        (3, 333_333, 333_334),
        (7, 142_857, 142_858),
        (100, 9_999, 10_000),
    ] {
        rejected(&mut ctx, ADMIN, &set_tiers(TM, &[row(0, lev, bad_ppm)]), ET_IM_MM);
        rejected(&mut ctx, ADMIN, &set_tiers(TM, &[row(0, lev, u32::MAX)]), ET_IM_MM);
        ok(&mut ctx, ADMIN, &set_tiers(TM, &[row(0, lev, ok_ppm)]));
        assert_eq!(tiers(&mut ctx, TM).effectiveMaintMarginRatesPpm, vec![ok_ppm]);
        // A violation in a LATER tier refuses the whole table.
        let t = [row(0, 100, 1_000), row(1_000, lev, bad_ppm.max(1_000))];
        rejected(&mut ctx, ADMIN, &set_tiers(TM, &t), ET_IM_MM);
    }
}

/// Rates must be non-decreasing across the table, compared EXACTLY: a derived `1/6` (166,666.6̅
/// ppm) is above an explicit 166,666 and below an explicit 166,667. Equal rates are allowed.
#[test]
fn p0_rates_are_non_decreasing_compared_exactly() {
    let mut ctx = fresh();
    let refused: [[Row; 2]; 5] = [
        [row(0, 3, 0), row(100, 3, 166_666)], // 166,666.6̅ → 166,666
        [row(0, 3, 166_667), row(100, 3, 0)], // 166,667 → 166,666.6̅
        [row(0, 4, 0), row(100, 4, 124_999)], // 125,000 → 124,999
        [row(0, 10, 50_000), row(100, 10, 49_999)],
        [row(0, 5, 0), row(100, 5, 99_999)], // 100,000 → 99,999
    ];
    for t in refused {
        rejected(&mut ctx, ADMIN, &set_tiers(TM, &t), ET_RATE_ORDER);
    }
    let accepted: [[Row; 2]; 5] = [
        [row(0, 3, 0), row(100, 3, 166_667)],
        [row(0, 3, 166_666), row(100, 3, 0)],
        [row(0, 4, 0), row(100, 4, 125_000)], // equal, derived = explicit
        [row(0, 3, 0), row(100, 3, 0)],       // equal, both derived
        [row(0, 10, 50_000), row(100, 10, 50_000)],
    ];
    for t in accepted {
        ok(&mut ctx, ADMIN, &set_tiers(TM, &t));
        assert_eq!(tiers(&mut ctx, TM).maintMarginRatesPpm, vec![t[0].ppm, t[1].ppm]);
    }
    // A decrease later in a long table is caught too.
    let mut t = BTC;
    t[7].ppm = 124_999;
    rejected(&mut ctx, ADMIN, &set_tiers(TM, &t), ET_RATE_ORDER);
}

/// Every structural refusal rejects the WHOLE call with zero writes, and the stored table is
/// untouched; up to 16 tiers fit, 17 do not.
#[test]
fn p0_structural_refusals_reject_the_whole_table() {
    let mut ctx = fresh();
    ok(&mut ctx, ADMIN, &set_tiers(TM, &BTC));
    let before = tiers_raw(&mut ctx, TM);
    let raw = |lbs: Vec<u64>, levs: Vec<u32>, ppms: Vec<u32>| {
        setMarginTiersCall {
            marketId: TM,
            lowerBounds: lbs,
            maxLeverages: levs,
            maintMarginRatesPpm: ppms,
            notionalCap: 0,
        }
        .abi_encode()
    };
    rejected(&mut ctx, ADMIN, &set_tiers(NO_MARKET, &BTC), ET_MARKET);
    rejected(&mut ctx, ADMIN, &raw(vec![0, 100], vec![3], vec![0, 0]), ET_LEN);
    rejected(&mut ctx, ADMIN, &raw(vec![], vec![], vec![]), ET_EMPTY);
    rejected(&mut ctx, ADMIN, &raw(vec![1], vec![3], vec![0]), ET_FIRST);
    rejected(&mut ctx, ADMIN, &raw(vec![0, 100, 100], vec![3, 3, 3], vec![0; 3]), ET_BOUNDS);
    rejected(&mut ctx, ADMIN, &raw(vec![0, 100, 50], vec![3, 3, 3], vec![0; 3]), ET_BOUNDS);
    rejected(&mut ctx, ADMIN, &raw(vec![0], vec![0], vec![0]), ET_LEV);
    rejected(&mut ctx, ADMIN, &raw(vec![0], vec![151], vec![0]), ET_LEV);
    // One above Binance's highest bracket (BTCUSDT's 150x, the hard cap) is refused.
    rejected(
        &mut ctx,
        ADMIN,
        &raw(vec![0, 300_000 * USD], vec![151, 100], vec![4_000, 5_000]),
        ET_LEV,
    );
    rejected(&mut ctx, ADMIN, &raw(vec![0, 100], vec![3, 4], vec![0, 0]), ET_LEV_ORDER);
    rejected(&mut ctx, ADMIN, &raw(vec![0, 100], vec![3, 3], vec![0]), ET_RATE_LEN);
    rejected(&mut ctx, ADMIN, &raw(vec![0, 100], vec![3, 3], vec![0, 0, 0]), ET_RATE_LEN);
    rejected(&mut ctx, BOB, &set_tiers(TM, &BTC), E_AUTH);

    let sixteen: Vec<Row> =
        (0..16u64).map(|i| row(i * 1_000, 100 - i as u32, 1_000 + 100 * i as u32)).collect();
    let mut seventeen = sixteen.clone();
    seventeen.push(row(16_000, 80, 3_000));
    rejected(&mut ctx, ADMIN, &set_tiers(TM, &seventeen), ET_MANY);
    assert!(tiers_raw(&mut ctx, TM) == before, "a refused call changed the table");

    ok(&mut ctx, ADMIN, &set_tiers(TM, &sixteen));
    let g = tiers(&mut ctx, TM);
    assert_eq!(g.lowerBounds.len(), 16);
    assert_eq!(g.cums, binance_cums(&sixteen));
    for n in [0, 15_000 * USD - 1, 15_000 * USD + 5, 99_999 * USD] {
        assert_eq!(engine_mm(&mut ctx, n, true), slice_mm(&sixteen, n), "N = {n}");
    }
}

/// The getter's first three arrays are exactly what the setter takes: writing back what was read
/// is a no-op for the table (a derived tier stays derived — it is NOT turned into its rounded
/// effective rate).
#[test]
fn p0_the_getter_round_trips_through_the_setter() {
    let mut ctx = fresh();
    let t = [row(0, 20, 0), row(5_000, 7, 0), row(60_000, 3, 170_000), row(70_000, 2, 0)];
    ok(&mut ctx, ADMIN, &set_tiers(TM, &t));
    let raw = tiers_raw(&mut ctx, TM);
    let g = tiers(&mut ctx, TM);
    let back = setMarginTiersCall {
        marketId: TM,
        lowerBounds: g.lowerBounds.clone(),
        maxLeverages: g.maxLeverages.clone(),
        maintMarginRatesPpm: g.maintMarginRatesPpm.clone(),
        notionalCap: *g.notionalCaps.last().unwrap(),
    }
    .abi_encode();
    ok(&mut ctx, ADMIN, &back);
    assert!(tiers_raw(&mut ctx, TM) == raw, "writing back what was read changed the table");
    assert_eq!(g.maintMarginRatesPpm, vec![0, 0, 170_000, 0]);
}

/// The column is not just reported — it is what `liquidate()` enforces. ALICE is long 1 @ $100 at
/// 3x (margin 33,333,333); at mark $90 her equity is 23,333,333. Derived (1/6): MM = 15,000,000
/// → healthy, refused. Explicit 30 %: MM = 27,000,000 → liquidatable.
#[test]
fn p0_the_explicit_rate_is_what_liquidation_enforces() {
    let mut ctx = fresh();
    seed(&mut ctx, ALICE, M1, 1, 3);
    seed(&mut ctx, BOB, M1, -1, 3);
    storage::save_mark_price(&mut ctx, M1, 9_000).unwrap();
    let liq = liquidateCall { user: ALICE, marketId: M1 }.abi_encode();

    assert_eq!(risk(&mut ctx, ALICE, M1).maintMargin, 15_000_000);
    assert_eq!(risk(&mut ctx, ALICE, M1).isolatedMargin, 23_333_333);
    rejected(&mut ctx, ERIN, &liq, E_HEALTHY);

    ok(&mut ctx, ADMIN, &set_tiers(M1, &[row(0, 3, 300_000)]));
    assert_eq!(risk(&mut ctx, ALICE, M1).maintMargin, 27_000_000);
    let c = ok(&mut ctx, ERIN, &liq);
    let l = evs::<Liquidation>(&c.logs);
    assert_eq!(l.len(), 1);
    assert_eq!((l[0].user, l[0].marketId, l[0].markPrice), (ALICE, M1, 9_000));
    assert_eq!(amount(&mut ctx, ALICE, M1), 0, "the liquidation did not close ALICE");
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// P1 — margin type and setMarginType
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// The wire codes (§3.1, Q1) and Binance's `MARGIN_TYPE_CHANGE` reason code (§3.5).
#[test]
fn mt_wire_codes_and_reason_nine() {
    assert_eq!(MarginType::Crossed as u8, CROSSED);
    assert_eq!(MarginType::Isolated as u8, ISOLATED);
    assert_eq!(
        PerpPosition::default().margin_type,
        CROSSED,
        "zero-initialised = Binance's default"
    );
    assert_eq!(AccountUpdateReason::MarginTypeChange.code(), 9);
}

/// Gated off, every surface reports ISOLATED for every user in every market — a user who never
/// traded, one with a resting order (getAccount row), and one with a position (PositionChanged on
/// each fill) — although every stored record is the zero-initialised CROSSED.
#[test]
fn mt_gated_every_surface_reports_isolated() {
    let mut ctx = fresh();
    for m in [M1, M2] {
        let s = sym(&mut ctx, ERIN, m);
        assert_eq!((s.leverage, s.maxNotionalValue, s.marginType), (1, 0, ISOLATED));
        assert_eq!(risk(&mut ctx, ERIN, m).marginType, ISOLATED);
    }
    let out = view(&mut ctx, &getSymbolConfigCall { user: ERIN, marketId: NO_MARKET }.abi_encode());
    assert!(out.reverted, "getSymbolConfig of an unknown market must revert");

    rest(&mut ctx, CAROL, M2, BUY, 9_500, 1);
    let rows = account(&mut ctx, CAROL).positions;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].marketId, rows[0].marginType), (M2, ISOLATED));

    let c = open(&mut ctx, ALICE, M1, 2);
    let pc = evs::<PositionChanged>(&c.logs);
    assert!(pc.iter().any(|e| e.user == ALICE) && pc.iter().any(|e| e.user == DAVE));
    assert!(pc.iter().all(|e| e.marginType == ISOLATED), "PositionChanged.marginType");
    assert_eq!(reported(&mut ctx, ALICE, M1), ISOLATED);
    assert_eq!(storage::load_position(&mut ctx, ALICE, M1).unwrap().margin_type, CROSSED);
    let c = close(&mut ctx, ALICE, M1);
    assert!(evs::<PositionChanged>(&c.logs).iter().all(|e| e.marginType == ISOLATED));
}

/// Gated off, an ISOLATED request is `-4046` (it is already in force) — before the open-order and
/// position checks, which it precedes — and a CROSSED request walks −4047 → −4048 → the gate.
/// Nothing is ever written.
#[test]
fn mt_gated_the_ladder_ends_at_the_gate() {
    let mut ctx = fresh();
    refuse(&mut ctx, ALICE, M1, ISOLATED, E_4046);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_GATE);

    seed(&mut ctx, ALICE, M1, 3, 2);
    let id = rest(&mut ctx, ALICE, M1, BUY, 9_000, 1);
    refuse(&mut ctx, ALICE, M1, ISOLATED, E_4046);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4047);
    cancel(&mut ctx, ALICE, M1, id);
    refuse(&mut ctx, ALICE, M1, ISOLATED, E_4046);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4048);
    close(&mut ctx, ALICE, M1);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_GATE);
    refuse(&mut ctx, ALICE, M1, ISOLATED, E_4046);
    assert_eq!(storage::load_position(&mut ctx, ALICE, M1).unwrap().margin_type, CROSSED);
}

/// Refusal 0 (an invalid code) precedes everything, refusal 1 (an unknown market) everything
/// after it — gated and with the gate on.
#[test]
fn mt_an_invalid_type_then_an_unknown_market_come_first() {
    for gate in [false, true] {
        let mut ctx = fresh();
        let _g = gate.then(cross_on);
        if gate {
            switch_self(&mut ctx, ALICE, M1, ISOLATED);
        }
        seed(&mut ctx, ALICE, M1, 1, 1);
        rest(&mut ctx, ALICE, M1, BUY, 9_000, 1);
        for t in [2u8, 3, 0x80, 255] {
            refuse(&mut ctx, ALICE, NO_MARKET, t, E_TYPE);
            refuse(&mut ctx, ALICE, M1, t, E_TYPE);
            refuse(&mut ctx, ERIN, M2, t, E_TYPE);
        }
        for t in [CROSSED, ISOLATED] {
            refuse(&mut ctx, ALICE, NO_MARKET, t, E_MARKET);
            refuse(&mut ctx, ALICE, 0, t, E_MARKET);
        }
    }
}

/// With the gate on, the default is Binance's CROSSED (Q1): CROSSED is `-4046`, ISOLATED switches,
/// then ISOLATED is `-4046` and CROSSED switches back — each switch flat-gas, one
/// `MarginTypeChanged`, no balance row, leverage kept, no index entry, value conserved.
#[test]
fn mt_with_the_gate_on_the_default_is_crossed_and_it_switches_both_ways() {
    let mut ctx = fresh();
    let _g = cross_on();
    assert_eq!(reported(&mut ctx, ALICE, M1), CROSSED);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4046);
    switch_self(&mut ctx, ALICE, M1, ISOLATED);
    refuse(&mut ctx, ALICE, M1, ISOLATED, E_4046);
    switch_self(&mut ctx, ALICE, M1, CROSSED);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4046);
    switch_self(&mut ctx, ALICE, M1, ISOLATED);
    assert!(account(&mut ctx, ALICE).positions.is_empty());
    // Per (user, market): nobody else and no other market moved (§1.8: "will only apply to the
    // selected contract").
    assert_eq!(reported(&mut ctx, ALICE, M2), CROSSED);
    assert_eq!(reported(&mut ctx, BOB, M1), CROSSED);
    switch_self(&mut ctx, ALICE, M2, ISOLATED);
    switch_self(&mut ctx, BOB, M1, ISOLATED);
    assert_eq!(reported(&mut ctx, ALICE, M1), ISOLATED);
    // Dropping the gate: every surface reads ISOLATED again whatever is stored.
    drop(_g);
    assert_eq!(reported(&mut ctx, ALICE, M1), ISOLATED);
    assert_eq!(reported(&mut ctx, CAROL, M1), ISOLATED);
}

/// The full §3.6 ladder with the gate on, each refusal reached exactly when every earlier one
/// passes: a position AND a book order AND a conditional order are all open; peeling them off one
/// at a time walks −4047 (book) → −4047 (conditional) → −4048 → success.
#[test]
fn mt_with_the_gate_on_each_refusal_is_reached_only_past_the_earlier_ones() {
    let mut ctx = fresh();
    let _g = cross_on();
    for u in [ALICE, DAVE] {
        switch_self(&mut ctx, u, M1, ISOLATED);
    }
    open(&mut ctx, ALICE, M1, 2);
    let book = rest(&mut ctx, ALICE, M1, SELL, 10_500, 1);
    let stop = place_algo(&mut ctx, ALICE, &algo_call(M1, STOP_MARKET, SELL, 9_000, 0, 1, 0, RO));

    refuse(&mut ctx, ALICE, NO_MARKET, 7, E_TYPE);
    refuse(&mut ctx, ALICE, NO_MARKET, CROSSED, E_MARKET);
    refuse(&mut ctx, ALICE, M1, ISOLATED, E_4046);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4047);
    cancel(&mut ctx, ALICE, M1, book);
    refuse(&mut ctx, ALICE, M1, ISOLATED, E_4046);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4047);
    cancel_algo(&mut ctx, ALICE, M1, stop);
    refuse(&mut ctx, ALICE, M1, ISOLATED, E_4046);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4048);
    close(&mut ctx, ALICE, M1);
    refuse(&mut ctx, ALICE, M1, ISOLATED, E_4046);
    switch_self(&mut ctx, ALICE, M1, CROSSED);
}

/// −4047 counts EVERY open order in the market: a resting bid, a resting ask, an untriggered
/// (reduce-only) STOP_MARKET and TAKE_PROFIT, and the position-bound
/// closePosition stop and trailing stop — which stay listed after the position closes and still
/// block until cancelled (§3.6.3, F30: our conservative reading). Each is checked alone.
#[test]
fn mt_every_kind_of_open_order_blocks_the_switch() {
    let mut ctx = fresh();
    let _g = cross_on();
    for u in [ALICE, DAVE] {
        switch_self(&mut ctx, u, M1, ISOLATED);
    }
    // Flat, one order at a time.
    for side in [BUY, SELL] {
        let price = if side == BUY { 9_100 } else { 10_900 };
        let id = rest(&mut ctx, ALICE, M1, side, price, 1);
        refuse(&mut ctx, ALICE, M1, CROSSED, E_4047);
        cancel(&mut ctx, ALICE, M1, id);
    }
    // This build accepts only reduce-only / closePosition conditionals; a reduce-only one is not
    // bound to a position and can rest while flat.
    let ro_stop = algo_call(M1, STOP_MARKET, BUY, 11_000, 0, 1, 0, RO);
    let ro_tp = algo_call(M1, TAKE_PROFIT, BUY, 9_000, 9_000, 1, GTC, RO);
    for input in [ro_stop, ro_tp] {
        let id = place_algo(&mut ctx, ALICE, &input);
        assert_eq!(open_algos(&mut ctx, ALICE, M1), vec![id]);
        refuse(&mut ctx, ALICE, M1, CROSSED, E_4047);
        cancel_algo(&mut ctx, ALICE, M1, id);
    }
    // Position-bound orders: placed against a long, then the long is closed under them.
    open(&mut ctx, ALICE, M1, 1);
    let cp = place_algo(&mut ctx, ALICE, &algo_call(M1, STOP_MARKET, SELL, 9_000, 0, 0, 0, CP));
    let trail = place_algo(&mut ctx, ALICE, &algo_call(M1, TRAILING, SELL, 0, 100, 1, 0, RO));
    close(&mut ctx, ALICE, M1);
    assert_eq!(open_algos(&mut ctx, ALICE, M1).len(), 2, "both stay listed after the close");
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4047);
    cancel_algo(&mut ctx, ALICE, M1, cp);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4047);
    cancel_algo(&mut ctx, ALICE, M1, trail);
    assert!(open_algos(&mut ctx, ALICE, M1).is_empty());
    switch_self(&mut ctx, ALICE, M1, CROSSED);
}

/// A reduce-only resting order on its own also blocks (flat is impossible for it, so it is checked
/// against a position, where −4047 must still win over −4048).
#[test]
fn mt_a_reduce_only_order_blocks_with_4047_before_4048() {
    let mut ctx = fresh();
    let _g = cross_on();
    for u in [ALICE, DAVE] {
        switch_self(&mut ctx, u, M1, ISOLATED);
    }
    open(&mut ctx, ALICE, M1, -2);
    let c = ok(&mut ctx, ALICE, &order(M1, BUY, 9_200, 1, LIMIT, RO));
    let id = placeOrderCall::abi_decode_returns(&c.out.bytes).unwrap().0;
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4047);
    cancel(&mut ctx, ALICE, M1, id);
    refuse(&mut ctx, ALICE, M1, CROSSED, E_4048);
}

/// −4047 / −4048 look at the caller's own orders and position IN THAT MARKET only: another
/// market's orders and position, and another user's orders and position in the same market, do
/// not block. An order that FILLED is no longer open (−4048 from the position it made, not −4047).
#[test]
fn mt_orders_and_positions_block_only_their_own_user_and_market() {
    let mut ctx = fresh();
    let _g = cross_on();
    for u in [ALICE, BOB, DAVE] {
        for m in [M1, M2] {
            switch_self(&mut ctx, u, m, ISOLATED);
        }
    }
    // ALICE: an order and a position in M2; BOB: an order and a position in M1.
    rest(&mut ctx, ALICE, M2, BUY, 9_000, 1);
    open(&mut ctx, ALICE, M2, 3);
    rest(&mut ctx, BOB, M1, BUY, 9_000, 1);
    place_algo(&mut ctx, BOB, &algo_call(M1, STOP_MARKET, BUY, 11_000, 0, 1, 0, RO));
    open(&mut ctx, BOB, M1, -1);
    switch_self(&mut ctx, ALICE, M1, CROSSED);
    refuse(&mut ctx, ALICE, M2, CROSSED, E_4047);
    refuse(&mut ctx, BOB, M1, CROSSED, E_4047);

    // A resting order that fills is no longer an open order.
    switch_self(&mut ctx, CAROL, M2, ISOLATED);
    switch_self(&mut ctx, ERIN, M2, ISOLATED);
    rest(&mut ctx, CAROL, M2, SELL, 10_100, 2);
    refuse(&mut ctx, CAROL, M2, CROSSED, E_4047);
    take(&mut ctx, ERIN, M2, BUY, 2);
    assert_eq!(amount(&mut ctx, CAROL, M2), -2);
    refuse(&mut ctx, CAROL, M2, CROSSED, E_4048);
}

/// −4048: any non-zero position, long or short, including what is left after a partial close;
/// going flat lifts it.
#[test]
fn mt_any_position_blocks_and_flat_lifts_it() {
    let mut ctx = fresh();
    let _g = cross_on();
    for u in [ALICE, BOB, DAVE] {
        switch_self(&mut ctx, u, M1, ISOLATED);
    }
    for (u, q) in [(ALICE, 3i64), (BOB, -3)] {
        open(&mut ctx, u, M1, q);
        refuse(&mut ctx, u, M1, CROSSED, E_4048);
        open(&mut ctx, u, M1, -q.signum() * 2);
        assert_eq!(amount(&mut ctx, u, M1), q.signum());
        refuse(&mut ctx, u, M1, CROSSED, E_4048);
        close(&mut ctx, u, M1);
        switch_self(&mut ctx, u, M1, CROSSED);
    }
}

/// The type is a SETTING like leverage (§3.1): it survives open, close and reopen; every
/// PositionChanged of the round trip carries it; setMarginType never moves leverage and
/// setLeverage never moves the type.
#[test]
fn mt_the_type_persists_across_flat_like_leverage() {
    let mut ctx = fresh();
    let _g = cross_on();
    switch_self(&mut ctx, DAVE, M1, ISOLATED);
    ok(&mut ctx, ALICE, &setLeverageCall { marketId: M1, leverage: 2 }.abi_encode());
    switch_self(&mut ctx, ALICE, M1, ISOLATED);
    assert_eq!(sym(&mut ctx, ALICE, M1).leverage, 2);

    for round in 0..2 {
        let c = open(&mut ctx, ALICE, M1, 2);
        let mine: Vec<_> =
            evs::<PositionChanged>(&c.logs).into_iter().filter(|e| e.user == ALICE).collect();
        assert!(!mine.is_empty());
        assert!(mine.iter().all(|e| e.marginType == ISOLATED && e.leverage == 2), "round {round}");
        assert_eq!(reported(&mut ctx, ALICE, M1), ISOLATED);
        let c = close(&mut ctx, ALICE, M1);
        let last =
            evs::<PositionChanged>(&c.logs).into_iter().filter(|e| e.user == ALICE).last().unwrap();
        assert_eq!((last.amount, last.marginType), (0, ISOLATED));
        assert_eq!(reported(&mut ctx, ALICE, M1), ISOLATED, "round {round}: lost at flat");
        refuse(&mut ctx, ALICE, M1, ISOLATED, E_4046);
    }
    ok(&mut ctx, ALICE, &setLeverageCall { marketId: M1, leverage: 3 }.abi_encode());
    let s = sym(&mut ctx, ALICE, M1);
    assert_eq!((s.leverage, s.marginType), (3, ISOLATED));
    switch_self(&mut ctx, ALICE, M1, CROSSED);
    assert_eq!(sym(&mut ctx, ALICE, M1).leverage, 3);
}

/// …and a LIQUIDATION and an ADL do not move it either: only `setMarginType` writes the type. With
/// the gate on and everyone ISOLATED, ALICE (long 10 @ $100, 3x) is liquidated at mark $75
/// (equity 333.33 − 250 = 83.33 < MM 750 / 6 = 125). The book takes 4 (DAVE's bid at $74, inside
/// the ±10 % band), the ADL takes BOB's whole short of 3, and 3 are left — a PARTIAL liquidation.
/// Every party's record still says ISOLATED, every surface reports it, and every `PositionChanged`
/// the liquidation published carries it: the liquidated user (the funding write and the close
/// legs), the book counterparty, and the ADL winner (the winner leg).
#[test]
fn mt_liquidation_and_adl_leave_the_type_alone() {
    let mut ctx = fresh();
    let _g = cross_on();
    for u in [ALICE, BOB, DAVE] {
        switch_self(&mut ctx, u, M1, ISOLATED);
    }
    seed(&mut ctx, ALICE, M1, 10, 3);
    seed(&mut ctx, BOB, M1, -3, 3);
    storage::save_mark_price(&mut ctx, M1, 7_500).unwrap();
    rest(&mut ctx, DAVE, M1, BUY, 7_400, 4);

    let c = ok(&mut ctx, ERIN, &liquidateCall { user: ALICE, marketId: M1 }.abi_encode());
    let l = evs::<Liquidation>(&c.logs);
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].user, ALICE);
    let adl = evs::<Adl>(&c.logs);
    assert_eq!(adl.len(), 1, "the ADL leg must run");
    assert_eq!((adl[0].adlUser, adl[0].qty), (BOB, 3));
    assert_eq!(amount(&mut ctx, DAVE, M1), 4, "the book leg must run");
    assert_eq!(amount(&mut ctx, BOB, M1), 0);
    assert_eq!(amount(&mut ctx, ALICE, M1), 3, "a partial liquidation leaves ALICE open");

    let rows = evs::<PositionChanged>(&c.logs);
    for u in [ALICE, BOB, DAVE] {
        assert!(rows.iter().any(|e| e.user == u), "{u} published no PositionChanged");
    }
    assert!(
        rows.iter().all(|e| e.marginType == ISOLATED),
        "a liquidation or ADL row reported CROSSED: {:?}",
        rows.iter().map(|e| (e.user, e.marginType)).collect::<Vec<_>>()
    );
    for u in [ALICE, BOB, DAVE] {
        assert_eq!(storage::load_position(&mut ctx, u, M1).unwrap().margin_type, ISOLATED, "{u}");
        assert_eq!(reported(&mut ctx, u, M1), ISOLATED, "{u}");
    }
}

/// With the gate on every surface follows the stored choice, per market: a user CROSSED in M1 and
/// ISOLATED in M2 reads so in getSymbolConfig, getPositionRisk, both getAccount rows and the
/// PositionChanged of a fill in each. (The CROSSED fill CLOSES a seeded cross short — see
/// [`seed_cross`]; the ISOLATED one opens.)
#[test]
fn mt_with_the_gate_on_every_surface_follows_the_stored_choice() {
    let mut ctx = fresh();
    let _g = cross_on();
    for m in [M1, M2] {
        switch_self(&mut ctx, DAVE, m, ISOLATED);
    }
    switch_self(&mut ctx, ALICE, M2, ISOLATED);
    seed_cross(&mut ctx, ALICE, M1, -1);
    rest(&mut ctx, ALICE, M1, BUY, 9_500, 1);
    rest(&mut ctx, ALICE, M2, BUY, 9_500, 1);
    let rows = account(&mut ctx, ALICE).positions;
    let by: Vec<(u64, u8)> = rows.iter().map(|r| (r.marketId, r.marginType)).collect();
    assert_eq!(by, vec![(M1, CROSSED), (M2, ISOLATED)]);
    assert_eq!(sym(&mut ctx, ALICE, M1).marginType, CROSSED);
    assert_eq!(risk(&mut ctx, ALICE, M2).marginType, ISOLATED);
    for (m, t) in [(M1, CROSSED), (M2, ISOLATED)] {
        let c = take(&mut ctx, DAVE, m, SELL, 1);
        let e: Vec<_> =
            evs::<PositionChanged>(&c.logs).into_iter().filter(|e| e.user == ALICE).collect();
        assert!(!e.is_empty());
        assert!(e.iter().all(|e| e.marketId == m && e.marginType == t));
        assert!(evs::<PositionChanged>(&c.logs)
            .iter()
            .filter(|e| e.user == DAVE)
            .all(|e| e.marginType == ISOLATED));
    }
}

/// `setMarginType` is on the asSubAccount whitelist: a master switches its SUB's own record (the
/// event names the sub, the master's record is untouched), billed inner + 5,000; the sub's own
/// refusals come back verbatim; a non-master is refused; a signed selector is not whitelisted.
#[test]
fn mt_a_master_switches_its_sub_account() {
    let mut ctx = fresh();
    let out = ok(&mut ctx, ALICE, &createSubAccountCall { name: "desk".into() }.abi_encode()).out;
    let sub = createSubAccountCall::abi_decode_returns(&out.bytes).unwrap();
    enroll(&mut ctx, sub);
    let wrap = |m: u64, t: u8| {
        asSubAccountCall { subAccount: sub, data: set_type(m, t).into() }.abi_encode()
    };

    // Gated: the sub's refusals.
    let o = rejected(&mut ctx, ALICE, &wrap(M1, ISOLATED), E_4046);
    assert_eq!(o.gas_used, MT_GAS + SUB_OVERHEAD);
    rejected(&mut ctx, ALICE, &wrap(M1, CROSSED), E_GATE);
    rejected(&mut ctx, ALICE, &wrap(M1, 5), E_TYPE);
    rejected(&mut ctx, ALICE, &wrap(NO_MARKET, ISOLATED), E_MARKET);

    let _g = cross_on();
    // The wrong caller still pays the full inner charge (gas is decided from the calldata).
    let o = rejected(&mut ctx, BOB, &wrap(M1, ISOLATED), E_NOT_MASTER);
    assert_eq!(o.gas_used, MT_GAS + SUB_OVERHEAD);
    rejected(&mut ctx, sub, &wrap(M1, ISOLATED), E_NOT_MASTER);
    switch(&mut ctx, ALICE, sub, &wrap(M1, ISOLATED), M1, ISOLATED, MT_GAS + SUB_OVERHEAD, false);
    assert_eq!(reported(&mut ctx, ALICE, M1), CROSSED, "the master's own record moved");
    rejected(&mut ctx, ALICE, &wrap(M1, ISOLATED), E_4046);

    // The sub's open order blocks the sub, not the master.
    let place =
        asSubAccountCall { subAccount: sub, data: order(M1, BUY, 9_000, 1, LIMIT, 0).into() }
            .abi_encode();
    ok(&mut ctx, ALICE, &place);
    rejected(&mut ctx, ALICE, &wrap(M1, CROSSED), E_4047);
    switch_self(&mut ctx, ALICE, M1, ISOLATED);

    // The master's own signed selector is not reachable through the envelope.
    let signed = setMarginTypeSignedCall {
        account: sub,
        marketId: M1,
        marginType: CROSSED,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 1,
        signature: vec![0u8; 64].into(),
    }
    .abi_encode();
    let inner = asSubAccountCall { subAccount: sub, data: signed.into() }.abi_encode();
    let o = rejected(&mut ctx, ALICE, &inner, E_NOT_WHITELISTED);
    assert_eq!(o.gas_used, SUB_OVERHEAD, "a non-whitelisted inner selector pays the envelope only");
}

// ── The signed path ──────────────────────────────────────────────────────────────────────────

const SIGNED_TS: u64 = 1; // == the default test block's timestamp
const SIGNED_RECV: u64 = 60;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn register(ctx: &mut TestCtx, user: Address, key_id: u8, sk: &SigningKey, expiry: u64) {
    ok(
        ctx,
        user,
        &registerApiKeyCall { keyId: key_id, pubkey: sk.verifying_key().to_bytes().into(), expiry }
            .abi_encode(),
    );
}

/// The ABI's message: `"perpdex_v1_margintype"(21) || chainId(8) || account(20) || marketId(8)
/// || marginType(1) || timestamp(8) || recvWindow(8) || keyId(1)` — 75 bytes.
#[derive(Clone, Copy)]
struct Msg {
    domain: &'static [u8],
    chain: u64,
    account: Address,
    market: u64,
    t: u8,
    ts: u64,
    recv: u64,
    key_id: u8,
}

impl Msg {
    fn new(ctx: &TestCtx, account: Address, market: u64, t: u8) -> Self {
        Msg {
            domain: b"perpdex_v1_margintype",
            chain: ctx.chain_id(),
            account,
            market,
            t,
            ts: SIGNED_TS,
            recv: SIGNED_RECV,
            key_id: 1,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(self.domain);
        m.extend_from_slice(&self.chain.to_be_bytes());
        m.extend_from_slice(self.account.as_slice());
        m.extend_from_slice(&self.market.to_be_bytes());
        m.push(self.t);
        m.extend_from_slice(&self.ts.to_be_bytes());
        m.extend_from_slice(&self.recv.to_be_bytes());
        m.push(self.key_id);
        m
    }

    /// Calldata carrying THIS message's fields, signed by `sk` over `signed` (normally the same
    /// message; a different one to test what the signature binds).
    fn call_signed_over(&self, sk: &SigningKey, signed: &Msg) -> Vec<u8> {
        let sig = sk.sign(&signed.bytes()).to_bytes().to_vec();
        setMarginTypeSignedCall {
            account: self.account,
            marketId: self.market,
            marginType: self.t,
            timestamp: self.ts,
            recvWindow: self.recv,
            keyId: self.key_id,
            signature: sig.into(),
        }
        .abi_encode()
    }

    fn call(&self, sk: &SigningKey) -> Vec<u8> {
        self.call_signed_over(sk, self)
    }
}

#[test]
fn mt_the_signed_message_is_75_bytes() {
    let ctx = fresh();
    assert_eq!(Msg::new(&ctx, ALICE, M1, ISOLATED).bytes().len(), 75);
}

/// Relayed by anyone, a signed switch changes the SIGNING account's record (not the relayer's),
/// exactly like the direct call; the same signature is then a duplicate — even resubmitted by the
/// account itself, and even though a fresh signature for the reverse switch succeeds.
#[test]
fn mt_signed_switch_succeeds_once_and_replays_are_duplicates() {
    let mut ctx = fresh();
    let sk = key(9);
    register(&mut ctx, ALICE, 1, &sk, 0);
    let _g = cross_on();
    let input = Msg::new(&ctx, ALICE, M1, ISOLATED).call(&sk);
    switch(&mut ctx, ERIN, ALICE, &input, M1, ISOLATED, MT_GAS, true);
    assert_eq!(reported(&mut ctx, ERIN, M1), CROSSED, "the relayer's record moved");
    rejected(&mut ctx, ERIN, &input, E_DUP);
    rejected(&mut ctx, ALICE, &input, E_DUP);
    let back = Msg { ts: SIGNED_TS + 1, ..Msg::new(&ctx, ALICE, M1, CROSSED) }.call(&sk);
    switch(&mut ctx, BOB, ALICE, &back, M1, CROSSED, MT_GAS, true);
    rejected(&mut ctx, ERIN, &back, E_DUP);
    // A replay of the FIRST signature would now be a real change (CROSSED → ISOLATED): still refused.
    rejected(&mut ctx, ERIN, &input, E_DUP);
    assert_eq!(reported(&mut ctx, ALICE, M1), CROSSED);
}

/// Refusals that are permanent for the message come BEFORE the burn — zero writes, and the same
/// calldata gets the same answer again (never "duplicate"): an invalid type, no key, a bad
/// signature, a timestamp in the future or outside recvWindow, an expired key, an unknown market.
/// Their documented order: type → key → (window / expiry / signature) → duplicate → market.
#[test]
fn mt_signed_permanent_refusals_precede_the_burn() {
    let mut ctx = fresh();
    let sk = key(9);
    let stranger = key(4);
    register(&mut ctx, ALICE, 1, &sk, 0);
    let base = Msg::new(&ctx, ALICE, M1, ISOLATED);

    // Refusal 0, even for an account with no key and an unknown market.
    rejected_before_burn(&mut ctx, &Msg { t: 2, ..base }.call(&sk), E_TYPE);
    rejected_before_burn(
        &mut ctx,
        &Msg { t: 9, account: CAROL, market: NO_MARKET, ..base }.call(&sk),
        E_TYPE,
    );
    // No key: before the signature is even looked at.
    rejected_before_burn(&mut ctx, &Msg { account: CAROL, ..base }.call(&sk), E_NO_KEY);
    rejected_before_burn(&mut ctx, &Msg { key_id: 2, ..base }.call(&sk), E_NO_KEY);
    // A signature by the wrong key / over other bytes.
    rejected_before_burn(&mut ctx, &base.call(&stranger), E_BAD_SIG);
    // The signature is checked before the market.
    rejected_before_burn(&mut ctx, &Msg { market: NO_MARKET, ..base }.call(&stranger), E_BAD_SIG);
    // Unknown market, validly signed: pre-burn too.
    rejected_before_burn(&mut ctx, &Msg { market: NO_MARKET, ..base }.call(&sk), E_MARKET);
    // Time: in the future beyond the 5 s skew, and outside recvWindow.
    rejected_before_burn(&mut ctx, &Msg { ts: SIGNED_TS + 6, ..base }.call(&sk), E_FUTURE);
    ctx.block.timestamp = U256::from(100);
    rejected_before_burn(&mut ctx, &base.call(&sk), E_STALE);
    // An expired api key.
    let short = key(5);
    register(&mut ctx, BOB, 1, &short, 150);
    ctx.block.timestamp = U256::from(150);
    let bob = Msg { account: BOB, ts: 150, ..base };
    rejected_before_burn(&mut ctx, &bob.call(&short), E_KEY_EXPIRED);

    // None of them spent anything: a fresh, valid message for ALICE goes through the ladder to its
    // first STATE refusal (gated: ISOLATED is −4046) — after the burn.
    let fresh_msg = Msg { ts: 150, ..base };
    rejected_after_burn(&mut ctx, &fresh_msg.call(&sk), E_4046);
}

/// Refusals that read the owner's state or the gate (−4046, −4047, −4048, the gate) come AFTER
/// the burn, with the direct call's strings: the signature is spent, nothing else moves, and the
/// same calldata is a duplicate even once the blocking state is gone.
#[test]
fn mt_signed_state_refusals_follow_the_burn() {
    let mut ctx = fresh();
    let sk = key(9);
    register(&mut ctx, ALICE, 1, &sk, 0);
    // Fresh signatures: one timestamp each (block time 10, all inside recvWindow).
    let mut ts = SIGNED_TS;
    let mut next = |ctx: &TestCtx, t: u8| {
        ts += 1;
        Msg { ts, ..Msg::new(ctx, ALICE, M1, t) }
    };

    // Gated.
    ctx.block.timestamp = U256::from(10);
    let m = next(&ctx, ISOLATED);
    rejected_after_burn(&mut ctx, &m.call(&sk), E_4046);
    let gate = next(&ctx, CROSSED);
    rejected_after_burn(&mut ctx, &gate.call(&sk), E_GATE);
    let id = rest(&mut ctx, ALICE, M1, BUY, 9_000, 1);
    let blocked = next(&ctx, CROSSED);
    rejected_after_burn(&mut ctx, &blocked.call(&sk), E_4047);
    cancel(&mut ctx, ALICE, M1, id);
    seed(&mut ctx, ALICE, M1, 1, 1);
    let pos = next(&ctx, CROSSED);
    rejected_after_burn(&mut ctx, &pos.call(&sk), E_4048);

    // With the blockers gone and the gate on, those spent signatures stay spent.
    close(&mut ctx, ALICE, M1);
    let _g = cross_on();
    for spent in [&gate, &blocked, &pos] {
        rejected(&mut ctx, ERIN, &spent.call(&sk), E_DUP);
    }
    let same = next(&ctx, CROSSED).call(&sk);
    rejected_after_burn(&mut ctx, &same, E_4046);
    let fine = next(&ctx, ISOLATED);
    switch(&mut ctx, ERIN, ALICE, &fine.call(&sk), M1, ISOLATED, MT_GAS, true);
}

/// The signature binds every field: the same signature with any one calldata field changed is a
/// signature failure (pre-burn), as is a signature over another chain id or another domain (the
/// leverage domain with the same tail); the original then still succeeds.
#[test]
fn mt_the_signature_binds_every_field() {
    let mut ctx = fresh();
    let sk = key(9);
    register(&mut ctx, ALICE, 1, &sk, 0);
    register(&mut ctx, ALICE, 2, &sk, 0);
    register(&mut ctx, BOB, 1, &sk, 0); // the same key under another account
    let _g = cross_on();
    let good = Msg::new(&ctx, ALICE, M1, ISOLATED);
    let variants = [
        Msg { account: BOB, ..good },
        Msg { market: M2, ..good },
        Msg { t: CROSSED, ..good },
        Msg { ts: SIGNED_TS + 1, ..good },
        Msg { recv: SIGNED_RECV - 1, ..good },
        Msg { key_id: 2, ..good },
    ];
    for v in variants {
        rejected_before_burn(&mut ctx, &v.call_signed_over(&sk, &good), E_BAD_SIG);
    }
    let other_chain = Msg { chain: good.chain + 1, ..good };
    rejected_before_burn(&mut ctx, &good.call_signed_over(&sk, &other_chain), E_BAD_SIG);
    // Same layout, the 19-byte leverage domain: a different message, so no signature reuse.
    let other_domain = Msg { domain: b"perpdex_v1_leverage", ..good };
    rejected_before_burn(&mut ctx, &good.call_signed_over(&sk, &other_domain), E_BAD_SIG);
    switch(&mut ctx, ERIN, ALICE, &good.call(&sk), M1, ISOLATED, MT_GAS, true);
}

/// A sub-account signs for itself: its key is registered through the asSubAccount envelope, and
/// a signed switch for the SUB (relayed by anyone) moves the sub's record only.
#[test]
fn mt_a_sub_account_signs_its_own_switch() {
    let mut ctx = fresh();
    let out = ok(&mut ctx, ALICE, &createSubAccountCall { name: "bot".into() }.abi_encode()).out;
    let sub = createSubAccountCall::abi_decode_returns(&out.bytes).unwrap();
    let sk = key(7);
    let reg =
        registerApiKeyCall { keyId: 1, pubkey: sk.verifying_key().to_bytes().into(), expiry: 0 };
    ok(
        &mut ctx,
        ALICE,
        &asSubAccountCall { subAccount: sub, data: reg.abi_encode().into() }.abi_encode(),
    );
    let _g = cross_on();
    let m = Msg::new(&ctx, sub, M2, ISOLATED);
    switch(&mut ctx, ERIN, sub, &m.call(&sk), M2, ISOLATED, MT_GAS, true);
    assert_eq!(reported(&mut ctx, ALICE, M2), CROSSED);
    // The sub's key does not sign for the master.
    let master = Msg::new(&ctx, ALICE, M2, ISOLATED).call(&sk);
    rejected_before_burn(&mut ctx, &master, E_NO_KEY);
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// While gated, nothing else changed
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// An isolated scenario on M1 exercising open, add margin, a mark move, a partial close at a
/// loss, a second mark move, a full close and a margin removal. Returns every log of every call
/// and every getter of every participant afterwards.
fn isolated_scenario(ctx: &mut TestCtx) -> (Vec<Vec<Log>>, Vec<Vec<u8>>) {
    fn step(ctx: &mut TestCtx, who: Address, input: Vec<u8>) -> Vec<Log> {
        let c = ok(ctx, who, &input);
        assert!(evs::<PositionChanged>(&c.logs).iter().all(|e| e.marginType == ISOLATED));
        c.logs
    }
    let logs = vec![
        step(ctx, DAVE, order(M1, SELL, 10_000, 10, LIMIT, 0)),
        step(ctx, ALICE, setLeverageCall { marketId: M1, leverage: 2 }.abi_encode()),
        step(ctx, ALICE, order(M1, BUY, 0, 4, MKT, 0)),
        step(ctx, CAROL, order(M1, BUY, 0, 3, MKT, 0)),
        step(ctx, ALICE, addPositionMarginCall { marketId: M1, amount: 1_000_000 }.abi_encode()),
        tick(ctx, M1, 9_800).logs,
        step(ctx, BOB, order(M1, BUY, 9_800, 5, LIMIT, 0)),
        step(ctx, ALICE, order(M1, SELL, 0, 2, MKT, 0)),
        tick(ctx, M1, 10_200).logs,
        step(ctx, CAROL, order(M1, SELL, 0, 3, MKT, 0)),
        step(ctx, ALICE, removePositionMarginCall { marketId: M1, amount: 500_000 }.abi_encode()),
    ];

    let mut views = Vec::new();
    for u in [ALICE, BOB, CAROL, DAVE] {
        views.push(view(ctx, &getAccountCall { user: u }.abi_encode()).bytes.to_vec());
        views.push(
            view(ctx, &getPositionRiskCall { user: u, marketId: M1 }.abi_encode()).bytes.to_vec(),
        );
        views.push(
            view(ctx, &getSymbolConfigCall { user: u, marketId: M1 }.abi_encode()).bytes.to_vec(),
        );
    }
    views.push(enc(&storage::load_insurance_fund(ctx).unwrap()));
    assert_eq!(amount(ctx, CAROL, M1), 0);
    assert_eq!(amount(ctx, ALICE, M1), 2);
    (logs, views)
}

/// Gated, refused switch attempts — direct, through a signed call (which burns its signature) and
/// for every participant — leave the scenario byte-identical: every log of every call and every
/// getter.
#[test]
fn gated_refused_switch_attempts_change_nothing() {
    let mut a = fresh();
    let baseline = isolated_scenario(&mut a);

    let mut b = fresh();
    let sk = key(9);
    register(&mut b, ALICE, 1, &sk, 0);
    for u in [ALICE, BOB, CAROL, DAVE] {
        refuse(&mut b, u, M1, ISOLATED, E_4046);
        refuse(&mut b, u, M1, CROSSED, E_GATE);
    }
    let signed = Msg::new(&b, ALICE, M1, CROSSED).call(&sk);
    rejected_after_burn(&mut b, &signed, E_GATE);
    let attempted = isolated_scenario(&mut b);
    assert!(attempted.0 == baseline.0, "the logs differ after refused switch attempts");
    assert!(attempted.1 == baseline.1, "the getters differ after refused switch attempts");
}

/// Gated, the STORED byte has no effect: actors whose records say ISOLATED (written with the gate
/// on, then the gate dropped) run the scenario byte-identically to actors at the default CROSSED.
#[test]
fn gated_the_stored_byte_has_no_effect() {
    let mut a = fresh();
    let baseline = isolated_scenario(&mut a);

    let mut c = fresh();
    {
        let _g = cross_on();
        for u in [ALICE, BOB, CAROL, DAVE] {
            switch_self(&mut c, u, M1, ISOLATED);
        }
    }
    for u in [ALICE, BOB, CAROL, DAVE] {
        assert_eq!(storage::load_position(&mut c, u, M1).unwrap().margin_type, ISOLATED);
    }
    let stored = isolated_scenario(&mut c);
    assert!(stored.0 == baseline.0, "the logs depend on the stored byte while gated");
    assert!(stored.1 == baseline.1, "the getters depend on the stored byte while gated");
    // …and the byte survived the scenario (open, close, margin moves).
    assert_eq!(storage::load_position(&mut c, CAROL, M1).unwrap().margin_type, ISOLATED);
}

/// With the gate ON and every actor explicitly ISOLATED, the scenario is byte-identical to the
/// gated one: ISOLATED is today's semantics, and the gate changes nothing for it.
#[test]
fn gated_and_explicitly_isolated_with_the_gate_on_are_the_same() {
    let mut a = fresh();
    let baseline = isolated_scenario(&mut a);

    let mut d = fresh();
    let _g = cross_on();
    for u in [ALICE, BOB, CAROL, DAVE] {
        switch_self(&mut d, u, M1, ISOLATED);
    }
    let on = isolated_scenario(&mut d);
    assert!(on.0 == baseline.0, "the logs differ with the gate on and everyone ISOLATED");
    assert!(on.1 == baseline.1, "the getters differ with the gate on and everyone ISOLATED");
}
