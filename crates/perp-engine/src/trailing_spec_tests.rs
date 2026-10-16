//! **The `fire()` capacity fix, closePosition parity and trailing stops — an independent spec
//! suite.**
//!
//! Written from `SPEC_closepos_trailing.md` (owner decisions 1–8 and commits 1–4) and the owner memo
//! at the end of `TRAIL_designs.md` (§0, §1.3, §2.5–§2.8), NOT from the implementation's own tests
//! (`trailing::tests`, `risk::tests::{algo_fire_capacity, close_position_parity}`). Exact reject
//! strings and ABI shapes come from the published ABI docs (`interface.rs`); every number asserted
//! below is worked out by hand from the spec formulas in the comment next to it:
//!
//! ```text
//! SELL fires   10000·mark ≤ H·(10000 − b)      thr = ⌊H(10000−b)/10000⌋, stop = thr ↓ tick
//! BUY  fires   10000·mark ≥ L·(10000 + b)      thr = ⌈L(10000+b)/10000⌉, stop = thr ↑ tick
//! H / L        the best mark since ACTIVATION (inclusive of the activation mark)
//! activation   SELL mark ≥ A, BUY mark ≤ A (inclusive); A = 0 → now, at the stored mark
//! ```
//!
//! Every call goes through the full shell (`run_perp_dex_call`), so the gas table, the dispatch,
//! the commit-only #23 tripwire and the event guards are in the loop. Every SUCCESSFUL call is
//! checked for value conservation (`Σ wallet + margin + vQuote + IF` over every account the test
//! created, to the unit); every REJECTED call for the exact reason, zero perp writes, no log and a
//! bit-identical world.
//!
//! The oracle in this file publishes `mark == index` exactly: the market has no funding
//! (`price1 = index`) and the contract-price leg is pinned to the index by clearing `last_traded`
//! before each tick, so `median(index, price2, index) = index` whatever the basis window holds. The
//! mark formula is not under test here; [`tick`] asserts the identity after every update.
//!
//! Three tests drive an [`InMemoryHost`] behind a counting wrapper ([`Counting`]) instead of the
//! revm journal, ending the simulated block before each measured tick so every read is cold: that
//! turns "the per-tick work does not depend on N" into a count of distinct storage objects the
//! host served.
//!
//! # Map (test prefix → spec item)
//!
//! * `c1_*` — commit 1, the `fire()` capacity fix: market / closePosition / quantity stops past a
//!   full- or half-size reduce-only TP, the post-fill eviction (6, 5), the prefix rule kept for a
//!   fired GTC limit, a trailing stop in the same bracket, truncation to the position at fire.
//! * `c2_*` — commit 2, closePosition parity: RO|CP refused, "would immediately trigger" for all
//!   eight (type, side) pairs before and after the first tick, binding at placement, the lazy
//!   expiry after flat / flip / close-and-reopen (as taker and as maker), resizes that keep it,
//!   the plain reduce-only stop as the unbound contrast.
//! * `t_*` — commit 3, trailing stops: every placement rule (grid, activation side, flags,
//!   quantity, tif / workingType, tick grid, both caps), activation (now, on touch, through a gap,
//!   before the first tick), the exact predicate on hand-computed SELL and BUY paths, staggered
//!   activations, tick-rounded stops, many buckets in one tick, budget overflow + drain, fixed
//!   triggers first, not latched, the activation cap (also across levels and past a tick), cancels
//!   anywhere, liquidation cancel-all, the signed path, STP inheritance, per-tick work vs N, the
//!   decline-then-spike path and its garbage, and a seeded random differential run against an
//!   O(N) per-order model written here from the formulas above ([`Model`]).
//! * `fix_*` — regressions for the review findings on this series, each pinning its fix (one of
//!   them replaces the `#[ignore]`d `bug_*` test this suite first kept for a finding outside it).
//! * `kill_*` — one test per mutant that survived the suite above, named after it.

use std::cell::RefCell;
use std::collections::HashSet;

use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use ed25519_dalek::{Signer, SigningKey};
use primitives::{address, hardfork::SpecId, keccak256, Address, FixedBytes, B256, U256};

use crate::call::PerpOutput;
use crate::host::{InMemoryHost, PerpHost};
use crate::interface::IPerpDex::{
    batchCancelOrdersCall, cancelAlgoOrderCall, cancelOrderCall, cancelOrderSignedCall,
    drainDeferredWorkCall, getAlgoOrderCall, getAlgoOrderReturn,
    getOpenAlgoOrdersCall, placeAlgoOrderCall, placeAlgoOrderSignedCall, placeOrderCall,
    registerApiKeyCall, updateIndexPriceCall, AlgoOrderActivated, AlgoOrderCancelled,
    AlgoOrderPlaced, AlgoOrderTriggered, Liquidation, OrderCancelled, OrderPlaced, Trade,
};
use crate::types::{AccountUpdateReason, MarginTiers, Market, PerpPosition};
use crate::{run_perp_dex_call, storage, PERP_DEX_ADDRESS, USDC_ADDRESS};

// ── Fixture ──────────────────────────────────────────────────────────────────────────────────

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;
type Log = primitives::Log;
type Id = [u8; 32];

/// The protected position in most scenarios.
const ALICE: Address = address!("1111111111111111111111111111111111111111");
/// Makers / the liquidity a close would hit.
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
const DAVE: Address = address!("4444444444444444444444444444444444444444");
/// A bystander: counter-position, keeper, the taker that hits a maker.
const ERIN: Address = address!("5555555555555555555555555555555555555555");
/// Admin = oracle.
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const MARKET_ID: u64 = 1;
/// $100.00 at price_decimals 2.
const MARK: u64 = 10_000;
/// $1e9: margin never interferes.
const WALLET: i64 = 1_000_000_000_000_000;
const GAS: u64 = 30_000_000;
/// Seconds between oracle ticks (the market's `priceUpdateInterval`).
const INTERVAL: u64 = 15;

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MKT: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;

/// `flags` bit 0 / bit 1, and the STP mode in bits 3:2.
const RO: u8 = 0b0001;
const CP: u8 = 0b0010;
const STP_TAKER: u8 = 0b0100;
const STP_BOTH: u8 = 0b1000;

/// `AlgoOrderType`.
const STOP: u8 = 0;
const STOP_MARKET: u8 = 1;
const TAKE_PROFIT: u8 = 2;
const TAKE_PROFIT_MARKET: u8 = 3;
const TRAILING: u8 = 4;

/// `CancelReason`.
const R_USER: u8 = 0;
const R_LIQUIDATION: u8 = 3;
const R_RO_SHRANK: u8 = 5;
const R_RO_WOULD_OPEN: u8 = 6;
const R_STP: u8 = 9;
const R_POSITION_CLOSED: u8 = 11;

/// The shared sweep budget (fixed triggers + trailing fires + liquidations) and the trailing
/// activation cap, as the ABI docs state them.
const BUDGET: usize = 50;
const ACTIVATION_CAP: usize = 64;

// Reject reasons (the ABI docs' wording).
const E_RO_CP: &str = "placeAlgoOrder: reduceOnly cannot be sent with closePosition";
const E_IMMEDIATE: &str = "placeAlgoOrder: order would immediately trigger";
const E_CP_BIND: &str =
    "placeAlgoOrder: closePosition requires an open position on the closing side";
const E_TRAIL_BIND: &str =
    "placeAlgoOrder: TRAILING_STOP_MARKET requires an open position on the closing side";
const E_CALLBACK: &str =
    "placeAlgoOrder: callbackRate (price) must be 10..=1000 bps in steps of 10";
const E_TRAIL_RO: &str = "placeAlgoOrder: TRAILING_STOP_MARKET requires reduceOnly";
const E_TRAIL_CP: &str =
    "placeAlgoOrder: closePosition is not supported for TRAILING_STOP_MARKET";
const E_QTY_ZERO: &str = "placeAlgoOrder: quantity must be non-zero";
const E_QTY_STEP: &str = "placeAlgoOrder: quantity must be a multiple of stepSize";
const E_QTY_MAX: &str = "placeAlgoOrder: quantity exceeds maxQuantity";
const E_TIF: &str = "placeAlgoOrder: tif is unused for a *_MARKET type, send 0";
const E_WORKING: &str =
    "placeAlgoOrder: workingType must be MARK_PRICE (CONTRACT_PRICE is not supported)";
const E_TICK: &str = "placeAlgoOrder: triggerPrice must be a multiple of tickSize";
const E_UNKNOWN_BITS: &str = "placeAlgoOrder: unknown flag bits";
const E_STP_MODE: &str = "placeAlgoOrder: invalid self-trade prevention mode";
const E_KIND: &str = "placeAlgoOrder: invalid orderType";
const E_ALGO_TYPE: &str = "placeAlgoOrder: algoType must be CONDITIONAL";
const E_ALGO_MAX: &str = "placeAlgoOrder: too many open conditional orders in this market";
const E_ALGO_CAP: &str = "placeAlgoOrder: max open orders reached";
const E_PLACE_CAP: &str = "placeOrder: max open orders reached";
const E_VIEW_NOT_FOUND: &str = "getAlgoOrder: not found";
const E_CANCEL_NOT_FOUND: &str = "cancelAlgoOrder: not found";
const E_CANCEL_OWNER: &str = "cancelAlgoOrder: not the owner";
const E_CANCEL_MARKET: &str = "cancelAlgoOrder: marketId mismatch";
const E_DUP_SIG: &str = "placeAlgoOrderSigned: duplicate signature (already submitted)";

thread_local! {
    /// Every account a test enrolled — the conservation and world snapshots run over these.
    static USERS: RefCell<Vec<Address>> = const { RefCell::new(Vec::new()) };
}

fn users() -> Vec<Address> {
    USERS.with(|u| u.borrow().clone())
}

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ADMIN, Address::ZERO] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

fn the_market(tick: u64, step: u64, min_qty: u64) -> Market {
    Market {
        market_id: MARKET_ID,
        base_decimals: 0,
        price_decimals: 2,
        tick_size: tick,
        step_size: step,
        min_quantity: min_qty,
        max_quantity: 1_000_000,
        max_price: 1_000_000,
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
    }
}

/// Market 1 (base 0, price 2 decimals, the given tick / step / minQuantity, no funding, the default
/// ±10 % fill band, maintenance 1/6) at `mark`, before any oracle tick, the admin set, and a $1e9
/// perp wallet for ALICE, BOB, CAROL, DAVE and ERIN. No fees.
fn market(tick: u64, step: u64, min_qty: u64, mark: u64) -> TestCtx {
    USERS.with(|u| *u.borrow_mut() = vec![ADMIN, Address::ZERO]);
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(&mut ctx, &the_market(tick, step, min_qty)).unwrap();
    storage::save_mark_price(&mut ctx, MARKET_ID, mark).unwrap();
    for u in [ALICE, BOB, CAROL, DAVE, ERIN] {
        enroll(&mut ctx, u);
    }
    let _ = take_logs(&mut ctx);
    ctx
}

fn fresh() -> TestCtx {
    market(1, 1, 1, MARK)
}

/// Loads `u`, gives it a $1e9 perp wallet and adds it to the snapshots.
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

/// The `k`-th extra account.
fn extra(k: u16) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0x77;
    b[18..20].copy_from_slice(&k.to_be_bytes());
    Address::from(b)
}

fn new_user(ctx: &mut TestCtx, k: u16) -> Address {
    let u = extra(k);
    enroll(ctx, u);
    u
}

/// Seeds `user` with `amount` contracts entered at the CURRENT stored mark, OFF-BOOK (so
/// `last_traded` stays 0), at `leverage`: `margin = |amount|·notional / leverage`. On this base-0 /
/// price-2 market one contract at price `P` is worth `P·10^4` quote units.
fn seed_pos(ctx: &mut TestCtx, user: Address, amount: i64, leverage: u64) {
    let m = storage::load_mark_price(ctx, MARKET_ID).unwrap() as i64;
    storage::save_position(
        ctx,
        user,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance: -amount * m * 10_000,
            margin: amount.abs() * m * 10_000 / leverage as i64,
            leverage,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
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

/// `Σ (perp wallet + position margin + vQuote)` over every enrolled account, plus the insurance
/// fund. A fill moves `calc_value` between two vQuotes and nothing here charges a fee, so no call
/// may move it by a single unit.
fn value(ctx: &mut TestCtx) -> i128 {
    let mut v = storage::load_insurance_fund(ctx).unwrap() as i128;
    for u in users() {
        let a = storage::load_account(ctx, u).unwrap();
        let p = storage::load_position(ctx, u, MARKET_ID).unwrap();
        v += a.perp_wallet_balance as i128 + p.margin as i128 + p.v_quote_balance as i128;
    }
    v
}

struct Call {
    out: PerpOutput,
    logs: Vec<Log>,
}

/// One SUCCESSFUL shell call, value-conserving to the unit.
fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Call {
    let _ = take_logs(ctx);
    let before = value(ctx);
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(!out.reverted, "unexpected revert: {}", revert_reason(&out));
    let logs = take_logs(ctx);
    assert_eq!(value(ctx), before, "value not conserved (Σ wallet + margin + vQuote + IF)");
    Call { out, logs }
}

fn enc<T: serde::Serialize>(v: &T) -> Vec<u8> {
    perp_core::codec::encode(v).unwrap()
}

/// Everything a rejected call must leave bit-identical.
fn world(ctx: &mut TestCtx) -> Vec<Vec<u8>> {
    let mut w = Vec::new();
    for u in users() {
        w.push(enc(&storage::load_account(ctx, u).unwrap()));
        w.push(enc(&storage::load_position(ctx, u, MARKET_ID).unwrap()));
        w.push(enc(&storage::load_buy_orders(ctx, u, MARKET_ID).unwrap()));
        w.push(enc(&storage::load_sell_orders(ctx, u, MARKET_ID).unwrap()));
        w.push(enc(&storage::load_user_algo_orders(ctx, u, MARKET_ID).unwrap()));
    }
    w.push(enc(&storage::load_bid_prices(ctx, MARKET_ID).unwrap()));
    w.push(enc(&storage::load_ask_prices(ctx, MARKET_ID).unwrap()));
    w.push(enc(&storage::load_insurance_fund(ctx).unwrap()));
    w.push(enc(&storage::load_index_price_state(ctx, MARKET_ID).unwrap()));
    w
}

/// A REJECTED call: the exact reason, no perp write, no log, the world bit-identical.
fn rejected(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
    rejected_burning(ctx, caller, input, expect, 0);
}

/// [`rejected`], with exactly `burn` perp writes allowed (a signed call's signature burn).
fn rejected_burning(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str, burn: u64) {
    let _ = take_logs(ctx);
    let before = world(ctx);
    let writes = ctx.perp_write_count();
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(out.reverted, "expected the revert `{expect}`, but the call SUCCEEDED");
    assert_eq!(revert_reason(&out), expect);
    assert_eq!(ctx.perp_write_count() - writes, burn, "[{expect}] perp writes");
    assert!(take_logs(ctx).is_empty(), "[{expect}] the rejected call logged");
    assert!(world(ctx) == before, "[{expect}] the rejected call changed stored state");
}

fn view(ctx: &mut TestCtx, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, GAS, ERIN, U256::ZERO, true, ctx).unwrap()
}

// ── Calldata ─────────────────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn algo_raw(
    algo_type: u8,
    kind: u8,
    side: u8,
    trigger: u64,
    price: u64,
    qty: u64,
    tif: u8,
    working: u8,
    flags: u8,
) -> Vec<u8> {
    placeAlgoOrderCall {
        algoType: algo_type,
        marketId: MARKET_ID,
        side,
        orderType: kind,
        triggerPrice: trigger,
        price,
        quantity: qty,
        tif,
        workingType: working,
        clientAlgoId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

fn algo_call(kind: u8, side: u8, trigger: u64, price: u64, qty: u64, tif: u8, flags: u8) -> Vec<u8> {
    algo_raw(0, kind, side, trigger, price, qty, tif, 0, flags)
}

/// TRAILING_STOP_MARKET: `triggerPrice` = activation price (0 = now), `price` = callback bps.
fn trail_call(side: u8, activation: u64, cb: u64, qty: u64, flags: u8) -> Vec<u8> {
    algo_call(TRAILING, side, activation, cb, qty, 0, flags)
}

/// A closePosition STOP_MARKET.
fn cp_stop_call(side: u8, trigger: u64) -> Vec<u8> {
    algo_call(STOP_MARKET, side, trigger, 0, 0, 0, CP)
}

/// A reduce-only STOP_MARKET of `qty`.
fn ro_stop_call(side: u8, trigger: u64, qty: u64) -> Vec<u8> {
    algo_call(STOP_MARKET, side, trigger, 0, qty, 0, RO)
}

fn order_call(side: u8, price: u64, qty: u64, kind: u8, tif: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: kind,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

// ── Shortcuts ────────────────────────────────────────────────────────────────────────────────

fn place_algo(ctx: &mut TestCtx, user: Address, input: &[u8]) -> (Id, Call) {
    let c = ok(ctx, user, input);
    let id = placeAlgoOrderCall::abi_decode_returns(&c.out.bytes).unwrap().0;
    (id, c)
}

/// A reduce-only trailing stop of `qty`.
fn trail(ctx: &mut TestCtx, user: Address, side: u8, activation: u64, cb: u64, qty: u64) -> Id {
    place_algo(ctx, user, &trail_call(side, activation, cb, qty, RO)).0
}

fn rest_flags(ctx: &mut TestCtx, user: Address, side: u8, price: u64, qty: u64, flags: u8) -> Id {
    let c = ok(ctx, user, &order_call(side, price, qty, LIMIT, GTC, flags));
    assert!(trades(&c.logs).is_empty(), "{user} {side}@{price} traded while resting");
    placeOrderCall::abi_decode_returns(&c.out.bytes).unwrap().0
}

/// A plain GTC limit that must rest untouched.
fn rest(ctx: &mut TestCtx, user: Address, side: u8, price: u64, qty: u64) -> Id {
    rest_flags(ctx, user, side, price, qty, 0)
}

/// A reduce-only GTC limit (a resting take-profit) that must rest untouched.
fn rest_ro(ctx: &mut TestCtx, user: Address, side: u8, price: u64, qty: u64) -> Id {
    rest_flags(ctx, user, side, price, qty, RO)
}

/// A plain market order.
fn take(ctx: &mut TestCtx, user: Address, side: u8, qty: u64) -> Call {
    ok(ctx, user, &order_call(side, 0, qty, MKT, GTC, 0))
}

fn cancel_algo(ctx: &mut TestCtx, user: Address, id: Id) -> Call {
    ok(ctx, user, &cancelAlgoOrderCall { algoId: id.into(), marketId: MARKET_ID }.abi_encode())
}

fn algo(ctx: &mut TestCtx, id: Id) -> getAlgoOrderReturn {
    let out = view(ctx, &getAlgoOrderCall { algoId: id.into() }.abi_encode());
    assert!(!out.reverted, "getAlgoOrder reverted: {}", revert_reason(&out));
    getAlgoOrderCall::abi_decode_returns(&out.bytes).unwrap()
}

/// `getAlgoOrder` no longer knows `id` (cancelled, spent, or fired and terminal).
fn algo_gone(ctx: &mut TestCtx, id: Id) {
    let out = view(ctx, &getAlgoOrderCall { algoId: id.into() }.abi_encode());
    assert_eq!(revert_reason(&out), E_VIEW_NOT_FOUND);
}

/// `(activated, peakPrice, currentStop)` — the appended trailing fields.
fn trail_state(ctx: &mut TestCtx, id: Id) -> (bool, u64, u64) {
    let r = algo(ctx, id);
    assert_eq!(r.orderType, TRAILING);
    (r.activated, r.peakPrice, r.currentStop)
}

fn open_algos(ctx: &mut TestCtx, user: Address) -> Vec<Id> {
    let out = view(ctx, &getOpenAlgoOrdersCall { user, marketId: MARKET_ID }.abi_encode());
    assert!(!out.reverted);
    getOpenAlgoOrdersCall::abi_decode_returns(&out.bytes)
        .unwrap()
        .into_iter()
        .map(|b| b.0)
        .collect()
}

fn amount(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_position(ctx, user, MARKET_ID).unwrap().amount
}

fn current_ts(ctx: &mut TestCtx) -> u64 {
    storage::load_index_price_state(ctx, MARKET_ID).unwrap().timestamp
}

/// One oracle tick at `index`, one interval after the last; publishes `mark == index` (module doc).
fn tick(ctx: &mut TestCtx, index: u64) -> Call {
    storage::save_last_traded_price(ctx, MARKET_ID, 0).unwrap();
    let ts = current_ts(ctx) + INTERVAL;
    let c = ok(
        ctx,
        ADMIN,
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: ts }.abi_encode(),
    );
    assert_eq!(current_ts(ctx), ts, "the tick was not applied");
    assert_eq!(storage::load_mark_price(ctx, MARKET_ID).unwrap(), index, "mark != index");
    c
}

/// `drainDeferredWork` by a keeper: `(moreRemaining, the call)`.
fn drain(ctx: &mut TestCtx) -> (bool, Call) {
    let c = ok(ctx, ERIN, &drainDeferredWorkCall { marketId: MARKET_ID }.abi_encode());
    let more = drainDeferredWorkCall::abi_decode_returns(&c.out.bytes).unwrap();
    (more, c)
}

fn deferred(ctx: &mut TestCtx) -> bool {
    storage::load_market_ref(ctx, MARKET_ID).unwrap().unwrap().deferred_work != 0
}

// ── Events ───────────────────────────────────────────────────────────────────────────────────

fn evs<E: SolEvent>(logs: &[Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

/// `(algoId, triggerPrice, markPrice)` of every `AlgoOrderTriggered`, in stream order.
fn triggered(logs: &[Log]) -> Vec<(Id, u64, u64)> {
    evs::<AlgoOrderTriggered>(logs)
        .into_iter()
        .map(|e| {
            assert_eq!(e.marketId, MARKET_ID);
            (e.algoId.0, e.triggerPrice, e.markPrice)
        })
        .collect()
}

/// `(algoId, markPrice, stopPrice)` of every `AlgoOrderActivated`, in stream order.
fn activations(logs: &[Log]) -> Vec<(Id, u64, u64)> {
    evs::<AlgoOrderActivated>(logs)
        .into_iter()
        .map(|e| {
            assert_eq!(e.marketId, MARKET_ID);
            (e.algoId.0, e.markPrice, e.stopPrice)
        })
        .collect()
}

/// `(algoId, reason)` of every `AlgoOrderCancelled`.
fn algo_cancels(logs: &[Log]) -> Vec<(Id, u8)> {
    evs::<AlgoOrderCancelled>(logs).into_iter().map(|e| (e.algoId.0, e.reason)).collect()
}

/// `(orderId, reason)` of every book `OrderCancelled`.
fn book_cancels(logs: &[Log]) -> Vec<(Id, u8)> {
    evs::<OrderCancelled>(logs).into_iter().map(|e| (e.orderId.0, e.reason)).collect()
}

/// `(taker, maker, price, qty)` of every `Trade`.
fn trades(logs: &[Log]) -> Vec<(Address, Address, u64, u64)> {
    evs::<Trade>(logs).into_iter().map(|e| (e.taker, e.maker, e.price, e.quantity)).collect()
}

/// `(orderId, side, quantity, orderType, flags)` of every `OrderPlaced`.
fn placed(logs: &[Log]) -> Vec<(Id, u8, u64, u8, u8)> {
    evs::<OrderPlaced>(logs)
        .into_iter()
        .map(|e| (e.orderId.0, e.side, e.quantity, e.orderType, e.flags))
        .collect()
}

/// How many of the four conditional-order events a stream carries.
fn algo_event_count(logs: &[Log]) -> usize {
    evs::<AlgoOrderPlaced>(logs).len()
        + evs::<AlgoOrderTriggered>(logs).len()
        + evs::<AlgoOrderActivated>(logs).len()
        + evs::<AlgoOrderCancelled>(logs).len()
}

fn ids_of(v: &[(Id, u64, u64)]) -> Vec<Id> {
    v.iter().map(|x| x.0).collect()
}

/// The spec formulas, for the places that need them computed rather than written out.
fn spec_thr(sell: bool, peak: u64, cb: u64) -> u64 {
    let p = peak as u128;
    if sell {
        (p * (10_000 - cb as u128) / 10_000) as u64
    } else {
        (p * (10_000 + cb as u128)).div_ceil(10_000) as u64
    }
}

fn spec_fires(sell: bool, mark: u64, peak: u64, cb: u64) -> bool {
    let m = mark as u128 * 10_000;
    if sell {
        m <= peak as u128 * (10_000 - cb as u128)
    } else {
        m >= peak as u128 * (10_000 + cb as u128)
    }
}

fn spec_stop(sell: bool, peak: u64, cb: u64, tick: u64) -> u64 {
    let t = spec_thr(sell, peak, cb);
    if sell {
        t / tick * tick
    } else {
        t.div_ceil(tick) * tick
    }
}

#[test]
fn the_hand_computed_numbers_satisfy_the_spec_formulas() {
    // Self-check of the arithmetic quoted throughout this file, against the formulas themselves.
    assert_eq!(spec_thr(true, 10_123, 10), 10_112);
    assert_eq!(spec_thr(true, 10_123, 70), 10_052);
    assert_eq!(spec_thr(true, 10_123, 250), 9_869);
    assert_eq!(spec_thr(true, 10_123, 1000), 9_110);
    assert_eq!(spec_thr(true, 10_200, 250), 9_945);
    assert_eq!(spec_thr(false, 9_877, 10), 9_887);
    assert_eq!(spec_thr(false, 9_877, 130), 10_006);
    assert_eq!(spec_thr(false, 9_800, 500), 10_290);
    assert_eq!(spec_thr(false, 9_800, 1000), 10_780);
    assert_eq!(spec_stop(true, 10_130, 70, 10), 10_050);
    assert_eq!(spec_stop(false, 9_870, 130, 10), 10_000);
    assert!(spec_fires(true, 10_112, 10_123, 10) && !spec_fires(true, 10_113, 10_123, 10));
    assert!(spec_fires(true, 9_945, 10_200, 250) && !spec_fires(true, 9_946, 10_200, 250));
    assert!(spec_fires(false, 10_006, 9_877, 130) && !spec_fires(false, 10_005, 9_877, 130));
    assert!(spec_fires(false, 10_290, 9_800, 500) && !spec_fires(false, 10_289, 9_800, 500));
    // L4: a peak never fires at itself.
    for cb in (10..=1000).step_by(10) {
        for p in [1u64, 7, 999, 10_000, 1_000_000] {
            assert!(!spec_fires(true, p, p, cb) && !spec_fires(false, p, p, cb));
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Commit 1 — a triggered MARKET order is bounded by the position only (memo §0)
// ═════════════════════════════════════════════════════════════════════════════════════════════
//
// The bracket: ALICE holds 10, a reduce-only take-profit rests on the closing side far from the
// mark, a maker rests 10 at the stop price, and a stop on the closing side fires on the next tick.
// Long: TP SELL @ 10_500, BOB BUY 10 @ 9_900, stop 9_900. Short: TP BUY @ 9_500, CAROL SELL 10 @
// 10_100, stop 10_100. Every price is inside the ±10 % band of the post-tick mark.

#[derive(Clone, Copy, Debug)]
enum Dir {
    Long,
    Short,
}

const DIRS: [Dir; 2] = [Dir::Long, Dir::Short];

impl Dir {
    fn sign(self) -> i64 {
        match self {
            Dir::Long => 1,
            Dir::Short => -1,
        }
    }
    /// The position's closing side.
    fn close(self) -> u8 {
        match self {
            Dir::Long => SELL,
            Dir::Short => BUY,
        }
    }
    fn open(self) -> u8 {
        1 - self.close()
    }
    /// The stop trigger, the tick that fires it and the maker's price.
    fn stop(self) -> u64 {
        match self {
            Dir::Long => 9_900,
            Dir::Short => 10_100,
        }
    }
    fn tp(self) -> u64 {
        match self {
            Dir::Long => 10_500,
            Dir::Short => 9_500,
        }
    }
    fn maker(self) -> Address {
        match self {
            Dir::Long => BOB,
            Dir::Short => CAROL,
        }
    }
}

/// The bracket fixture; returns the TP's id.
fn bracket(d: Dir, tp_qty: u64) -> (TestCtx, Id) {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10 * d.sign(), 1);
    seed_pos(&mut ctx, ERIN, -10 * d.sign(), 1);
    let tp = rest_ro(&mut ctx, ALICE, d.close(), d.tp(), tp_qty);
    rest(&mut ctx, d.maker(), d.open(), d.stop(), 10);
    (ctx, tp)
}

fn placed_qty(logs: &[Log], id: Id) -> Vec<u64> {
    placed(logs).into_iter().filter(|p| p.0 == id).map(|p| p.2).collect()
}

/// Memo §0's probe, both ways round: with a FULL-size (10) or half-size (5) reduce-only TP resting,
/// a closePosition STOP_MARKET and a quantity-10 STOP_MARKET each close all 10 — a market order
/// fills before every resting order, so only `|position|` bounds it — and the TP left behind on a
/// flat position is evicted afterwards as `ReduceOnlyWouldOpen` (6).
#[test]
fn c1_a_market_stop_closes_the_whole_position_past_a_resting_reduce_only_tp() {
    for d in DIRS {
        for tp_qty in [10, 5] {
            for cp in [true, false] {
                let tag = format!("{d:?} tp={tp_qty} cp={cp}");
                let (mut ctx, tp) = bracket(d, tp_qty);
                let input = if cp {
                    cp_stop_call(d.close(), d.stop())
                } else {
                    ro_stop_call(d.close(), d.stop(), 10)
                };
                let sl = place_algo(&mut ctx, ALICE, &input).0;
                let c = tick(&mut ctx, d.stop());
                assert_eq!(triggered(&c.logs), vec![(sl, d.stop(), d.stop())], "{tag}");
                assert_eq!(placed_qty(&c.logs, sl), vec![10], "{tag}: placed for the whole position");
                assert_eq!(trades(&c.logs), vec![(ALICE, d.maker(), d.stop(), 10)], "{tag}");
                assert_eq!(amount(&mut ctx, ALICE), 0, "{tag}");
                assert_eq!(book_cancels(&c.logs), vec![(tp, R_RO_WOULD_OPEN)], "{tag}: TP evicted");
                assert!(algo_cancels(&c.logs).is_empty(), "{tag}: the stop was not expired");
                assert!(open_algos(&mut ctx, ALICE).is_empty(), "{tag}");
            }
        }
    }
}

/// A partial market stop (4 of 10) closes 4; the full-size TP no longer fits the 6 left and is
/// evicted by the post-fill pass as `ReduceOnlyPositionShrank` (5) — "trimmed per restore".
#[test]
fn c1_a_partial_market_stop_closes_its_quantity_and_the_tp_is_trimmed_after_the_fill() {
    for d in DIRS {
        let (mut ctx, tp) = bracket(d, 10);
        let sl = place_algo(&mut ctx, ALICE, &ro_stop_call(d.close(), d.stop(), 4)).0;
        let c = tick(&mut ctx, d.stop());
        assert_eq!(triggered(&c.logs), vec![(sl, d.stop(), d.stop())], "{d:?}");
        assert_eq!(trades(&c.logs), vec![(ALICE, d.maker(), d.stop(), 4)], "{d:?}");
        assert_eq!(amount(&mut ctx, ALICE), 6 * d.sign(), "{d:?}");
        assert_eq!(book_cancels(&c.logs), vec![(tp, R_RO_SHRANK)], "{d:?}");
    }
}

/// The spec keeps the PREFIX rule for a fired limit that RESTS: a GTC STOP at the maker's price
/// behind a full-size reduce-only TP has no room (10 − 0 ahead − 10 reduce-only = 0) and is spent
/// as reason 6 with nothing placed; behind a half-size TP it gets exactly the 5 that fit, crosses
/// the maker and the TP (5 ≤ 5 left) stays.
#[test]
fn c1_a_fired_gtc_limit_still_obeys_the_prefix_rule() {
    for d in DIRS {
        let (mut ctx, tp) = bracket(d, 10);
        let sl = place_algo(
            &mut ctx,
            ALICE,
            &algo_call(STOP, d.close(), d.stop(), d.stop(), 10, GTC, RO),
        )
        .0;
        let c = tick(&mut ctx, d.stop());
        assert_eq!(triggered(&c.logs), vec![(sl, d.stop(), d.stop())], "{d:?}");
        assert_eq!(algo_cancels(&c.logs), vec![(sl, R_RO_WOULD_OPEN)], "{d:?}");
        assert!(placed(&c.logs).is_empty() && trades(&c.logs).is_empty(), "{d:?}");
        assert_eq!(amount(&mut ctx, ALICE), 10 * d.sign(), "{d:?}");
        assert!(book_cancels(&c.logs).is_empty(), "{d:?}: the TP is untouched");
        let resting: Vec<Id> = if d.close() == SELL {
            storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID).unwrap()
        } else {
            storage::load_buy_orders(&mut ctx, ALICE, MARKET_ID).unwrap()
        }
        .iter()
        .map(|e| e.order_id)
        .collect();
        assert_eq!(resting, vec![tp], "{d:?}");

        let (mut ctx, _tp) = bracket(d, 5);
        let sl = place_algo(
            &mut ctx,
            ALICE,
            &algo_call(STOP, d.close(), d.stop(), d.stop(), 10, GTC, RO),
        )
        .0;
        let c = tick(&mut ctx, d.stop());
        assert_eq!(placed_qty(&c.logs, sl), vec![5], "{d:?}: exactly what fits");
        assert_eq!(trades(&c.logs), vec![(ALICE, d.maker(), d.stop(), 5)], "{d:?}");
        assert_eq!(amount(&mut ctx, ALICE), 5 * d.sign(), "{d:?}");
        assert!(book_cancels(&c.logs).is_empty(), "{d:?}: TP 5 fits the 5 left");
    }
}

/// ⚠️ Pins the implementation's DOCUMENTED deviation from the spec's literal "limit fires keep
/// today's prefix logic": a fired limit IOC fills immediately, so it is bounded like `placeOrder`'s
/// own IOC — by `|position|` alone (the ABI docs: "the `_MARKET` pair, or a limit IOC / FOK").
#[test]
fn c1_a_fired_limit_ioc_is_bounded_like_an_immediate_order() {
    for d in DIRS {
        let (mut ctx, tp) = bracket(d, 10);
        let sl = place_algo(
            &mut ctx,
            ALICE,
            &algo_call(STOP, d.close(), d.stop(), d.stop(), 10, IOC, RO),
        )
        .0;
        let c = tick(&mut ctx, d.stop());
        assert_eq!(trades(&c.logs), vec![(ALICE, d.maker(), d.stop(), 10)], "{d:?}");
        assert_eq!(amount(&mut ctx, ALICE), 0, "{d:?}");
        assert_eq!(placed_qty(&c.logs, sl), vec![10], "{d:?}");
        assert_eq!(book_cancels(&c.logs), vec![(tp, R_RO_WOULD_OPEN)], "{d:?}");
    }
}

/// The reason the fix is a prerequisite (memo §0): a trailing stop is always a market fire, so next
/// to a full-size reduce-only TP it must close everything too. Peak 10_000, b = 100: SELL stop
/// ⌊10_000·0.99⌋ = 9_900, BUY stop ⌈10_000·1.01⌉ = 10_100 — exactly the bracket's stop prices.
#[test]
fn c1_a_trailing_stop_next_to_a_full_size_tp_closes_the_whole_position() {
    for d in DIRS {
        let (mut ctx, tp) = bracket(d, 10);
        let ts = trail(&mut ctx, ALICE, d.close(), 0, 100, 10);
        let c = tick(&mut ctx, d.stop());
        assert_eq!(triggered(&c.logs), vec![(ts, d.stop(), d.stop())], "{d:?}");
        assert_eq!(trades(&c.logs), vec![(ALICE, d.maker(), d.stop(), 10)], "{d:?}");
        assert_eq!(amount(&mut ctx, ALICE), 0, "{d:?}");
        assert_eq!(book_cancels(&c.logs), vec![(tp, R_RO_WOULD_OPEN)], "{d:?}");
    }
}

/// A trailing stop's quantity is a request bounded by the position AT FIRE TIME: 1_000 against a
/// position of 10 places 10; after the position shrank to 6 (without closing — same position) a
/// quantity-10 trailer places 6.
#[test]
fn c1_a_trailing_fire_is_truncated_to_the_position_at_fire_time() {
    for d in DIRS {
        let (mut ctx, _tp) = bracket(d, 1);
        let ts = trail(&mut ctx, ALICE, d.close(), 0, 100, 1_000);
        // The TP (1) is the only other closing order: still bounded by |position| = 10 alone.
        let c = tick(&mut ctx, d.stop());
        assert_eq!(placed_qty(&c.logs, ts), vec![10], "{d:?}");
        assert_eq!(amount(&mut ctx, ALICE), 0, "{d:?}");

        let mut ctx = fresh();
        seed_pos(&mut ctx, ALICE, 10 * d.sign(), 1);
        seed_pos(&mut ctx, ERIN, -10 * d.sign(), 1);
        let ts = trail(&mut ctx, ALICE, d.close(), 0, 100, 10);
        // Reduce by 4 at the mark (ALICE is the taker), keeping the position open.
        rest(&mut ctx, d.maker(), d.open(), MARK, 4);
        take(&mut ctx, ALICE, d.close(), 4);
        assert_eq!(amount(&mut ctx, ALICE), 6 * d.sign());
        rest(&mut ctx, DAVE, d.open(), d.stop(), 20);
        let c = tick(&mut ctx, d.stop());
        assert_eq!(triggered(&c.logs), vec![(ts, d.stop(), d.stop())], "{d:?}");
        assert_eq!(placed_qty(&c.logs, ts), vec![6], "{d:?}");
        assert_eq!(trades(&c.logs), vec![(ALICE, DAVE, d.stop(), 6)], "{d:?}");
        assert!(algo_cancels(&c.logs).is_empty(), "{d:?}: same position, not expired");
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Commit 2 — closePosition parity (decisions 1–3, memo §1.3)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// Binance: reduceOnly "cannot be sent with closePosition=true". Refused for both closePosition
/// types, both sides, with or without a quantity — and for a trailing stop the exclusivity is the
/// FIRST flag rule it meets. Each bit alone still places.
#[test]
fn c2_reduce_only_and_close_position_together_are_refused() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10, 1);
    seed_pos(&mut ctx, CAROL, -10, 1);
    // (user, side, STOP_MARKET trigger, TAKE_PROFIT_MARKET trigger): none would fire at 10_000.
    for (user, side, stop_t, tp_t) in [(ALICE, SELL, 9_500, 10_500), (CAROL, BUY, 10_500, 9_500)] {
        for (kind, t) in [(STOP_MARKET, stop_t), (TAKE_PROFIT_MARKET, tp_t)] {
            for qty in [0, 5] {
                rejected(&mut ctx, user, &algo_call(kind, side, t, 0, qty, 0, RO | CP), E_RO_CP);
            }
            rejected(
                &mut ctx,
                user,
                &algo_call(kind, side, t, 0, 0, 0, RO | CP | STP_BOTH),
                E_RO_CP,
            );
        }
        rejected(&mut ctx, user, &trail_call(side, 0, 100, 5, RO | CP), E_RO_CP);
        // Each bit alone is fine.
        let (cp, _) = place_algo(&mut ctx, user, &algo_call(STOP_MARKET, side, stop_t, 0, 0, 0, CP));
        let r = algo(&mut ctx, cp);
        assert!(r.closePosition && !r.reduceOnly && r.quantity == 0);
        let (ro, _) = place_algo(&mut ctx, user, &ro_stop_call(side, stop_t, 5));
        let r = algo(&mut ctx, ro);
        assert!(!r.closePosition && r.reduceOnly && r.quantity == 5);
    }
}

/// `(STOP|STOP_MARKET, BUY)` and `(TAKE_PROFIT|TAKE_PROFIT_MARKET, SELL)` fire UP (mark ≥ trigger),
/// the other four DOWN.
fn fires_up(kind: u8, side: u8) -> bool {
    matches!((kind, side), (STOP | STOP_MARKET, BUY) | (TAKE_PROFIT | TAKE_PROFIT_MARKET, SELL))
}

/// Binance `-2021` for every conditional type and direction, against the STORED mark — the
/// market's initial mark before the first oracle tick, the tick's mark after it. Equality counts
/// as "would trigger" (the sweep fires at equality). One tick past the mark on the far side places.
#[test]
fn c2_a_conditional_that_would_fire_on_the_stored_mark_is_refused() {
    for first_tick in [None, Some(9_500u64)] {
        let mut ctx = fresh();
        if let Some(px) = first_tick {
            tick(&mut ctx, px);
        }
        let m = first_tick.unwrap_or(MARK);
        for kind in [STOP, STOP_MARKET, TAKE_PROFIT, TAKE_PROFIT_MARKET] {
            for side in [BUY, SELL] {
                let up = fires_up(kind, side);
                let limit = kind == STOP || kind == TAKE_PROFIT;
                let call = |t: u64| algo_call(kind, side, t, if limit { t } else { 0 }, 1, GTC, RO);
                let refused: [u64; 4] =
                    if up { [m, m - 1, m - 100, 1] } else { [m, m + 1, m + 100, 1_000_000] };
                for t in refused {
                    rejected(&mut ctx, ALICE, &call(t), E_IMMEDIATE);
                }
                let fine = if up { m + 1 } else { m - 1 };
                let (id, c) = place_algo(&mut ctx, ALICE, &call(fine));
                assert_eq!(evs::<AlgoOrderPlaced>(&c.logs)[0].triggerPrice, fine);
                assert_eq!(algo(&mut ctx, id).triggerPrice, fine);
            }
        }
        assert_eq!(open_algos(&mut ctx, ALICE).len(), 8, "{first_tick:?}");
    }
}

/// Decision 1, at placement: a closePosition order (and a trailing stop) needs an open position
/// with the order on its CLOSING side — refused when flat and refused on the opening side, both
/// ways round; the closing side places.
#[test]
fn c2_close_position_and_trailing_need_an_open_position_on_the_closing_side() {
    let mut ctx = fresh();
    // Flat: every side is refused.
    rejected(&mut ctx, ALICE, &cp_stop_call(SELL, 9_500), E_CP_BIND);
    rejected(&mut ctx, ALICE, &cp_stop_call(BUY, 10_500), E_CP_BIND);
    rejected(
        &mut ctx,
        ALICE,
        &algo_call(TAKE_PROFIT_MARKET, SELL, 10_500, 0, 0, 0, CP),
        E_CP_BIND,
    );
    for (side, a) in [(SELL, 0), (SELL, 10_500), (BUY, 0), (BUY, 9_500)] {
        rejected(&mut ctx, ALICE, &trail_call(side, a, 100, 1, RO), E_TRAIL_BIND);
    }
    // A plain reduce-only conditional is NOT position-bound (Binance: no check before trigger).
    place_algo(&mut ctx, ALICE, &ro_stop_call(SELL, 9_500, 1));

    seed_pos(&mut ctx, ALICE, 10, 1);
    seed_pos(&mut ctx, CAROL, -10, 1);
    for (user, close, open, stop_close, stop_open) in
        [(ALICE, SELL, BUY, 9_500, 10_500), (CAROL, BUY, SELL, 10_500, 9_500)]
    {
        rejected(&mut ctx, user, &cp_stop_call(open, stop_open), E_CP_BIND);
        rejected(&mut ctx, user, &trail_call(open, 0, 100, 1, RO), E_TRAIL_BIND);
        // A pending one on the opening side (its activation price on the valid side of the mark).
        rejected(&mut ctx, user, &trail_call(open, stop_close, 100, 1, RO), E_TRAIL_BIND);
        place_algo(&mut ctx, user, &cp_stop_call(close, stop_close));
        trail(&mut ctx, user, close, 0, 100, 1);
        trail(&mut ctx, user, close, stop_open, 100, 1);
    }
}

/// Decision 1, the lazy expiry — the case it exists for. ALICE's long is closed and a NEW long of
/// the same size is opened the same way round. The stale orders are spent with reason 11
/// (`PositionClosed`) when they would act — the pending trailer when its activation price is
/// reached (no `AlgoOrderTriggered`: it never triggered), the closePosition stop and the activated
/// trailer when they fire (`AlgoOrderTriggered`, then 11) — and the new long is NOT closed, with
/// liquidity sitting right there. Closing the position touched none of them (they still listed).
///
/// With `fresh_stop`, a closePosition stop placed AFTER the reopening (on the new position) fires
/// on the same tick and closes exactly the new 10 — the one Trade of the tick.
#[test]
fn c2_a_reopened_position_is_never_closed_by_orders_bound_to_the_old_one() {
    for fresh_stop in [false, true] {
        let mut ctx = fresh();
        seed_pos(&mut ctx, ALICE, 10, 1);
        seed_pos(&mut ctx, ERIN, -10, 1);
        let cp_old = place_algo(&mut ctx, ALICE, &cp_stop_call(SELL, 9_500)).0;
        let tr_act = trail(&mut ctx, ALICE, SELL, 0, 100, 10);
        let tr_pend = trail(&mut ctx, ALICE, SELL, 10_200, 100, 10);

        // Close the long completely, as a taker.
        rest(&mut ctx, BOB, BUY, MARK, 10);
        let c = take(&mut ctx, ALICE, SELL, 10);
        assert_eq!(trades(&c.logs), vec![(ALICE, BOB, MARK, 10)]);
        assert_eq!(amount(&mut ctx, ALICE), 0);
        assert_eq!(algo_event_count(&c.logs), 0, "closing a position touches no order");
        assert_eq!(open_algos(&mut ctx, ALICE), vec![cp_old, tr_act, tr_pend]);

        // A NEW long, same size, same direction.
        rest(&mut ctx, CAROL, SELL, MARK, 10);
        take(&mut ctx, ALICE, BUY, 10);
        assert_eq!(amount(&mut ctx, ALICE), 10);
        let cp_new = fresh_stop.then(|| place_algo(&mut ctx, ALICE, &cp_stop_call(SELL, 9_500)).0);

        let c = tick(&mut ctx, 10_200);
        assert_eq!(algo_cancels(&c.logs), vec![(tr_pend, R_POSITION_CLOSED)]);
        assert!(activations(&c.logs).is_empty() && triggered(&c.logs).is_empty());
        algo_gone(&mut ctx, tr_pend);
        // Not yet due: the activated one trails on (peak 10_200, stop ⌊10_200·0.99⌋ = 10_098).
        assert_eq!(trail_state(&mut ctx, tr_act), (true, 10_200, 10_098));

        // Liquidity a wrong close would hit.
        rest(&mut ctx, DAVE, BUY, 9_500, 20);
        let c = tick(&mut ctx, 9_500);
        let mut want = vec![(cp_old, 9_500, 9_500)];
        want.extend(cp_new.map(|id| (id, 9_500, 9_500)));
        want.push((tr_act, 10_098, 9_500));
        assert_eq!(triggered(&c.logs), want, "fresh_stop={fresh_stop}");
        assert_eq!(
            algo_cancels(&c.logs),
            vec![(cp_old, R_POSITION_CLOSED), (tr_act, R_POSITION_CLOSED)],
            "fresh_stop={fresh_stop}"
        );
        match cp_new {
            None => {
                assert!(placed(&c.logs).is_empty() && trades(&c.logs).is_empty());
                assert_eq!(amount(&mut ctx, ALICE), 10, "the new long is untouched");
            }
            Some(id) => {
                assert_eq!(placed_qty(&c.logs, id), vec![10]);
                assert_eq!(placed(&c.logs).len(), 1, "nothing placed for the stale orders");
                assert_eq!(trades(&c.logs), vec![(ALICE, DAVE, 9_500, 10)]);
                assert_eq!(amount(&mut ctx, ALICE), 0);
            }
        }
        assert!(open_algos(&mut ctx, ALICE).is_empty());
    }
}

/// A FLIP ends the position too: long 10 → short 5 leaves the closePosition SELL on today's
/// opening side, and it is reported as 11, not 6 (`ReduceOnlyWouldOpen`). Flipping back (→ long 3)
/// puts the side back on the closing side, but the order belongs to a position that ended twice
/// over: still 11, and the long 3 is untouched.
#[test]
fn c2_a_flip_expires_the_bound_orders_as_position_closed() {
    for flip_back in [false, true] {
        let mut ctx = fresh();
        seed_pos(&mut ctx, ALICE, 10, 1);
        seed_pos(&mut ctx, ERIN, -10, 1);
        let cp = place_algo(&mut ctx, ALICE, &cp_stop_call(SELL, 9_500)).0;
        let tr = trail(&mut ctx, ALICE, SELL, 0, 100, 10);
        rest(&mut ctx, BOB, BUY, MARK, 15);
        take(&mut ctx, ALICE, SELL, 15);
        let mut left = -5;
        if flip_back {
            rest(&mut ctx, CAROL, SELL, MARK, 8);
            take(&mut ctx, ALICE, BUY, 8);
            left = 3;
        }
        assert_eq!(amount(&mut ctx, ALICE), left);
        rest(&mut ctx, DAVE, BUY, 9_500, 20);
        let c = tick(&mut ctx, 9_500);
        assert_eq!(triggered(&c.logs), vec![(cp, 9_500, 9_500), (tr, 9_900, 9_500)]);
        assert_eq!(
            algo_cancels(&c.logs),
            vec![(cp, R_POSITION_CLOSED), (tr, R_POSITION_CLOSED)],
            "flip_back={flip_back}"
        );
        assert!(placed(&c.logs).is_empty() && trades(&c.logs).is_empty());
        assert_eq!(amount(&mut ctx, ALICE), left);
    }
}

/// Only flat or flipped ends a position. Shrinking it (10 → 6) or growing it (10 → 15) keeps the
/// binding, and closePosition takes the whole |position| AT TRIGGER TIME.
#[test]
fn c2_a_resized_position_keeps_its_close_position_order() {
    for (adjust, qty, left) in [(SELL, 4u64, 6i64), (BUY, 5, 15)] {
        let mut ctx = fresh();
        seed_pos(&mut ctx, ALICE, 10, 1);
        seed_pos(&mut ctx, ERIN, -10, 1);
        let cp = place_algo(&mut ctx, ALICE, &cp_stop_call(SELL, 9_500)).0;
        let maker = if adjust == SELL { BOB } else { CAROL };
        rest(&mut ctx, maker, 1 - adjust, MARK, qty);
        take(&mut ctx, ALICE, adjust, qty);
        assert_eq!(amount(&mut ctx, ALICE), left);
        rest(&mut ctx, DAVE, BUY, 9_500, 20);
        let c = tick(&mut ctx, 9_500);
        assert_eq!(triggered(&c.logs), vec![(cp, 9_500, 9_500)]);
        assert_eq!(placed_qty(&c.logs, cp), vec![left as u64]);
        assert_eq!(trades(&c.logs), vec![(ALICE, DAVE, 9_500, left as u64)]);
        assert!(algo_cancels(&c.logs).is_empty());
        assert_eq!(amount(&mut ctx, ALICE), 0);
    }
}

/// The counter sees EVERY amount transition, not only the taker path: ALICE goes flat as a MAKER
/// (her resting sell is hit) and reopens as a maker (her resting buy is hit). The old order is
/// still spent with 11 and the reopened long is untouched.
#[test]
fn c2_the_expiry_also_follows_a_position_closed_and_reopened_as_a_maker() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10, 1);
    let cp = place_algo(&mut ctx, ALICE, &cp_stop_call(SELL, 9_500)).0;
    rest(&mut ctx, ALICE, SELL, MARK, 10);
    let c = take(&mut ctx, ERIN, BUY, 10);
    assert_eq!(trades(&c.logs), vec![(ERIN, ALICE, MARK, 10)]);
    assert_eq!(amount(&mut ctx, ALICE), 0);
    rest(&mut ctx, ALICE, BUY, MARK, 10);
    take(&mut ctx, ERIN, SELL, 10);
    assert_eq!(amount(&mut ctx, ALICE), 10);
    rest(&mut ctx, DAVE, BUY, 9_500, 20);
    let c = tick(&mut ctx, 9_500);
    assert_eq!(triggered(&c.logs), vec![(cp, 9_500, 9_500)]);
    assert_eq!(algo_cancels(&c.logs), vec![(cp, R_POSITION_CLOSED)]);
    assert!(trades(&c.logs).is_empty());
    assert_eq!(amount(&mut ctx, ALICE), 10);
}

/// The appended `getAlgoOrder` fields are zero for every non-trailing conditional, and the type is
/// derived correctly for each (a trailing SELL must not read as a TAKE_PROFIT_MARKET, a trailing
/// BUY not as a STOP_MARKET).
#[test]
fn c2_get_algo_order_reports_types_and_zero_trailing_fields_for_fixed_conditionals() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10, 1);
    seed_pos(&mut ctx, CAROL, -10, 1);
    let cases = [
        (ALICE, algo_call(STOP, SELL, 9_500, 9_400, 2, GTC, RO), STOP),
        (ALICE, ro_stop_call(SELL, 9_500, 2), STOP_MARKET),
        (ALICE, algo_call(TAKE_PROFIT, SELL, 10_500, 10_600, 2, GTC, RO), TAKE_PROFIT),
        (ALICE, algo_call(TAKE_PROFIT_MARKET, SELL, 10_500, 0, 0, 0, CP), TAKE_PROFIT_MARKET),
        (CAROL, cp_stop_call(BUY, 10_500), STOP_MARKET),
        (CAROL, algo_call(TAKE_PROFIT_MARKET, BUY, 9_500, 0, 2, 0, RO), TAKE_PROFIT_MARKET),
    ];
    for (user, input, kind) in cases {
        let (id, _) = place_algo(&mut ctx, user, &input);
        let r = algo(&mut ctx, id);
        assert_eq!(r.orderType, kind);
        assert_eq!((r.callbackRateBps, r.activated, r.peakPrice, r.currentStop), (0, false, 0, 0));
    }
    for (user, side) in [(ALICE, SELL), (CAROL, BUY)] {
        let id = trail(&mut ctx, user, side, 0, 100, 1);
        assert_eq!(algo(&mut ctx, id).orderType, TRAILING, "side {side}");
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Commit 3 — trailing stops: placement (decisions 4, 5; memo §2.7)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// ALICE long 10 (SELL trailers), CAROL short 10 (BUY trailers), mark 10_000, no tick yet.
fn trail_fixture() -> TestCtx {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10, 1);
    seed_pos(&mut ctx, CAROL, -10, 1);
    ctx
}

/// Decision 4: the callback grid is 10..=1000 bps in steps of 10 (Binance 0.1 %–10 % in 0.1 %
/// steps), for an immediate and a pending order alike; on-grid values echo as both `price` and
/// `callbackRateBps`.
#[test]
fn t_the_callback_must_be_on_the_10_to_1000_bps_grid() {
    let mut ctx = trail_fixture();
    for (user, side, pending) in [(ALICE, SELL, 10_500), (CAROL, BUY, 9_500)] {
        for cb in [0u64, 1, 5, 9, 11, 15, 25, 995, 999, 1_001, 1_005, 1_010, 1_100, 10_000, u64::MAX]
        {
            rejected(&mut ctx, user, &trail_call(side, 0, cb, 1, RO), E_CALLBACK);
            rejected(&mut ctx, user, &trail_call(side, pending, cb, 1, RO), E_CALLBACK);
        }
        for cb in [10u64, 20, 330, 990, 1_000] {
            let (id, c) = place_algo(&mut ctx, user, &trail_call(side, 0, cb, 1, RO));
            assert_eq!(evs::<AlgoOrderPlaced>(&c.logs)[0].price, cb);
            let r = algo(&mut ctx, id);
            assert_eq!((r.price, r.callbackRateBps as u64), (cb, cb));
        }
    }
}

/// Binance `-2021` for an activatePrice on the wrong side: a SELL must activate ABOVE the stored
/// mark and a BUY BELOW it; reaching it already (equality included) is refused, 0 means "now".
/// Re-checked against the new stored mark after a tick.
#[test]
fn t_an_activation_price_already_reached_is_refused() {
    let mut ctx = trail_fixture();
    for a in [MARK, MARK - 1, 1] {
        rejected(&mut ctx, ALICE, &trail_call(SELL, a, 100, 1, RO), E_IMMEDIATE);
    }
    for a in [MARK, MARK + 1, 1_000_000] {
        rejected(&mut ctx, CAROL, &trail_call(BUY, a, 100, 1, RO), E_IMMEDIATE);
    }
    let (s, c) = place_algo(&mut ctx, ALICE, &trail_call(SELL, MARK + 1, 100, 1, RO));
    assert_eq!(evs::<AlgoOrderPlaced>(&c.logs)[0].triggerPrice, MARK + 1);
    assert!(activations(&c.logs).is_empty());
    assert_eq!(trail_state(&mut ctx, s), (false, 0, 0));
    let (b, _) = place_algo(&mut ctx, CAROL, &trail_call(BUY, MARK - 1, 100, 1, RO));
    assert_eq!(trail_state(&mut ctx, b), (false, 0, 0));

    // Tick 9_999: the SELL's 10_001 is not reached; the BUY's 9_999 is, inclusively (stop
    // ⌈9_999·1.01⌉ = ⌈10_098.99⌉ = 10_099). Then the refusals follow the NEW stored mark.
    let c = tick(&mut ctx, 9_999);
    assert_eq!(activations(&c.logs), vec![(b, 9_999, 10_099)]);
    for a in [9_999, 9_998] {
        rejected(&mut ctx, ALICE, &trail_call(SELL, a, 100, 1, RO), E_IMMEDIATE);
    }
    for a in [9_999, 10_000] {
        rejected(&mut ctx, CAROL, &trail_call(BUY, a, 100, 1, RO), E_IMMEDIATE);
    }
    place_algo(&mut ctx, ALICE, &trail_call(SELL, 10_000, 100, 1, RO));
    place_algo(&mut ctx, CAROL, &trail_call(BUY, 9_998, 100, 1, RO));
}

/// Decision 5: reduceOnly REQUIRED, closePosition REFUSED; the STP bits are honoured and inherited
/// state is reported; unknown bits and the reserved STP mode keep their own rejects.
#[test]
fn t_reduce_only_is_required_and_close_position_refused() {
    let mut ctx = trail_fixture();
    for flags in [0, STP_TAKER, STP_BOTH] {
        rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, flags), E_TRAIL_RO);
        rejected(&mut ctx, ALICE, &trail_call(SELL, 10_500, 100, 1, flags), E_TRAIL_RO);
    }
    for flags in [CP, CP | STP_TAKER] {
        rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 0, flags), E_TRAIL_CP);
        rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, flags), E_TRAIL_CP);
    }
    rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, RO | CP), E_RO_CP);
    rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, RO | 0x10), E_UNKNOWN_BITS);
    rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, RO | 0x80), E_UNKNOWN_BITS);
    rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, RO | 0b1100), E_STP_MODE);
    for (flags, mode) in [(RO, 0u8), (RO | STP_TAKER, 1), (RO | STP_BOTH, 2)] {
        let (id, c) = place_algo(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, flags));
        assert_eq!(evs::<AlgoOrderPlaced>(&c.logs)[0].flags, flags, "echoed as sent");
        let r = algo(&mut ctx, id);
        assert!(r.reduceOnly && !r.closePosition);
        assert_eq!(r.selfTradePreventionMode, mode);
    }
}

/// `quantity` > 0, a multiple of stepSize, ≤ maxQuantity, and NOT held to minQuantity (the
/// reduce-only exemption); it may exceed the position (truncated at fire). `tif` and `workingType`
/// must be 0; algoType must be CONDITIONAL; orderType 5 does not exist; a non-zero activation
/// price must be on the tick grid (the callback is NOT a price and is not).
#[test]
fn t_quantity_tif_working_type_and_grid_rules() {
    let mut ctx = trail_fixture();
    rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 0, RO), E_QTY_ZERO);
    rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1_000_001, RO), E_QTY_MAX);
    for tif in [1u8, 2, 3] {
        rejected(&mut ctx, ALICE, &algo_call(TRAILING, SELL, 0, 100, 1, tif, RO), E_TIF);
    }
    rejected(&mut ctx, ALICE, &algo_raw(0, TRAILING, SELL, 0, 100, 1, 0, 1, RO), E_WORKING);
    rejected(&mut ctx, ALICE, &algo_raw(1, TRAILING, SELL, 0, 100, 1, 0, 0, RO), E_ALGO_TYPE);
    rejected(&mut ctx, ALICE, &algo_call(5, SELL, 0, 100, 1, 0, RO), E_KIND);
    let id = trail(&mut ctx, ALICE, SELL, 0, 100, 1_000_000);
    assert_eq!(algo(&mut ctx, id).quantity, 1_000_000, "a request, not bounded by |position|");

    // stepSize 5, minQuantity 5.
    let mut ctx = market(1, 5, 5, MARK);
    seed_pos(&mut ctx, ALICE, 10, 1);
    rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 7, RO), E_QTY_STEP);
    trail(&mut ctx, ALICE, SELL, 0, 100, 5);
    // stepSize 1, minQuantity 5: one contract is fine.
    let mut ctx = market(1, 1, 5, MARK);
    seed_pos(&mut ctx, ALICE, 10, 1);
    let id = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    assert_eq!(algo(&mut ctx, id).quantity, 1);

    // tickSize 30, mark 9_990 = 333 ticks.
    let mut ctx = market(30, 1, 1, 9_990);
    seed_pos(&mut ctx, ALICE, 10, 1);
    rejected(&mut ctx, ALICE, &trail_call(SELL, 10_005, 100, 1, RO), E_TICK);
    trail(&mut ctx, ALICE, SELL, 10_020, 100, 1);
    // cb 20 is not a multiple of the 30 tick and must not be treated as a price.
    trail(&mut ctx, ALICE, SELL, 0, 20, 1);
}

/// Trailing stops count toward the 32 conditionals per (user, market) and the 200 open orders
/// shared with the book — refused at the cap with the cap's own reason, in either phase, and the
/// slot comes back with a cancel.
#[test]
fn t_trailing_stops_count_toward_both_caps() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 100, 1);
    let mut ids = Vec::new();
    for k in 0..32u64 {
        let a = if k % 2 == 0 { 0 } else { 10_100 + k };
        ids.push(trail(&mut ctx, ALICE, SELL, a, 10 + 10 * (k % 10), 1));
    }
    rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, RO), E_ALGO_MAX);
    rejected(&mut ctx, ALICE, &trail_call(SELL, 10_500, 100, 1, RO), E_ALGO_MAX);
    rejected(&mut ctx, ALICE, &ro_stop_call(SELL, 9_000, 1), E_ALGO_MAX);
    for k in [0usize, 1] {
        cancel_algo(&mut ctx, ALICE, ids[k]);
        trail(&mut ctx, ALICE, SELL, 0, 100, 1);
        rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, RO), E_ALGO_MAX);
    }

    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 100, 1);
    for _ in 0..170 {
        rest(&mut ctx, ALICE, BUY, 9_000, 1);
    }
    let mut ids = Vec::new();
    for k in 0..30u64 {
        ids.push(trail(&mut ctx, ALICE, SELL, if k < 15 { 0 } else { 10_500 }, 100, 1));
    }
    rejected(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, RO), E_ALGO_CAP);
    rejected(&mut ctx, ALICE, &trail_call(SELL, 10_500, 100, 1, RO), E_ALGO_CAP);
    rejected(&mut ctx, ALICE, &ro_stop_call(SELL, 9_000, 1), E_ALGO_CAP);
    rejected(&mut ctx, ALICE, &order_call(BUY, 9_000, 1, LIMIT, GTC, 0), E_PLACE_CAP);
    cancel_algo(&mut ctx, ALICE, ids[20]);
    rest(&mut ctx, ALICE, BUY, 9_000, 1);
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Activation, events and the view (memo §2.7 events, §2.8 edges)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// Activation price 0 = activate NOW at the stored mark: the effective activation price is that
/// mark (event and view agree), `AlgoOrderActivated` FOLLOWS `AlgoOrderPlaced` in the same
/// transaction, and the view reports the whole record. SELL b = 100: stop ⌊10_000·0.99⌋ = 9_900;
/// BUY b = 250: ⌈10_000·1.025⌉ = 10_250. After a tick the "now" is the tick's mark (10_300:
/// stop ⌊10_197⌋).
#[test]
fn t_activation_now_events_and_view() {
    for (user, side, pos, cb, stop) in
        [(ALICE, SELL, 10i64, 100u64, 9_900u64), (CAROL, BUY, -10, 250, 10_250)]
    {
        let mut ctx = fresh();
        seed_pos(&mut ctx, user, pos, 1);
        let mut client = [0u8; 16];
        client[0] = 0xc1;
        let input = placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side,
            orderType: TRAILING,
            triggerPrice: 0,
            price: cb,
            quantity: 5,
            tif: 0,
            workingType: 0,
            clientAlgoId: client.into(),
            flags: RO | STP_TAKER,
        }
        .abi_encode();
        let (id, c) = place_algo(&mut ctx, user, &input);
        assert_eq!(c.logs.len(), 2, "exactly the two conditional events");
        let p = AlgoOrderPlaced::decode_raw_log(c.logs[0].data.topics(), &c.logs[0].data.data).unwrap();
        assert_eq!(
            (p.user, p.marketId, p.algoId.0, p.side, p.orderType, p.triggerPrice, p.price),
            (user, MARKET_ID, id, side, TRAILING, MARK, cb)
        );
        assert_eq!((p.quantity, p.tif, p.clientAlgoId.0, p.flags), (5, 0, client, RO | STP_TAKER));
        let a = AlgoOrderActivated::decode_raw_log(c.logs[1].data.topics(), &c.logs[1].data.data).unwrap();
        assert_eq!(
            (a.user, a.marketId, a.algoId.0, a.markPrice, a.stopPrice),
            (user, MARKET_ID, id, MARK, stop)
        );
        assert_eq!(c.logs[1].address, PERP_DEX_ADDRESS);

        let r = algo(&mut ctx, id);
        assert_eq!((r.owner, r.marketId, r.side, r.orderType), (user, MARKET_ID, side, TRAILING));
        assert_eq!((r.triggerPrice, r.price, r.quantity, r.tif, r.status), (MARK, cb, 5, 0, 0));
        assert_eq!((r.reduceOnly, r.closePosition, r.selfTradePreventionMode), (true, false, 1));
        assert_eq!(
            (r.callbackRateBps as u64, r.activated, r.peakPrice, r.currentStop),
            (cb, true, MARK, stop)
        );
        assert_eq!(open_algos(&mut ctx, user), vec![id]);
    }
    let mut ctx = trail_fixture();
    tick(&mut ctx, 10_300);
    let (id, c) = place_algo(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 1, RO));
    assert_eq!(evs::<AlgoOrderPlaced>(&c.logs)[0].triggerPrice, 10_300);
    assert_eq!(activations(&c.logs), vec![(id, 10_300, 10_197)]);
    assert_eq!(algo(&mut ctx, id).triggerPrice, 10_300);
}

/// Activation is "reaches", inclusive, and seeds the peak at the CROSSING mark: one tick short
/// does nothing; touching it activates at it; a gap through it activates at the gapped mark (not
/// at the activation price). It never fires on the tick it activates.
#[test]
fn t_activation_on_touch_and_through_a_gap() {
    // (user, side, activation, the tick one short, a gap tick, stop at A, stop at the gap)
    // SELL A 10_200: ⌊10_200·0.99⌋ = 10_098, gap 10_800 → ⌊10_692⌋.
    // BUY  A  9_800: ⌈9_800·1.01⌉ = 9_898,   gap  9_000 → ⌈9_090⌉.
    for (user, side, a, short, gap, stop_a, stop_gap) in [
        (ALICE, SELL, 10_200u64, 10_199u64, 10_800u64, 10_098u64, 10_692u64),
        (CAROL, BUY, 9_800, 9_801, 9_000, 9_898, 9_090),
    ] {
        let mut ctx = trail_fixture();
        let id = trail(&mut ctx, user, side, a, 100, 1);
        let c = tick(&mut ctx, short);
        assert_eq!(algo_event_count(&c.logs), 0);
        assert_eq!(trail_state(&mut ctx, id), (false, 0, 0));
        let c = tick(&mut ctx, a);
        assert_eq!(activations(&c.logs), vec![(id, a, stop_a)]);
        assert!(triggered(&c.logs).is_empty());
        assert_eq!(trail_state(&mut ctx, id), (true, a, stop_a));
        assert_eq!(algo(&mut ctx, id).triggerPrice, a, "triggerPrice stays the activation price");

        let mut ctx = trail_fixture();
        let id = trail(&mut ctx, user, side, a, 100, 1);
        let c = tick(&mut ctx, gap);
        assert_eq!(activations(&c.logs), vec![(id, gap, stop_gap)]);
        assert!(triggered(&c.logs).is_empty());
        assert_eq!(trail_state(&mut ctx, id), (true, gap, stop_gap));
    }
}

/// The peak is the best mark SINCE ACTIVATION (Binance's "since placement": every earlier mark was
/// short of the activation price), then the stop follows it. SELL A 10_200, b 100:
/// 10_100, 10_150 pending → 10_200 activates (10_098) → 10_180 keeps 10_200 → 10_250 (⌊10_147.5⌋ =
/// 10_147) → 10_148 does not fire → 10_147 fires, reported at 10_147.
#[test]
fn t_a_pending_order_trails_from_its_activation() {
    let mut ctx = trail_fixture();
    let id = trail(&mut ctx, ALICE, SELL, 10_200, 100, 1);
    for (m, state) in [
        (10_100, (false, 0, 0)),
        (10_150, (false, 0, 0)),
        (10_200, (true, 10_200, 10_098)),
        (10_180, (true, 10_200, 10_098)),
        (10_250, (true, 10_250, 10_147)),
        (10_148, (true, 10_250, 10_147)),
    ] {
        let c = tick(&mut ctx, m);
        assert!(triggered(&c.logs).is_empty(), "@{m}");
        assert_eq!(trail_state(&mut ctx, id), state, "@{m}");
    }
    let c = tick(&mut ctx, 10_147);
    assert_eq!(triggered(&c.logs), vec![(id, 10_147, 10_147)]);
    algo_gone(&mut ctx, id);
    assert!(open_algos(&mut ctx, ALICE).is_empty());
}

/// An order placed before the first oracle tick trails from the stored (initial) mark at seq 0: a
/// first tick below it is not a new high (peak stays 10_000), one above it is.
#[test]
fn t_an_order_placed_before_the_first_tick_seeds_at_the_initial_mark() {
    let mut ctx = trail_fixture();
    let a = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    let p = trail(&mut ctx, ALICE, SELL, 10_050, 100, 1);
    assert_eq!(current_ts(&mut ctx), 0);
    let c = tick(&mut ctx, 9_950);
    assert_eq!(algo_event_count(&c.logs), 0);
    assert_eq!(trail_state(&mut ctx, a), (true, 10_000, 9_900));
    let c = tick(&mut ctx, 9_900);
    assert_eq!(triggered(&c.logs), vec![(a, 9_900, 9_900)]);
    let c = tick(&mut ctx, 10_050);
    assert_eq!(activations(&c.logs), vec![(p, 10_050, 9_949)]);

    let mut ctx = trail_fixture();
    let a = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    tick(&mut ctx, 10_100);
    assert_eq!(trail_state(&mut ctx, a), (true, 10_100, 9_999));
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// The exact fire predicate on hand-computed mark paths (memo §2.4 graft 5, §2.7)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// SELL, four callbacks activated together at 10_000, ALICE long 100. Thresholds are the u128
/// cross-multiplication `10000·m ≤ H·(10000−b)`; at H = 10_123 they are NOT integers, so the
/// boundary is one tick wide:
///
/// ```text
/// b=10    H·9990 = 101_128_770 → thr 10_112      b=70    H·9930 = 100_521_390 → thr 10_052
/// b=250   H·9750 =  98_699_250 → thr  9_869      b=1000  H·9000 =  91_107_000 → thr  9_110
/// then H = 10_200:  b=250 → 99_450_000 → thr 9_945 EXACTLY (equality fires),  b=1000 → 9_180
/// ```
#[test]
fn t_sell_trailers_fire_exactly_at_their_thresholds() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 100, 1);
    let ids: Vec<(u64, Id)> =
        [10u64, 70, 250, 1_000].map(|cb| (cb, trail(&mut ctx, ALICE, SELL, 0, cb, 1))).to_vec();
    let id = |cb: u64| ids.iter().find(|x| x.0 == cb).unwrap().1;
    let views = |ctx: &mut TestCtx| -> Vec<(bool, u64, u64)> {
        ids.iter().map(|x| trail_state(ctx, x.1)).collect()
    };
    assert_eq!(
        views(&mut ctx),
        vec![(true, 10_000, 9_990), (true, 10_000, 9_930), (true, 10_000, 9_750), (true, 10_000, 9_000)]
    );

    let c = tick(&mut ctx, 10_123);
    assert_eq!(algo_event_count(&c.logs), 0, "no per-tick event when the peak moves");
    assert_eq!(
        views(&mut ctx),
        vec![
            (true, 10_123, 10_112),
            (true, 10_123, 10_052),
            (true, 10_123, 9_869),
            (true, 10_123, 9_110)
        ]
    );

    assert!(triggered(&tick(&mut ctx, 10_113).logs).is_empty());
    assert_eq!(triggered(&tick(&mut ctx, 10_112).logs), vec![(id(10), 10_112, 10_112)]);
    assert!(triggered(&tick(&mut ctx, 10_053).logs).is_empty());
    assert_eq!(triggered(&tick(&mut ctx, 10_052).logs), vec![(id(70), 10_052, 10_052)]);
    let c = tick(&mut ctx, 10_200);
    assert!(triggered(&c.logs).is_empty());
    assert_eq!(trail_state(&mut ctx, id(250)), (true, 10_200, 9_945));
    assert_eq!(trail_state(&mut ctx, id(1_000)), (true, 10_200, 9_180));
    assert!(triggered(&tick(&mut ctx, 9_946).logs).is_empty());
    assert_eq!(triggered(&tick(&mut ctx, 9_945).logs), vec![(id(250), 9_945, 9_945)]);
    assert!(triggered(&tick(&mut ctx, 9_181).logs).is_empty());
    // A gap through the stop fires at the gapped mark; the event shows both.
    assert_eq!(triggered(&tick(&mut ctx, 9_100).logs), vec![(id(1_000), 9_180, 9_100)]);
    assert!(open_algos(&mut ctx, ALICE).is_empty());
    assert_eq!(amount(&mut ctx, ALICE), 100, "no liquidity: every fired order expired unfilled");
}

/// The BUY mirror, `10000·m ≥ L·(10000+b)`, CAROL short 100, activated at 10_000:
///
/// ```text
/// L = 9_877:  b=10 → 98_868_770 → ⌈⌉ 9_887    b=130 → 100_054_010 → 10_006
///             b=500 → 103_708_500 → 10_371    b=1000 → 108_647_000 → 10_865
/// L = 9_800:  b=500 → 102_900_000 → 10_290 EXACTLY,  b=1000 → 10_780
/// ```
#[test]
fn t_buy_trailers_fire_exactly_at_their_thresholds() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, CAROL, -100, 1);
    let ids: Vec<(u64, Id)> =
        [10u64, 130, 500, 1_000].map(|cb| (cb, trail(&mut ctx, CAROL, BUY, 0, cb, 1))).to_vec();
    let id = |cb: u64| ids.iter().find(|x| x.0 == cb).unwrap().1;
    let views = |ctx: &mut TestCtx| -> Vec<(bool, u64, u64)> {
        ids.iter().map(|x| trail_state(ctx, x.1)).collect()
    };
    assert_eq!(
        views(&mut ctx),
        vec![
            (true, 10_000, 10_010),
            (true, 10_000, 10_130),
            (true, 10_000, 10_500),
            (true, 10_000, 11_000)
        ]
    );
    let c = tick(&mut ctx, 9_877);
    assert_eq!(algo_event_count(&c.logs), 0);
    assert_eq!(
        views(&mut ctx),
        vec![(true, 9_877, 9_887), (true, 9_877, 10_006), (true, 9_877, 10_371), (true, 9_877, 10_865)]
    );
    assert!(triggered(&tick(&mut ctx, 9_886).logs).is_empty());
    assert_eq!(triggered(&tick(&mut ctx, 9_887).logs), vec![(id(10), 9_887, 9_887)]);
    assert!(triggered(&tick(&mut ctx, 10_005).logs).is_empty());
    assert_eq!(triggered(&tick(&mut ctx, 10_006).logs), vec![(id(130), 10_006, 10_006)]);
    tick(&mut ctx, 9_800);
    assert_eq!(trail_state(&mut ctx, id(500)), (true, 9_800, 10_290));
    assert_eq!(trail_state(&mut ctx, id(1_000)), (true, 9_800, 10_780));
    assert!(triggered(&tick(&mut ctx, 10_289).logs).is_empty());
    assert_eq!(triggered(&tick(&mut ctx, 10_290).logs), vec![(id(500), 10_290, 10_290)]);
    assert_eq!(triggered(&tick(&mut ctx, 11_000).logs), vec![(id(1_000), 10_780, 11_000)]);
    assert_eq!(amount(&mut ctx, CAROL), -100);
}

/// Orders activated at DIFFERENT ticks share the market's mark history, each from its own start
/// (SELL, b = 100, so stop = ⌊0.99·peak⌋):
///
/// ```text
/// tick   mark    placed   X        Y        Z        W
///  1    10_000    X     10_000
///  2    10_300          10_300
///  3    10_250    Y     10_300   10_250
///  4    10_280          10_300   10_280
///  5    10_260    Z     10_300   10_280   10_260
///  6    10_290          10_300   10_290   10_290
///  7    10_400          10_400   10_400   10_400
///  8    10_350    W     10_400   10_400   10_400   10_350
///  9    10_296   fires X, Y, Z (stop 10_296, FIFO);   W stop ⌊10_246.5⌋ = 10_246 stays
/// 11    10_246   fires W
/// ```
#[test]
fn t_peaks_since_activation_with_staggered_activations() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 100, 1);
    tick(&mut ctx, 10_000);
    let x = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    tick(&mut ctx, 10_300);
    tick(&mut ctx, 10_250);
    let y = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    assert_eq!(trail_state(&mut ctx, x), (true, 10_300, 10_197));
    assert_eq!(trail_state(&mut ctx, y), (true, 10_250, 10_147));
    tick(&mut ctx, 10_280);
    assert_eq!(trail_state(&mut ctx, x), (true, 10_300, 10_197));
    assert_eq!(trail_state(&mut ctx, y), (true, 10_280, 10_177));
    tick(&mut ctx, 10_260);
    let z = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    assert_eq!(trail_state(&mut ctx, z), (true, 10_260, 10_157));
    tick(&mut ctx, 10_290);
    assert_eq!(trail_state(&mut ctx, x), (true, 10_300, 10_197));
    assert_eq!(trail_state(&mut ctx, y), (true, 10_290, 10_187));
    assert_eq!(trail_state(&mut ctx, z), (true, 10_290, 10_187));
    tick(&mut ctx, 10_400);
    for id in [x, y, z] {
        assert_eq!(trail_state(&mut ctx, id), (true, 10_400, 10_296));
    }
    tick(&mut ctx, 10_350);
    let w = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    assert_eq!(trail_state(&mut ctx, w), (true, 10_350, 10_246));
    let c = tick(&mut ctx, 10_296);
    assert_eq!(
        triggered(&c.logs),
        vec![(x, 10_296, 10_296), (y, 10_296, 10_296), (z, 10_296, 10_296)]
    );
    assert_eq!(trail_state(&mut ctx, w), (true, 10_350, 10_246));
    assert!(triggered(&tick(&mut ctx, 10_247).logs).is_empty());
    assert_eq!(triggered(&tick(&mut ctx, 10_246).logs), vec![(w, 10_246, 10_246)]);
}

/// The reported stop is the threshold rounded onto the tick grid AWAY from the mark, and since a
/// mark is on the grid the order fires exactly when the mark reaches the reported stop. Tick 10:
/// SELL b 70 at H 10_130 → thr ⌊10_059.09⌋ = 10_059 → stop 10_050 (10_060 does not fire, 10_050
/// does); BUY b 130 at L 9_870 → thr ⌈9_998.31⌉ = 9_999 → stop 10_000 (9_990 no, 10_000 yes).
/// A low-priced market (mark 1_000, tick 10) moves in whole ticks: b 10 → SELL stop 990, BUY 1_010.
#[test]
fn t_the_reported_stop_is_tick_rounded_away_from_the_mark() {
    let mut ctx = market(10, 1, 1, MARK);
    seed_pos(&mut ctx, ALICE, 10, 1);
    let (s, c) = place_algo(&mut ctx, ALICE, &trail_call(SELL, 0, 70, 1, RO));
    assert_eq!(activations(&c.logs), vec![(s, 10_000, 9_930)]);
    tick(&mut ctx, 10_130);
    assert_eq!(trail_state(&mut ctx, s), (true, 10_130, 10_050));
    assert!(triggered(&tick(&mut ctx, 10_060).logs).is_empty());
    assert_eq!(triggered(&tick(&mut ctx, 10_050).logs), vec![(s, 10_050, 10_050)]);

    let mut ctx = market(10, 1, 1, MARK);
    seed_pos(&mut ctx, CAROL, -10, 1);
    let (b, c) = place_algo(&mut ctx, CAROL, &trail_call(BUY, 0, 130, 1, RO));
    assert_eq!(activations(&c.logs), vec![(b, 10_000, 10_130)]);
    tick(&mut ctx, 9_870);
    assert_eq!(trail_state(&mut ctx, b), (true, 9_870, 10_000));
    assert!(triggered(&tick(&mut ctx, 9_990).logs).is_empty());
    assert_eq!(triggered(&tick(&mut ctx, 10_000).logs), vec![(b, 10_000, 10_000)]);

    for (user, side, pos, stop, fire) in
        [(ALICE, SELL, 10i64, 990u64, 990u64), (CAROL, BUY, -10, 1_010, 1_010)]
    {
        let mut ctx = market(10, 1, 1, 1_000);
        seed_pos(&mut ctx, user, pos, 1);
        let (id, c) = place_algo(&mut ctx, user, &trail_call(side, 0, 10, 1, RO));
        assert_eq!(activations(&c.logs), vec![(id, 1_000, stop)]);
        assert!(triggered(&tick(&mut ctx, 1_000).logs).is_empty(), "a peak never fires at itself");
        assert_eq!(triggered(&tick(&mut ctx, fire).logs), vec![(id, stop, fire)]);
    }
}

/// Several orders, several buckets, ONE tick: every firing order fires, best threshold first
/// (memo §2.4/§2.5: the directory is sorted by threshold, then activation tick, then callback),
/// FIFO within a bucket, and the one that does not fire keeps trailing.
///
/// SELL (ALICE): tick 10_000 — P1, P1b (b 100), R (b 200); tick 10_200; tick 10_150 — P2 (b 100),
/// Q (b 50), S (b 1000). Stops: Q ⌊10_099.25⌋ = 10_099, P1 = P1b = 10_098, P2 ⌊10_048.5⌋ = 10_048,
/// R 9_996, S 9_135. Tick 9_990 fires Q, P1, P1b, P2, R.
///
/// BUY (CAROL) mirror: tick 10_000 — P1, P1b, R; tick 9_800; tick 9_850 — P2, Q, S. Stops: P1 =
/// P1b = 9_898, Q ⌈9_899.25⌉ = 9_900, P2 ⌈9_948.5⌉ = 9_949, R 9_996, S 10_835. Tick 10_010 fires P1,
/// P1b, Q, P2, R.
#[test]
fn t_many_orders_across_buckets_fire_in_one_tick_best_threshold_first() {
    for (user, side, pos, m1, m2, fire, s_stop, order_stops) in [
        (
            ALICE,
            SELL,
            100i64,
            10_200u64,
            10_150u64,
            9_990u64,
            9_135u64,
            [10_099u64, 10_098, 10_098, 10_048, 9_996],
        ),
        (CAROL, BUY, -100, 9_800, 9_850, 10_010, 10_835, [9_898, 9_898, 9_900, 9_949, 9_996]),
    ] {
        let mut ctx = fresh();
        seed_pos(&mut ctx, user, pos, 1);
        tick(&mut ctx, 10_000);
        let p1 = trail(&mut ctx, user, side, 0, 100, 1);
        let p1b = trail(&mut ctx, user, side, 0, 100, 1);
        let r = trail(&mut ctx, user, side, 0, 200, 1);
        tick(&mut ctx, m1);
        let c = tick(&mut ctx, m2);
        assert!(triggered(&c.logs).is_empty());
        let p2 = trail(&mut ctx, user, side, 0, 100, 1);
        let q = trail(&mut ctx, user, side, 0, 50, 1);
        let s = trail(&mut ctx, user, side, 0, 1_000, 1);
        let order = if side == SELL { [q, p1, p1b, p2, r] } else { [p1, p1b, q, p2, r] };
        let c = tick(&mut ctx, fire);
        let want: Vec<(Id, u64, u64)> =
            order.iter().zip(order_stops).map(|(id, st)| (*id, st, fire)).collect();
        assert_eq!(triggered(&c.logs), want, "side {side}");
        assert_eq!(trail_state(&mut ctx, s), (true, m2, s_stop), "side {side}");
        assert_eq!(open_algos(&mut ctx, user), vec![s]);
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Budget, drain and the not-latched rule (decision 7; memo §2.6, §2.8)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// `n` trailing SELLs (b 100, quantity 1) activated NOW, spread over long accounts 32 at a time
/// (the per-market conditional cap); returns them in placement order.
fn crowd(ctx: &mut TestCtx, n: usize, activation: u64) -> Vec<Id> {
    let mut ids = Vec::new();
    let mut owners = 0u16;
    while ids.len() < n {
        let u = new_user(ctx, owners);
        owners += 1;
        seed_pos(ctx, u, 100, 1);
        for _ in 0..(n - ids.len()).min(32) {
            ids.push(trail(ctx, u, SELL, activation, 100, 1));
        }
    }
    ids
}

/// 60 stops crossing together: the tick fires the first 50 (the shared budget) in activation
/// order and sets the deferred-work flag; ONE drain at the same mark finishes the other 10 and
/// clears it; a second drain is a no-op with no log.
#[test]
fn t_budget_overflow_is_finished_by_the_drain_at_the_same_mark() {
    let mut ctx = fresh();
    let ids = crowd(&mut ctx, 60, 0);
    let c = tick(&mut ctx, 9_900);
    let want: Vec<(Id, u64, u64)> = ids.iter().map(|id| (*id, 9_900, 9_900)).collect();
    assert_eq!(triggered(&c.logs), want[..BUDGET].to_vec());
    assert!(deferred(&mut ctx));
    for id in &ids[BUDGET..] {
        assert_eq!(trail_state(&mut ctx, *id), (true, MARK, 9_900), "deferred, still trailing");
    }
    let (more, c) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(triggered(&c.logs), want[BUDGET..].to_vec(), "same mark, same stop");
    assert!(!deferred(&mut ctx));
    let (more, c) = drain(&mut ctx);
    assert!(!more && c.logs.is_empty());
}

/// Exactly 50: all fire on the tick. The flag is CONSERVATIVE here (the ABI docs: the shared
/// budget ran out with liquidation candidates unexamined), so a keeper's drain finds nothing to
/// fire and reports `false`.
#[test]
fn t_exactly_the_budget_fires_on_the_tick() {
    let mut ctx = fresh();
    let ids = crowd(&mut ctx, BUDGET, 0);
    let c = tick(&mut ctx, 9_900);
    assert_eq!(ids_of(&triggered(&c.logs)), ids);
    let (more, c) = drain(&mut ctx);
    assert!(!more);
    assert!(triggered(&c.logs).is_empty());
    assert!(!deferred(&mut ctx));
}

/// Fixed triggers are served FIRST from the shared budget: 45 fixed stops (trigger 9_950) and 10
/// trailing ones (stop 9_900) crossing on one tick → the 45 fixed (in index FIFO order) then 5
/// trailing; the drain fires the last 5.
#[test]
fn t_fixed_triggers_spend_the_shared_budget_before_trailing_fires() {
    let mut ctx = fresh();
    let mut fixed = Vec::new();
    for (k, n) in [(0u16, 30usize), (1, 15)] {
        let u = new_user(&mut ctx, 100 + k);
        seed_pos(&mut ctx, u, 100, 1);
        for _ in 0..n {
            fixed.push(place_algo(&mut ctx, u, &ro_stop_call(SELL, 9_950, 1)).0);
        }
    }
    let trailing = crowd(&mut ctx, 10, 0);
    let c = tick(&mut ctx, 9_900);
    let mut want: Vec<(Id, u64, u64)> = fixed.iter().map(|id| (*id, 9_950, 9_900)).collect();
    want.extend(trailing[..5].iter().map(|id| (*id, 9_900, 9_900)));
    assert_eq!(triggered(&c.logs), want);
    assert!(deferred(&mut ctx));
    let (more, c) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(
        triggered(&c.logs),
        trailing[5..].iter().map(|id| (*id, 9_900, 9_900)).collect::<Vec<_>>()
    );
}

/// Decision 7, NOT latched: a fire the budget deferred and nobody drained is re-tested at the next
/// mark. The bounce to 9_950 is above the 9_900 stop, so the 10 do not fire and keep trailing
/// from the SAME peak (the peak itself is never deferred); a later high (10_100 → stop 9_999) and
/// a drop to it fire them.
#[test]
fn t_an_undrained_fire_is_retested_at_the_next_mark() {
    let mut ctx = fresh();
    let ids = crowd(&mut ctx, 60, 0);
    tick(&mut ctx, 9_900);
    let c = tick(&mut ctx, 9_950);
    assert!(triggered(&c.logs).is_empty(), "bounced above the stop: no fire");
    assert!(!deferred(&mut ctx));
    for id in &ids[..BUDGET] {
        algo_gone(&mut ctx, *id);
    }
    for id in &ids[BUDGET..] {
        assert_eq!(trail_state(&mut ctx, *id), (true, MARK, 9_900));
    }
    tick(&mut ctx, 10_100);
    for id in &ids[BUDGET..] {
        assert_eq!(trail_state(&mut ctx, *id), (true, 10_100, 9_999));
    }
    let c = tick(&mut ctx, 9_999);
    assert_eq!(
        triggered(&c.logs),
        ids[BUDGET..].iter().map(|id| (*id, 9_999, 9_999)).collect::<Vec<_>>()
    );
}

/// Activations have their OWN cap (64): 70 pending orders reached together → 64 activate on the
/// tick (FIFO at the level), the flag is set, the drain activates the other 6 at the SAME mark.
/// All 70 then share one history: a high of 10_200 and a drop to its stop (10_098) fire them in
/// activation order, 50 + a drain of 20.
#[test]
fn t_activations_have_their_own_cap_and_the_drain_finishes_them() {
    let mut ctx = fresh();
    let ids = crowd(&mut ctx, 70, 10_100);
    let c = tick(&mut ctx, 10_100);
    let want: Vec<(Id, u64, u64)> = ids.iter().map(|id| (*id, 10_100, 9_999)).collect();
    assert_eq!(activations(&c.logs), want[..ACTIVATION_CAP].to_vec());
    assert!(triggered(&c.logs).is_empty());
    assert!(deferred(&mut ctx));
    for id in &ids[ACTIVATION_CAP..] {
        assert_eq!(trail_state(&mut ctx, *id), (false, 0, 0));
    }
    let (more, c) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(activations(&c.logs), want[ACTIVATION_CAP..].to_vec());
    tick(&mut ctx, 10_200);
    for id in &ids {
        assert_eq!(trail_state(&mut ctx, *id), (true, 10_200, 10_098));
    }
    let c = tick(&mut ctx, 10_098);
    assert_eq!(ids_of(&triggered(&c.logs)), ids[..BUDGET].to_vec());
    let (more, c) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(ids_of(&triggered(&c.logs)), ids[BUDGET..].to_vec());
}

/// An activation carried past a tick with no drain activates at THAT tick's mark if its price is
/// still reached (the documented divergence: its peak starts there), and otherwise keeps waiting.
/// In the first case the 6 late ones tag the later tick, so at an equal stop they fire AFTER the
/// 64 (activation tick ascending).
#[test]
fn t_an_activation_carried_past_a_tick_activates_at_that_ticks_mark() {
    // Still reached: 10_150 ≥ 10_100. Stop ⌊10_150·0.99⌋ = 10_048 for all 70.
    let mut ctx = fresh();
    let ids = crowd(&mut ctx, 70, 10_100);
    tick(&mut ctx, 10_100);
    let c = tick(&mut ctx, 10_150);
    assert_eq!(
        activations(&c.logs),
        ids[ACTIVATION_CAP..].iter().map(|id| (*id, 10_150, 10_048)).collect::<Vec<_>>()
    );
    assert!(!deferred(&mut ctx));
    for id in &ids {
        assert_eq!(trail_state(&mut ctx, *id), (true, 10_150, 10_048));
    }
    let c = tick(&mut ctx, 10_048);
    assert_eq!(ids_of(&triggered(&c.logs)), ids[..BUDGET].to_vec());
    let (_, c) = drain(&mut ctx);
    let mut rest: Vec<Id> = ids[BUDGET..ACTIVATION_CAP].to_vec();
    rest.extend_from_slice(&ids[ACTIVATION_CAP..]);
    assert_eq!(ids_of(&triggered(&c.logs)), rest, "the late tag fires after the 64");

    // Fallen back: 10_050 < 10_100 — the 6 keep waiting, then activate at 10_120 (stop 10_018).
    let mut ctx = fresh();
    let ids = crowd(&mut ctx, 70, 10_100);
    tick(&mut ctx, 10_100);
    let c = tick(&mut ctx, 10_050);
    assert!(activations(&c.logs).is_empty() && triggered(&c.logs).is_empty());
    assert!(!deferred(&mut ctx));
    for id in &ids[ACTIVATION_CAP..] {
        assert_eq!(trail_state(&mut ctx, *id), (false, 0, 0));
    }
    assert_eq!(trail_state(&mut ctx, ids[0]), (true, 10_100, 9_999));
    let c = tick(&mut ctx, 10_120);
    assert_eq!(
        activations(&c.logs),
        ids[ACTIVATION_CAP..].iter().map(|id| (*id, 10_120, 10_018)).collect::<Vec<_>>()
    );
    assert_eq!(trail_state(&mut ctx, ids[0]), (true, 10_120, 10_018));
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Cancels: head, middle and tail of a bucket, a pending level, and the cancel refusals
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// The staggered path of `t_peaks_since_activation_with_staggered_activations` up to tick 5: one
/// bucket (b 100) holding X (peak 10_300, stop 10_197), Y (10_280, 10_177), Z (10_260, 10_157).
fn xyz() -> (TestCtx, [Id; 3]) {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 100, 1);
    tick(&mut ctx, 10_000);
    let x = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    tick(&mut ctx, 10_300);
    tick(&mut ctx, 10_250);
    let y = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    tick(&mut ctx, 10_280);
    tick(&mut ctx, 10_260);
    let z = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    (ctx, [x, y, z])
}

/// Removing the head, a middle or the tail of a bucket leaves the others firing at exactly their
/// own stops — in particular a new head must fire at ITS peak's stop, not the old head's.
#[test]
fn t_a_cancel_anywhere_in_a_bucket_leaves_the_rest_exact() {
    let stops = [10_197u64, 10_177, 10_157];
    for gone in 0..3 {
        let (mut ctx, ids) = xyz();
        let c = cancel_algo(&mut ctx, ALICE, ids[gone]);
        assert_eq!(algo_cancels(&c.logs), vec![(ids[gone], R_USER)]);
        assert_eq!(algo_event_count(&c.logs), 1);
        algo_gone(&mut ctx, ids[gone]);
        let left: Vec<Id> = (0..3).filter(|k| *k != gone).map(|k| ids[k]).collect();
        assert_eq!(open_algos(&mut ctx, ALICE), left);
        for k in 0..3 {
            // One tick ABOVE each stop fires nothing; the stop itself fires exactly that order.
            assert!(triggered(&tick(&mut ctx, stops[k] + 1).logs).is_empty(), "gone={gone} k={k}");
            let want = if k == gone { vec![] } else { vec![(ids[k], stops[k], stops[k])] };
            assert_eq!(triggered(&tick(&mut ctx, stops[k]).logs), want, "gone={gone} k={k}");
        }
        assert!(open_algos(&mut ctx, ALICE).is_empty());
    }
}

/// A pending order cancelled from the middle of its activation level is skipped; the others
/// activate FIFO. Stop at 10_500: ⌊10_395⌋.
#[test]
fn t_a_pending_cancel_mid_level() {
    let mut ctx = trail_fixture();
    let p: Vec<Id> = (0..3).map(|_| trail(&mut ctx, ALICE, SELL, 10_500, 100, 1)).collect();
    cancel_algo(&mut ctx, ALICE, p[1]);
    let c = tick(&mut ctx, 10_500);
    assert_eq!(activations(&c.logs), vec![(p[0], 10_500, 10_395), (p[2], 10_500, 10_395)]);
}

/// Cancel refusals: someone else's order, the wrong market, an order already fired (its record is
/// gone, so "not found"), an order already cancelled.
#[test]
fn t_cancel_refusals() {
    let mut ctx = trail_fixture();
    let id = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    let pend = trail(&mut ctx, ALICE, SELL, 10_500, 100, 1);
    rejected(
        &mut ctx,
        BOB,
        &cancelAlgoOrderCall { algoId: id.into(), marketId: MARKET_ID }.abi_encode(),
        E_CANCEL_OWNER,
    );
    rejected(
        &mut ctx,
        ALICE,
        &cancelAlgoOrderCall { algoId: pend.into(), marketId: 2 }.abi_encode(),
        E_CANCEL_MARKET,
    );
    tick(&mut ctx, 9_900);
    rejected(
        &mut ctx,
        ALICE,
        &cancelAlgoOrderCall { algoId: id.into(), marketId: MARKET_ID }.abi_encode(),
        E_CANCEL_NOT_FOUND,
    );
    cancel_algo(&mut ctx, ALICE, pend);
    rejected(
        &mut ctx,
        ALICE,
        &cancelAlgoOrderCall { algoId: pend.into(), marketId: MARKET_ID }.abi_encode(),
        E_CANCEL_NOT_FOUND,
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Liquidation cancel-all
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// The oracle sweep liquidates ALICE (long 3 @ $100, margin $60: at 9_100 her equity is
/// 60 − 3·9 = $33 < maintenance 3·91/6 = $45.5) and cancels BOTH her trailing stops — the
/// activated one at the HEAD of the b-1000 bucket and the pending one at the head of the 10_500
/// level — with reason 3. Neither fires on that tick (stop 9_000 < 9_100). DAVE's orders behind
/// hers in the same bucket and level are untouched and stay exact: his pending one activates at
/// 10_500 (stop 10_395) and both of his fire on the drop to 9_450 (best stop first).
#[test]
fn t_liquidation_cancels_pending_and_activated_trailers() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 3, 5);
    seed_pos(&mut ctx, DAVE, 1, 1);
    seed_pos(&mut ctx, ERIN, -4, 1);
    let a_act = trail(&mut ctx, ALICE, SELL, 0, 1_000, 3);
    let d_act = trail(&mut ctx, DAVE, SELL, 0, 1_000, 1);
    let a_pend = trail(&mut ctx, ALICE, SELL, 10_500, 100, 3);
    let d_pend = trail(&mut ctx, DAVE, SELL, 10_500, 100, 1);
    rest(&mut ctx, BOB, BUY, 9_100, 3);

    let c = tick(&mut ctx, 9_100);
    let liq = evs::<Liquidation>(&c.logs);
    assert_eq!(liq.len(), 1);
    assert_eq!(liq[0].user, ALICE);
    assert_eq!(amount(&mut ctx, ALICE), 0);
    let mut cancels = algo_cancels(&c.logs);
    cancels.sort();
    let mut want = vec![(a_act, R_LIQUIDATION), (a_pend, R_LIQUIDATION)];
    want.sort();
    assert_eq!(cancels, want);
    assert!(triggered(&c.logs).is_empty() && activations(&c.logs).is_empty());
    algo_gone(&mut ctx, a_act);
    algo_gone(&mut ctx, a_pend);
    assert!(open_algos(&mut ctx, ALICE).is_empty());
    assert_eq!(trail_state(&mut ctx, d_act), (true, MARK, 9_000));
    assert_eq!(trail_state(&mut ctx, d_pend), (false, 0, 0));

    let c = tick(&mut ctx, 10_500);
    assert_eq!(activations(&c.logs), vec![(d_pend, 10_500, 10_395)]);
    assert_eq!(trail_state(&mut ctx, d_act), (true, 10_500, 9_450));
    let c = tick(&mut ctx, 9_450);
    assert_eq!(triggered(&c.logs), vec![(d_pend, 10_395, 9_450), (d_act, 9_450, 9_450)]);
    assert!(open_algos(&mut ctx, DAVE).is_empty());
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// The signed path and STP inheritance
// ═════════════════════════════════════════════════════════════════════════════════════════════

const SIGNED_TS: u64 = 1; // == the default test block's timestamp
const SIGNED_RECV: u64 = 60;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[9u8; 32])
}

fn register_alice_key(ctx: &mut TestCtx) {
    ok(
        ctx,
        ALICE,
        &registerApiKeyCall { keyId: 1, pubkey: key().verifying_key().to_bytes().into(), expiry: 0 }
            .abi_encode(),
    );
}

/// `placeAlgoOrderSigned` for ALICE, a reduce-only trailing stop: see [`signed_algo`].
fn signed_trail(
    ctx: &mut TestCtx,
    side: u8,
    activation: u64,
    cb: u64,
    qty: u64,
    salt: u8,
) -> (Vec<u8>, Vec<u8>) {
    signed_algo(ctx, TRAILING, side, activation, cb, qty, RO, salt)
}

/// `placeAlgoOrderSigned` for ALICE: the 114-byte message rebuilt from the ABI docs. Returns the
/// calldata and the signature.
#[allow(clippy::too_many_arguments)]
fn signed_algo(
    ctx: &mut TestCtx,
    kind: u8,
    side: u8,
    trigger: u64,
    price: u64,
    qty: u64,
    flags: u8,
    salt: u8,
) -> (Vec<u8>, Vec<u8>) {
    let mut client = [0u8; 16];
    client[15] = salt;
    let mut msg = Vec::new();
    msg.extend_from_slice(b"perpdex_v1_algo");
    msg.extend_from_slice(&ctx.chain_id().to_be_bytes());
    msg.extend_from_slice(ALICE.as_slice());
    msg.push(0); // algoType
    msg.extend_from_slice(&MARKET_ID.to_be_bytes());
    msg.extend_from_slice(&[side, kind]);
    msg.extend_from_slice(&trigger.to_be_bytes());
    msg.extend_from_slice(&price.to_be_bytes());
    msg.extend_from_slice(&qty.to_be_bytes());
    msg.extend_from_slice(&[0, 0]); // tif, workingType
    msg.extend_from_slice(&client);
    msg.push(flags);
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.push(1); // keyId
    assert_eq!(msg.len(), 114);
    let sig = key().sign(&msg).to_bytes().to_vec();
    let input = placeAlgoOrderSignedCall {
        account: ALICE,
        algoType: 0,
        marketId: MARKET_ID,
        side,
        orderType: kind,
        triggerPrice: trigger,
        price,
        quantity: qty,
        tif: 0,
        workingType: 0,
        clientAlgoId: client.into(),
        flags,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 1,
        signature: sig.clone().into(),
    }
    .abi_encode();
    (input, sig)
}

/// The signed variant works unchanged: relayed by anyone, the algo id is keccak256(signature), an
/// immediate one activates in the same transaction and a pending one in the tick, and both fire at
/// their stops (H 10_200: b 50 → ⌊10_149⌋, b 100 → 10_098). A malformed request is refused BEFORE
/// the burn (zero writes, resubmittable to the same answer); a refusal at placement (the opening
/// side) comes after the burn — the signature is spent and its resubmission is a duplicate.
#[test]
fn t_the_signed_path_places_activates_and_fires_trailing_stops() {
    let mut ctx = trail_fixture();
    register_alice_key(&mut ctx);
    let (input, sig) = signed_trail(&mut ctx, SELL, 0, 100, 10, 1);
    let c = ok(&mut ctx, DAVE, &input);
    let id = placeAlgoOrderSignedCall::abi_decode_returns(&c.out.bytes).unwrap().0;
    assert_eq!(id, keccak256(&sig).0);
    assert_eq!(evs::<AlgoOrderPlaced>(&c.logs)[0].user, ALICE);
    assert_eq!(activations(&c.logs), vec![(id, MARK, 9_900)]);
    let (input, sig2) = signed_trail(&mut ctx, SELL, 10_200, 50, 10, 2);
    let c = ok(&mut ctx, DAVE, &input);
    let pend = keccak256(&sig2).0;
    assert!(activations(&c.logs).is_empty());
    assert_eq!(open_algos(&mut ctx, ALICE), vec![id, pend]);

    let c = tick(&mut ctx, 10_200);
    assert_eq!(activations(&c.logs), vec![(pend, 10_200, 10_149)]);
    let c = tick(&mut ctx, 10_149);
    assert_eq!(triggered(&c.logs), vec![(pend, 10_149, 10_149)]);
    let c = tick(&mut ctx, 10_098);
    assert_eq!(triggered(&c.logs), vec![(id, 10_098, 10_098)]);

    let (bad, _) = signed_trail(&mut ctx, SELL, 0, 15, 1, 3);
    let reason = format!("placeAlgoOrderSigned: {E_CALLBACK}");
    rejected(&mut ctx, DAVE, &bad, &reason);
    rejected(&mut ctx, DAVE, &bad, &reason);

    let (wrong_side, _) = signed_trail(&mut ctx, BUY, 0, 100, 1, 4);
    let _ = take_logs(&mut ctx);
    let w = ctx.perp_write_count();
    let before = world(&mut ctx);
    let out = run_perp_dex_call(&wrong_side, GAS, DAVE, U256::ZERO, false, &mut ctx).unwrap();
    assert_eq!(revert_reason(&out), E_TRAIL_BIND);
    assert!(ctx.perp_write_count() > w, "the signature was burned");
    assert!(take_logs(&mut ctx).is_empty());
    assert!(world(&mut ctx) == before, "only the seen-signature set moved");
    rejected(&mut ctx, DAVE, &wrong_side, E_DUP_SIG);
}

/// The fired order INHERITS the STP mode set at placement (`OrderPlaced.flags` = reduce-only +
/// those bits) and matches with it against its owner's own resting bid (2 @ 9_900, ahead of BOB's
/// 10 @ 9_890): EXPIRE_MAKER cancels ALICE's bid (reason 9) and closes 10 against BOB;
/// EXPIRE_TAKER stops at it and expires, leaving the bid and the position; EXPIRE_BOTH cancels
/// the bid AND expires.
#[test]
fn t_the_fired_order_inherits_the_stp_mode() {
    for (stp, mode) in [(0u8, 0u8), (STP_TAKER, 1), (STP_BOTH, 2)] {
        let mut ctx = fresh();
        seed_pos(&mut ctx, ALICE, 10, 1);
        seed_pos(&mut ctx, ERIN, -10, 1);
        let own = rest(&mut ctx, ALICE, BUY, 9_900, 2);
        rest(&mut ctx, BOB, BUY, 9_890, 10);
        let (id, _) = place_algo(&mut ctx, ALICE, &trail_call(SELL, 0, 100, 10, RO | stp));
        assert_eq!(algo(&mut ctx, id).selfTradePreventionMode, mode);
        let c = tick(&mut ctx, 9_900);
        assert_eq!(triggered(&c.logs), vec![(id, 9_900, 9_900)], "mode {mode}");
        let p = placed(&c.logs);
        assert_eq!(p.len(), 1);
        assert_eq!((p[0].0, p[0].1, p[0].2, p[0].3, p[0].4), (id, SELL, 10, MKT, RO | stp), "mode {mode}");
        let bids: Vec<Id> =
            storage::load_buy_orders(&mut ctx, ALICE, MARKET_ID)
                .unwrap()
                .iter()
                .map(|e| e.order_id)
                .collect();
        match mode {
            0 => {
                assert_eq!(book_cancels(&c.logs), vec![(own, R_STP)]);
                assert_eq!(trades(&c.logs), vec![(ALICE, BOB, 9_890, 10)]);
                assert_eq!(amount(&mut ctx, ALICE), 0);
                assert!(bids.is_empty());
            }
            1 => {
                assert!(book_cancels(&c.logs).is_empty() && trades(&c.logs).is_empty());
                assert_eq!(amount(&mut ctx, ALICE), 10);
                assert_eq!(bids, vec![own]);
            }
            _ => {
                assert_eq!(book_cancels(&c.logs), vec![(own, R_STP)]);
                assert!(trades(&c.logs).is_empty());
                assert_eq!(amount(&mut ctx, ALICE), 10);
                assert!(bids.is_empty());
            }
        }
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Per-tick work does not depend on N (spec commit 3: "Per-tick work bound (excluding fire())
// must be independent of N"; memo §2.6)
// ═════════════════════════════════════════════════════════════════════════════════════════════
//
// Measured at the HOST: an `InMemoryHost` behind a wrapper that records every key the engine asks
// it for (byte-tier `perp_load` and the cross-block decoded `perp_load_arc` alike). The simulated
// block is ended right before each measured tick, so every read in it is cold — the distinct-key
// count is the number of storage objects the tick touched, whatever the engine caches inside a
// block. Writes are the host's own `perp_write_count`.

struct Counting {
    inner: InMemoryHost,
    on: bool,
    keys: HashSet<B256>,
    loads: u64,
}

impl Counting {
    fn new() -> Self {
        Self { inner: InMemoryHost::new(1), on: false, keys: HashSet::new(), loads: 0 }
    }
}

impl PerpHost for Counting {
    type Checkpoint = usize;
    fn live(&mut self) -> &mut crate::typed_store::TypedPerpStore {
        self.inner.live()
    }
    fn perp_load(&mut self, key: B256) -> Result<Vec<u8>, crate::PerpError> {
        if self.on {
            self.keys.insert(key);
            self.loads += 1;
        }
        self.inner.perp_load(key)
    }
    fn perp_store(&mut self, key: B256, value: Vec<u8>) {
        self.inner.perp_store(key, value)
    }
    fn perp_load_arc(
        &mut self,
        key: B256,
    ) -> Result<Option<std::sync::Arc<context_interface::journaled_state::PerpBlob>>, crate::PerpError>
    {
        if self.on {
            self.keys.insert(key);
        }
        self.inner.perp_load_arc(key)
    }
    fn log(&mut self, log: Log) {
        self.inner.log(log)
    }
    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }
    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }
    fn external_balance(&mut self, token: Address, owner: Address) -> Result<U256, crate::PerpError> {
        self.inner.external_balance(token, owner)
    }
    fn set_external_balance(
        &mut self,
        token: Address,
        owner: Address,
        value: U256,
    ) -> Result<(), crate::PerpError> {
        self.inner.set_external_balance(token, owner, value)
    }
    fn checkpoint(&mut self) -> usize {
        self.inner.checkpoint()
    }
    fn checkpoint_commit(&mut self) {
        self.inner.checkpoint_commit()
    }
    fn checkpoint_revert(&mut self, cp: usize) {
        self.inner.checkpoint_revert(cp)
    }
    fn perp_write_count(&self) -> u64 {
        self.inner.perp_write_count()
    }
    fn call_depth(&self) -> usize {
        self.inner.call_depth()
    }
}

/// The seven trailing key prefixes, as the ABI's data model names them (`keys.rs`).
const TRAIL_PREFIXES: [&[u8; 4]; 7] = [b"trlm", b"trls", b"trld", b"trlb", b"trlo", b"tpnp", b"tpnl"];

fn trailing_prefix(k: &B256) -> Option<&'static [u8; 4]> {
    TRAIL_PREFIXES.iter().copied().find(|p| k.0[..4] == p[..])
}

fn h_ok(h: &mut Counting, caller: Address, input: &[u8]) -> PerpOutput {
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, h).unwrap();
    assert!(!out.reverted, "unexpected revert: {}", revert_reason(&out));
    out
}

fn h_setup(n_long: u16, n_short: u16) -> (Counting, Vec<Address>, Vec<Address>) {
    let mut h = Counting::new();
    storage::save_admin(&mut h, ADMIN).unwrap();
    storage::save_market(&mut h, &the_market(1, 1, 1)).unwrap();
    storage::save_mark_price(&mut h, MARKET_ID, MARK).unwrap();
    let seed = |h: &mut Counting, k: u16, amount: i64| {
        let u = extra(k);
        let mut a = storage::load_account(h, u).unwrap();
        a.perp_wallet_balance = WALLET;
        storage::save_account(h, u, a, AccountUpdateReason::Adjustment).unwrap();
        storage::save_position(
            h,
            u,
            MARKET_ID,
            &PerpPosition {
                amount,
                v_quote_balance: -amount * MARK as i64 * 10_000,
                margin: amount.abs() * MARK as i64 * 10_000,
                leverage: 1,
                ..PerpPosition::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
        u
    };
    let longs = (0..n_long).map(|k| seed(&mut h, k, 1_000)).collect();
    let shorts = (0..n_short).map(|k| seed(&mut h, 1_000 + k, -1_000)).collect();
    (h, longs, shorts)
}

fn h_ts(h: &mut Counting) -> u64 {
    storage::load_index_price_state(h, MARKET_ID).unwrap().timestamp
}

fn h_tick(h: &mut Counting, index: u64) {
    let ts = h_ts(h) + INTERVAL;
    h_ok(
        h,
        ADMIN,
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: ts }.abi_encode(),
    );
    assert_eq!(storage::load_mark_price(h, MARKET_ID).unwrap(), index);
}

fn h_trail(h: &mut Counting, user: Address, side: u8, a: u64, cb: u64) -> Id {
    let out = h_ok(h, user, &trail_call(side, a, cb, 1, RO));
    placeAlgoOrderCall::abi_decode_returns(&out.bytes).unwrap().0
}

fn h_view(h: &mut Counting, id: Id) -> (bool, u64, u64) {
    let input = getAlgoOrderCall { algoId: id.into() }.abi_encode();
    let out = run_perp_dex_call(&input, GAS, ERIN, U256::ZERO, true, h).unwrap();
    assert!(!out.reverted, "{}", revert_reason(&out));
    let r = getAlgoOrderCall::abi_decode_returns(&out.bytes).unwrap();
    (r.activated, r.peakPrice, r.currentStop)
}

/// What one tick cost, measured cold.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cost {
    keys: usize,
    loads: u64,
    writes: u64,
    logs: usize,
    /// The trailing objects among them, by prefix (one entry per distinct key).
    trailing_keys: Vec<String>,
}

fn measured_tick(h: &mut Counting, index: u64) -> Cost {
    let ts = h_ts(h) + INTERVAL;
    let input =
        updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: ts }.abi_encode();
    h.inner.end_block();
    h.keys.clear();
    h.loads = 0;
    h.on = true;
    let w0 = h.perp_write_count();
    let l0 = h.inner.logs.len();
    let out = run_perp_dex_call(&input, GAS, ADMIN, U256::ZERO, false, h).unwrap();
    h.on = false;
    assert!(!out.reverted, "{}", revert_reason(&out));
    let mut trailing_keys: Vec<String> = h
        .keys
        .iter()
        .filter_map(trailing_prefix)
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    trailing_keys.sort();
    let cost = Cost {
        keys: h.keys.len(),
        loads: h.loads,
        writes: h.perp_write_count() - w0,
        logs: h.inner.logs.len() - l0,
        trailing_keys,
    };
    assert_eq!(storage::load_mark_price(h, MARKET_ID).unwrap(), index);
    cost
}

const SELL_CBS: std::ops::RangeInclusive<u64> = 11..=100; // ×10 → 110..=1000 bps
const BUY_CBS: std::ops::RangeInclusive<u64> = 20..=100; // ×10 → 200..=1000 bps

/// One market with 20 long and 20 short accounts. ALICE's stand-in `u0` holds one activated SELL
/// (b 110) and one SELL pending at 10_100 (b 110) in BOTH runs; with `many`, every account adds
/// 25 more — activated over three ticks (10_000, 9_990, 9_980, so the SELL side has a 3-entry
/// history), pending at four prices per side — 1_002 trailing orders in all, over 90 SELL and 81
/// BUY buckets. Then three measured ticks: a QUIET one (9_980 again: no new extreme on either
/// side, nothing due), a NEW HIGH (10_050) and a new high that ACTIVATES `u0`'s pending order
/// (10_100). No thresholds are crossed at any point (SELL b ≥ 110 below a 9_980 low, BUY b ≥ 200
/// above a 9_980 trough, nothing fires on a new extreme).
fn n_scenario(many: bool) -> (Vec<Cost>, usize) {
    let (mut h, longs, shorts) = h_setup(20, 20);
    let mut n = 0usize;
    let mut k = 0u64;
    let mut next_cb = |sell: bool| {
        k += 1;
        let r = if sell { SELL_CBS } else { BUY_CBS };
        (r.start() + k % (r.end() - r.start() + 1)) * 10
    };
    h_tick(&mut h, 10_000);
    h_trail(&mut h, longs[0], SELL, 0, 110);
    h_trail(&mut h, longs[0], SELL, 10_100, 110);
    n += 2;
    let rounds: [(u64, usize, bool); 3] = [(10_000, 9, true), (9_990, 6, false), (9_980, 6, false)];
    for (r, (m, per, with_pending)) in rounds.into_iter().enumerate() {
        if r > 0 {
            h_tick(&mut h, m);
        }
        if !many {
            continue;
        }
        for u in &longs {
            for _ in 0..per {
                h_trail(&mut h, *u, SELL, 0, next_cb(true));
                n += 1;
            }
            if with_pending {
                for a in [10_200, 10_300, 10_400, 10_500] {
                    h_trail(&mut h, *u, SELL, a, next_cb(true));
                    n += 1;
                }
            }
        }
        for u in &shorts {
            for _ in 0..per {
                h_trail(&mut h, *u, BUY, 0, next_cb(false));
                n += 1;
            }
            if with_pending {
                for a in [9_500, 9_600, 9_700, 9_800] {
                    h_trail(&mut h, *u, BUY, a, next_cb(false));
                    n += 1;
                }
            }
        }
    }
    let costs = vec![
        measured_tick(&mut h, 9_980),
        measured_tick(&mut h, 10_050),
        measured_tick(&mut h, 10_100),
    ];
    // The 10_100 tick activated `u0`'s pending order and nothing else.
    let acts = evs::<AlgoOrderActivated>(&h.inner.logs[h.inner.logs.len() - costs[2].logs..]);
    assert_eq!(acts.len(), 1);
    assert_eq!((acts[0].user, acts[0].markPrice, acts[0].stopPrice), (longs[0], 10_100, 9_988));
    assert!(evs::<AlgoOrderTriggered>(&h.inner.logs).is_empty(), "nothing fired anywhere");
    (costs, n)
}

/// 2 vs 1_002 trailing orders: the three ticks cost the same — the same storage objects, the same
/// number of byte loads, the same writes and logs. On the quiet tick the only trailing object read
/// is the per-market meta, and nothing at all is written for trailing stops.
#[test]
fn t_per_tick_work_does_not_grow_with_the_number_of_trailing_orders() {
    let (few, n_few) = n_scenario(false);
    let (many, n_many) = n_scenario(true);
    assert_eq!((n_few, n_many), (2, 1_002));
    for (k, name) in ["quiet", "new high", "new high + activation"].iter().enumerate() {
        assert_eq!(few[k], many[k], "{name}: the cost moved with N");
    }
    for (k, name) in ["quiet", "new high", "new high + activation"].iter().enumerate() {
        eprintln!("{name}: {:?}", many[k]);
    }
    assert_eq!(few[0].trailing_keys, vec!["trlm"], "a quiet tick reads one trailing object");
    // Nothing trailing is written on a quiet tick: compare with a market that has NO trailing
    // order at all.
    let (mut h, _, _) = h_setup(20, 20);
    for m in [10_000, 9_990, 9_980] {
        h_tick(&mut h, m);
    }
    let none = measured_tick(&mut h, 9_980);
    assert_eq!(none.writes, few[0].writes, "a quiet tick writes nothing for trailing stops");
    assert_eq!(none.logs, few[0].logs);
}

/// memo §2.6's adversarial path (adv.py): a decline of `d` ticks with one activation per tick
/// builds a `d`-entry history (b 1000, so nothing fires on the way down), then ONE spike to 10_500
/// pops all of it. Returns the spike's cost, the cost of a quiet tick after it, and the ids.
fn decline_then_spike(d: usize) -> (Cost, Cost, Counting, Vec<Id>) {
    // The SAME 13 position holders whatever `d` is: the liquidation sweep reads every position
    // in the market, which is not trailing work and must not differ between the two runs.
    assert!(d <= 13 * 32);
    let (mut h, longs, _) = h_setup(13, 0);
    h_tick(&mut h, 10_000);
    let mut ids = Vec::new();
    for k in 0..d {
        ids.push(h_trail(&mut h, longs[k / 32], SELL, 0, 1_000));
        h_tick(&mut h, 10_000 - 1 - k as u64);
    }
    // Before the spike every order still has its own activation mark as its peak.
    assert_eq!(h_view(&mut h, ids[0]), (true, 10_000, 9_000));
    let last = 10_000 - (d as u64 - 1);
    assert_eq!(h_view(&mut h, ids[d - 1]), (true, last, spec_thr(true, last, 1_000)));
    let spike = measured_tick(&mut h, 10_500);
    let quiet = measured_tick(&mut h, 10_500);
    (spike, quiet, h, ids)
}

/// The spike costs at most ⌈log2 pages⌉ + 1 more storage objects for a 400-entry history (7 pages
/// of 64) than for a 16-entry one (1 page), and its extra writes are only the lazy garbage
/// collection's (≤ 8 + 8 page deletes per tick); the next quiet tick is identical. After it every
/// order's peak is the spike (stop 9_450), and the drop to 9_450 fires all 400 in activation
/// order, 50 per sweep. When they are all gone and the GC has run, no trailing object is left.
#[test]
fn t_decline_then_spike_costs_log_pages_and_collects_its_garbage() {
    let (s16, q16, _, _) = decline_then_spike(16);
    let (s400, q400, mut h, ids) = decline_then_spike(400);
    eprintln!("decline-then-spike, d=16: spike {s16:?}, quiet after {q16:?}");
    eprintln!("decline-then-spike, d=400: spike {s400:?}, quiet after {q400:?}");
    assert!(s400.keys >= s16.keys && s400.keys - s16.keys <= 4, "spike keys {s400:?} vs {s16:?}");
    assert!(
        s400.writes >= s16.writes && s400.writes - s16.writes <= 16,
        "spike writes {s400:?} vs {s16:?}"
    );
    assert_eq!(s400.logs, s16.logs);
    assert_eq!(q400, q16, "after the spike the 400-order market ticks like the 16-order one");

    for id in &ids {
        assert_eq!(h_view(&mut h, *id), (true, 10_500, 9_450));
    }
    let l0 = h.inner.logs.len();
    h_tick(&mut h, 9_450);
    let mut fired = ids_of(&triggered(&h.inner.logs[l0..]));
    assert_eq!(fired.len(), BUDGET);
    loop {
        let l = h.inner.logs.len();
        let out = h_ok(&mut h, ERIN, &drainDeferredWorkCall { marketId: MARKET_ID }.abi_encode());
        let more = drainDeferredWorkCall::abi_decode_returns(&out.bytes).unwrap();
        let batch = triggered(&h.inner.logs[l..]);
        assert!(batch.iter().all(|t| t.1 == 9_450 && t.2 == 9_450));
        assert!(batch.len() <= BUDGET);
        fired.extend(ids_of(&batch));
        if !more {
            break;
        }
    }
    assert_eq!(fired, ids, "all 400, in activation order");
    for _ in 0..4 {
        h_tick(&mut h, 9_450);
    }
    h.inner.end_block();
    let left: Vec<&[u8; 4]> = h
        .inner
        .committed
        .keys()
        .filter_map(trailing_prefix)
        .filter(|p| *p != b"trlm")
        .collect();
    assert!(left.is_empty(), "trailing objects left behind: {left:?}");
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// A seeded random differential run against an independent O(N) per-order model
// ═════════════════════════════════════════════════════════════════════════════════════════════
//
// The model is the spec read literally: every activated order carries its own peak, updated on
// every sweep BEFORE anything fires; the firing set is the u128 predicate; the fire order is the
// memo's directory order (threshold best-first, activation tick, callback, then activation FIFO),
// SELL side then BUY side, on the shared 50 budget; activations FIFO per price (SELL ascending,
// BUY descending) on their own 64 cap; the deferred flag is any cap stopped with work left, or
// the shared budget spent with liquidation candidates (every account here holds a position)
// unexamined; a drain with the flag clear does nothing. No order ever fills (there is no book).
// Positions end only when the run itself closes and reopens one (decision 1): the model keeps a
// per-account generation, and an order placed under an older one is spent with 11 where it would
// act — at activation (no `AlgoOrderActivated`, a cap unit spent) or at fire (`AlgoOrderTriggered`
// as usual, a budget unit spent).

#[derive(Clone, Debug)]
struct MOrd {
    id: Id,
    owner: Address,
    sell: bool,
    cb: u64,
    /// Effective activation price (the mark when sent as 0).
    act: u64,
    active: bool,
    tag: u64,
    peak: u64,
    /// Activation order.
    ord: u64,
    /// Placement order.
    placed: u64,
    /// The owner's position generation at placement.
    gen: u64,
}

#[derive(Default)]
struct Model {
    seq: u64,
    mark: u64,
    orders: Vec<MOrd>,
    next_ord: u64,
    next_placed: u64,
    flag: bool,
    gens: std::collections::HashMap<Address, u64>,
}

type Ev = (Id, u64, u64);

impl Model {
    fn place(&mut self, id: Id, owner: Address, sell: bool, a: u64, cb: u64) -> Vec<Ev> {
        let placed = self.next_placed;
        self.next_placed += 1;
        let gen = self.gen(owner);
        let mut o =
            MOrd { id, owner, sell, cb, act: a, active: false, tag: 0, peak: 0, ord: 0, placed, gen };
        let mut out = Vec::new();
        if a == 0 {
            o.act = self.mark;
            o.active = true;
            o.tag = self.seq;
            o.peak = self.mark;
            o.ord = self.next_ord;
            self.next_ord += 1;
            out.push((id, self.mark, spec_stop(sell, self.mark, cb, 1)));
        }
        self.orders.push(o);
        out
    }

    fn cancel(&mut self, id: Id) {
        self.orders.retain(|o| o.id != id);
    }

    fn gen(&self, owner: Address) -> u64 {
        self.gens.get(&owner).copied().unwrap_or(0)
    }

    /// `(fires, activations, spent with PositionClosed)`, each in stream order.
    fn sweep(&mut self) -> (Vec<Ev>, Vec<Ev>, Vec<Id>) {
        let (m, seq) = (self.mark, self.seq);
        for o in self.orders.iter_mut().filter(|o| o.active) {
            o.peak = if o.sell { o.peak.max(m) } else { o.peak.min(m) };
        }
        let mut budget = BUDGET;
        let mut deferred = false;
        let mut fires = Vec::new();
        let mut spent = Vec::new();
        for sell in [true, false] {
            let mut cand: Vec<MOrd> = self
                .orders
                .iter()
                .filter(|o| o.active && o.sell == sell && spec_fires(sell, m, o.peak, o.cb))
                .cloned()
                .collect();
            cand.sort_by_key(|o| {
                let t = spec_thr(sell, o.peak, o.cb);
                (if sell { u64::MAX - t } else { t }, o.tag, o.cb, o.ord)
            });
            for o in cand {
                if budget == 0 {
                    deferred = true;
                    break;
                }
                fires.push((o.id, spec_stop(sell, o.peak, o.cb, 1), m));
                if o.gen != self.gen(o.owner) {
                    spent.push(o.id);
                }
                self.cancel(o.id);
                budget -= 1;
            }
        }
        let mut cap = ACTIVATION_CAP;
        let mut acts = Vec::new();
        for sell in [true, false] {
            let mut due: Vec<MOrd> = self
                .orders
                .iter()
                .filter(|o| !o.active && o.sell == sell && if sell { m >= o.act } else { m <= o.act })
                .cloned()
                .collect();
            due.sort_by_key(|o| (if sell { o.act } else { u64::MAX - o.act }, o.placed));
            for d in due {
                if cap == 0 {
                    deferred = true;
                    break;
                }
                cap -= 1;
                if d.gen != self.gen(d.owner) {
                    spent.push(d.id);
                    self.cancel(d.id);
                    continue;
                }
                let ord = self.next_ord;
                self.next_ord += 1;
                let o = self.orders.iter_mut().find(|o| o.id == d.id).unwrap();
                o.active = true;
                o.tag = seq;
                o.peak = m;
                o.ord = ord;
                acts.push((o.id, m, spec_stop(sell, m, o.cb, 1)));
            }
        }
        if budget == 0 {
            deferred = true; // the liquidation sweep stops on the spent budget
        }
        self.flag = deferred;
        (fires, acts, spent)
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

#[derive(Default, Debug)]
struct Coverage {
    fires: usize,
    activations: usize,
    immediate: usize,
    fire_overflows: usize,
    activation_overflows: usize,
    drains_with_work: usize,
    cancels_active: usize,
    cancels_pending: usize,
    max_live: usize,
    long_trends: usize,
    reopened: usize,
    spent_at_fire: usize,
    spent_at_activation: usize,
}

impl Coverage {
    fn spent(&mut self, fires: &[Ev], spent: &[Id]) {
        let fired: HashSet<Id> = fires.iter().map(|e| e.0).collect();
        self.spent_at_fire += spent.iter().filter(|id| fired.contains(*id)).count();
        self.spent_at_activation += spent.iter().filter(|id| !fired.contains(*id)).count();
    }
}

/// The ids `AlgoOrderCancelled(PositionClosed)` reports, in stream order.
fn spent_ids(logs: &[Log]) -> Vec<Id> {
    algo_cancels(logs).into_iter().filter(|x| x.1 == R_POSITION_CLOSED).map(|x| x.0).collect()
}

fn check_views(ctx: &mut TestCtx, model: &Model, sample: Option<&mut Rng>) {
    let n = model.orders.len();
    let picks: Vec<usize> = match sample {
        Some(r) if n > 24 => (0..24).map(|_| r.below(n as u64) as usize).collect(),
        _ => (0..n).collect(),
    };
    for i in picks {
        let o = &model.orders[i];
        let r = algo(ctx, o.id);
        let want = if o.active {
            (true, o.peak, spec_stop(o.sell, o.peak, o.cb, 1))
        } else {
            (false, 0, 0)
        };
        assert_eq!((r.activated, r.peakPrice, r.currentStop), want, "view of {:?}", o);
        assert_eq!(r.triggerPrice, o.act, "effective activation price of {:?}", o);
    }
}

fn run_differential(seed: u64, steps: usize, cov: &mut Coverage) {
    let mut rng = Rng(seed);
    let mut ctx = fresh();
    let longs: Vec<Address> = (0..6).map(|k| new_user(&mut ctx, 200 + k)).collect();
    let shorts: Vec<Address> = (0..6).map(|k| new_user(&mut ctx, 300 + k)).collect();
    for u in &longs {
        seed_pos(&mut ctx, *u, 1_000, 1);
    }
    for u in &shorts {
        seed_pos(&mut ctx, *u, -1_000, 1);
    }
    let mut model = Model { mark: MARK, ..Model::default() };
    let cbs = [10u64, 20, 30, 50, 100, 150, 200, 300, 500, 1_000];
    let mut trend = 0usize;

    let place_one = |ctx: &mut TestCtx,
                     model: &mut Model,
                     rng: &mut Rng,
                     sell: bool,
                     a: u64,
                     cb: u64|
     -> bool {
        let pool = if sell { &longs } else { &shorts };
        let start = rng.below(pool.len() as u64) as usize;
        let Some(owner) = (0..pool.len())
            .map(|k| pool[(start + k) % pool.len()])
            .find(|u| model.orders.iter().filter(|o| o.owner == *u).count() < 32)
        else {
            return false;
        };
        let (id, c) = place_algo(ctx, owner, &trail_call(if sell { SELL } else { BUY }, a, cb, 1, RO));
        let want = model.place(id, owner, sell, a, cb);
        assert_eq!(activations(&c.logs), want, "seed {seed}: placement");
        true
    };

    for step in 0..steps {
        let tag = format!("seed {seed} step {step}");
        let roll = rng.below(100);
        if trend > 0 {
            // A slow decline (or rise) with one activation per tick: the history grows by one
            // entry per tick, across page boundaries.
            trend -= 1;
            let sell = trend.is_multiple_of(2) || rng.chance(70);
            if model.orders.len() < 300 {
                place_one(&mut ctx, &mut model, &mut rng, sell, 0, 1_000);
            }
            let m = if sell { model.mark - 1 - rng.below(3) } else { model.mark + 1 + rng.below(3) };
            let c = tick(&mut ctx, m);
            model.seq = current_ts(&mut ctx);
            model.mark = m;
            let (f, a, x) = model.sweep();
            assert_eq!(triggered(&c.logs), f, "{tag}: trend fires");
            assert_eq!(activations(&c.logs), a, "{tag}: trend activations");
            assert_eq!(spent_ids(&c.logs), x, "{tag}: trend expiries");
            cov.spent(&f, &x);
            cov.fires += f.len();
            cov.activations += a.len();
            continue;
        }
        if roll < 2 {
            trend = 70 + rng.below(40) as usize;
            cov.long_trends += 1;
        } else if roll < 30 {
            let sell = rng.chance(50);
            let cb = if rng.chance(20) { 10 * (1 + rng.below(100)) } else { cbs[rng.below(10) as usize] };
            let a = if rng.chance(40) {
                0
            } else if sell {
                model.mark + 1 + rng.below(300)
            } else {
                model.mark.saturating_sub(1 + rng.below(300)).max(1)
            };
            if a == 0 {
                cov.immediate += 1;
            }
            place_one(&mut ctx, &mut model, &mut rng, sell, a, cb);
        } else if roll < 34 {
            // A crowd: same side, same callback, either all now or all at one nearby price.
            let sell = rng.chance(50);
            let cb = cbs[rng.below(10) as usize];
            let n = 20 + rng.below(60);
            let a = if rng.chance(50) {
                0
            } else if sell {
                model.mark + 1 + rng.below(5)
            } else {
                model.mark - 1 - rng.below(5)
            };
            for _ in 0..n {
                if !place_one(&mut ctx, &mut model, &mut rng, sell, a, cb) {
                    break;
                }
            }
        } else if roll < 37 {
            // Close an account's position and reopen it the same way round: every order it holds
            // now belongs to a position that ended.
            let k = rng.below(12) as usize;
            let (u, amt) = if k < 6 { (longs[k], 1_000) } else { (shorts[k - 6], -1_000) };
            let flat = PerpPosition::default();
            storage::save_position(&mut ctx, u, MARKET_ID, &flat, AccountUpdateReason::Adjustment)
                .unwrap();
            seed_pos(&mut ctx, u, amt, 1);
            *model.gens.entry(u).or_insert(0) += 1;
            cov.reopened += 1;
        } else if roll < 46 {
            if !model.orders.is_empty() {
                let o = model.orders[rng.below(model.orders.len() as u64) as usize].clone();
                let c = cancel_algo(&mut ctx, o.owner, o.id);
                assert_eq!(algo_cancels(&c.logs), vec![(o.id, R_USER)], "{tag}");
                model.cancel(o.id);
                if o.active {
                    cov.cancels_active += 1;
                } else {
                    cov.cancels_pending += 1;
                }
            }
        } else if roll < 86 {
            let r = rng.below(100);
            let step = if r < 10 {
                0
            } else if r < 70 {
                1 + rng.below(40)
            } else if r < 95 {
                50 + rng.below(300)
            } else {
                500 + rng.below(1_000)
            };
            let up = rng.chance(50);
            let m = if up { model.mark + step } else { model.mark.saturating_sub(step) };
            let m = m.clamp(2_000, 50_000);
            let c = tick(&mut ctx, m);
            model.seq = current_ts(&mut ctx);
            model.mark = m;
            let (f, a, x) = model.sweep();
            assert_eq!(triggered(&c.logs), f, "{tag}: fires @ {m}");
            assert_eq!(activations(&c.logs), a, "{tag}: activations @ {m}");
            assert_eq!(spent_ids(&c.logs), x, "{tag}: expiries @ {m}");
            assert!(algo_cancels(&c.logs).iter().all(|x| x.1 != R_USER), "{tag}");
            cov.spent(&f, &x);
            let left = model.orders.iter().any(|o| o.active && spec_fires(o.sell, m, o.peak, o.cb));
            if f.len() == BUDGET && left {
                cov.fire_overflows += 1;
            }
            if a.len() == ACTIVATION_CAP && model.flag {
                cov.activation_overflows += 1;
            }
            cov.fires += f.len();
            cov.activations += a.len();
        } else {
            let (more, c) = drain(&mut ctx);
            if model.flag {
                let (f, a, x) = model.sweep();
                assert_eq!(triggered(&c.logs), f, "{tag}: drain fires");
                assert_eq!(activations(&c.logs), a, "{tag}: drain activations");
                assert_eq!(spent_ids(&c.logs), x, "{tag}: drain expiries");
                cov.spent(&f, &x);
                assert_eq!(more, model.flag, "{tag}: moreRemaining");
                if !f.is_empty() || !a.is_empty() {
                    cov.drains_with_work += 1;
                }
                cov.fires += f.len();
                cov.activations += a.len();
            } else {
                assert!(!more && c.logs.is_empty(), "{tag}: a drain with nothing deferred");
            }
        }
        assert_eq!(deferred(&mut ctx), model.flag, "{tag}: deferred-work flag");
        cov.max_live = cov.max_live.max(model.orders.len());
        let full = step % 25 == 0;
        check_views(&mut ctx, &model, if full { None } else { Some(&mut rng) });
        if full {
            for u in longs.iter().chain(shorts.iter()) {
                let want: Vec<Id> = {
                    let mut v: Vec<&MOrd> = model.orders.iter().filter(|o| o.owner == *u).collect();
                    v.sort_by_key(|o| o.placed);
                    v.into_iter().map(|o| o.id).collect()
                };
                assert_eq!(open_algos(&mut ctx, *u), want, "{tag}: open list of {u}");
            }
        }
    }
    check_views(&mut ctx, &model, None);
}

/// The differential run: every placement, tick and drain must produce EXACTLY the model's
/// `AlgoOrderActivated` and `AlgoOrderTriggered` sequences (ids, marks, reported stops, order),
/// the same deferred-work flag and `moreRemaining`, and every live order's view must equal the
/// model's peak / stop / activation price. `TRAIL_SPEC_SEEDS` raises the seed count.
#[test]
fn t_random_paths_match_an_independent_per_order_model() {
    let seeds: u64 = std::env::var("TRAIL_SPEC_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(16);
    let mut cov = Coverage::default();
    for s in 0..seeds {
        run_differential(0x5eed_0000 + s, 260, &mut cov);
    }
    // The run must actually have exercised what it claims to.
    assert!(cov.fires >= 200, "{cov:?}");
    assert!(cov.activations >= 200, "{cov:?}");
    assert!(cov.immediate >= 50, "{cov:?}");
    assert!(cov.fire_overflows >= 1, "{cov:?}");
    assert!(cov.activation_overflows >= 1, "{cov:?}");
    assert!(cov.drains_with_work >= 2, "{cov:?}");
    assert!(cov.cancels_active >= 20 && cov.cancels_pending >= 10, "{cov:?}");
    assert!(cov.long_trends >= 1, "{cov:?}");
    assert!(cov.reopened >= 5 && cov.spent_at_fire >= 5 && cov.spent_at_activation >= 2, "{cov:?}");
    eprintln!("trailing spec differential coverage: {cov:?}");
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Edges the structure depends on
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// The contrast case for decision 1: a plain reduce-only quantity stop is NOT position-bound
/// (Binance's ordinary GTC). Closed and reopened the same way round, it closes the NEW position;
/// after a flip it is on the opening side and is spent as `ReduceOnlyWouldOpen` (6), not 11.
#[test]
fn c2_a_plain_reduce_only_stop_is_not_position_bound() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10, 1);
    seed_pos(&mut ctx, ERIN, -10, 1);
    let ro = place_algo(&mut ctx, ALICE, &ro_stop_call(SELL, 9_500, 10)).0;
    rest(&mut ctx, BOB, BUY, MARK, 10);
    take(&mut ctx, ALICE, SELL, 10);
    rest(&mut ctx, CAROL, SELL, MARK, 10);
    take(&mut ctx, ALICE, BUY, 10);
    rest(&mut ctx, DAVE, BUY, 9_500, 20);
    let c = tick(&mut ctx, 9_500);
    assert_eq!(triggered(&c.logs), vec![(ro, 9_500, 9_500)]);
    assert_eq!(trades(&c.logs), vec![(ALICE, DAVE, 9_500, 10)]);
    assert_eq!(amount(&mut ctx, ALICE), 0);

    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10, 1);
    seed_pos(&mut ctx, ERIN, -10, 1);
    let ro = place_algo(&mut ctx, ALICE, &ro_stop_call(SELL, 9_500, 10)).0;
    rest(&mut ctx, BOB, BUY, MARK, 15);
    take(&mut ctx, ALICE, SELL, 15);
    let c = tick(&mut ctx, 9_500);
    assert_eq!(triggered(&c.logs), vec![(ro, 9_500, 9_500)]);
    assert_eq!(algo_cancels(&c.logs), vec![(ro, R_RO_WOULD_OPEN)]);
    assert_eq!(amount(&mut ctx, ALICE), -5);
}

/// Cancelling the ONLY order of a pending price level — the front level, and the last one —
/// must take the price out of the activation index: a later tick past it activates nothing for it
/// and the next level still activates exactly (FIFO). Both sides.
#[test]
fn t_cancelling_whole_pending_levels_keeps_the_activation_index_exact() {
    // (user, side, front level, next level, a tick between them, stop at the next level)
    // SELL: 10_500 / 10_600, ⌊10_600·0.99⌋ = 10_494;  BUY: 9_500 / 9_400, ⌈9_400·1.01⌉ = 9_494.
    for (user, side, front, next, between, stop) in [
        (ALICE, SELL, 10_500u64, 10_600u64, 10_550u64, 10_494u64),
        (CAROL, BUY, 9_500, 9_400, 9_450, 9_494),
    ] {
        let mut ctx = trail_fixture();
        let f = trail(&mut ctx, user, side, front, 100, 1);
        let n1 = trail(&mut ctx, user, side, next, 100, 1);
        let n2 = trail(&mut ctx, user, side, next, 100, 1);
        cancel_algo(&mut ctx, user, f);
        let c = tick(&mut ctx, between);
        assert_eq!(algo_event_count(&c.logs), 0, "side {side}");
        let c = tick(&mut ctx, next);
        assert_eq!(activations(&c.logs), vec![(n1, next, stop), (n2, next, stop)], "side {side}");

        // The last level emptied by cancels: a crossing tick is a no-op, and a new order works.
        let mut ctx = trail_fixture();
        let a = trail(&mut ctx, user, side, front, 100, 1);
        let b = trail(&mut ctx, user, side, next, 100, 1);
        cancel_algo(&mut ctx, user, b);
        cancel_algo(&mut ctx, user, a);
        let c = tick(&mut ctx, next);
        assert_eq!(algo_event_count(&c.logs), 0, "side {side}");
        let late = trail(&mut ctx, user, side, if side == SELL { next + 100 } else { next - 100 }, 100, 1);
        let c = tick(&mut ctx, if side == SELL { next + 100 } else { next - 100 });
        assert_eq!(ids_of(&activations(&c.logs)), vec![late], "side {side}");
    }
}

/// The activation cap splits across price levels in price order: 40 waiting at 10_100 and 40 at
/// 10_150, one tick to 10_200 → all 40 of the nearer level, then 24 of the next; the drain does
/// the last 16. Every one activates at the tick's mark (stop ⌊10_200·0.99⌋ = 10_098).
#[test]
fn t_the_activation_cap_splits_across_levels_in_price_order() {
    let mut ctx = fresh();
    let mut near = Vec::new();
    let mut far = Vec::new();
    for k in 0..4u16 {
        let u = new_user(&mut ctx, 400 + k);
        seed_pos(&mut ctx, u, 100, 1);
        for _ in 0..10 {
            far.push(trail(&mut ctx, u, SELL, 10_150, 100, 1));
        }
        for _ in 0..10 {
            near.push(trail(&mut ctx, u, SELL, 10_100, 100, 1));
        }
    }
    let c = tick(&mut ctx, 10_200);
    let mut want = near.clone();
    want.extend_from_slice(&far[..24]);
    assert_eq!(ids_of(&activations(&c.logs)), want);
    assert!(activations(&c.logs).iter().all(|a| (a.1, a.2) == (10_200, 10_098)));
    let (more, c) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(ids_of(&activations(&c.logs)), far[24..].to_vec());
}

/// When the OLDEST orders leave, the history below the oldest live one is garbage: a 400-entry
/// decline (order k activated at 10_000 − k, b 1000), the first 300 cancelled → one quiet tick
/// later only the stack pages holding entries 300..399 (pages 4, 5, 6 of 64) are left, and the 100
/// survivors are exact: peak 9_700 − j, then a partial new high (9_650 lifts only those below it),
/// then a spike and a fire of all 100 in activation order.
#[test]
fn t_the_oldest_orders_leaving_frees_the_low_history_and_the_rest_stay_exact() {
    let (mut h, longs, _) = h_setup(13, 0);
    h_tick(&mut h, 10_000);
    let mut ids = Vec::new();
    for k in 0..400usize {
        ids.push((longs[k / 32], h_trail(&mut h, longs[k / 32], SELL, 0, 1_000)));
        h_tick(&mut h, 10_000 - 1 - k as u64);
    }
    for (owner, id) in &ids[..300] {
        let input = cancelAlgoOrderCall { algoId: (*id).into(), marketId: MARKET_ID }.abi_encode();
        h_ok(&mut h, *owner, &input);
    }
    h_tick(&mut h, 9_600); // quiet: equal to the last mark
    h.inner.end_block();
    let mut pages: Vec<u64> = (0..8u64)
        .filter(|p| {
            let key = perp_core::keys::trail_stack_page_key(MARKET_ID, SELL, *p);
            h.inner.committed.contains_key(&key)
        })
        .collect();
    pages.sort();
    assert_eq!(pages, vec![4, 5, 6], "only the live history's pages remain");
    let survivors: Vec<Id> = ids[300..].iter().map(|x| x.1).collect();
    for (j, id) in survivors.iter().enumerate() {
        let peak = 9_700 - j as u64;
        assert_eq!(h_view(&mut h, *id), (true, peak, spec_thr(true, peak, 1_000)), "survivor {j}");
    }
    h_tick(&mut h, 9_650);
    for (j, id) in survivors.iter().enumerate() {
        let peak = (9_700 - j as u64).max(9_650);
        assert_eq!(h_view(&mut h, *id), (true, peak, spec_thr(true, peak, 1_000)), "survivor {j}");
    }
    h_tick(&mut h, 10_500);
    let l0 = h.inner.logs.len();
    h_tick(&mut h, 9_450);
    h_ok(&mut h, ERIN, &drainDeferredWorkCall { marketId: MARKET_ID }.abi_encode());
    let fired = triggered(&h.inner.logs[l0..]);
    assert_eq!(ids_of(&fired), survivors);
    assert!(fired.iter().all(|t| (t.1, t.2) == (9_450, 9_450)));
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Review findings on this series — regressions
// ═════════════════════════════════════════════════════════════════════════════════════════════

const E_CANCEL_CONDITIONAL: &str =
    "cancelOrder: order not cancellable (an untriggered conditional order — use cancelAlgoOrder)";

fn cancel_order_call(id: Id) -> Vec<u8> {
    cancelOrderCall { orderId: id.into(), marketId: MARKET_ID }.abi_encode()
}

fn bid_level_count(ctx: &mut TestCtx, price: u64) -> u64 {
    storage::load_bid_level_arc(ctx, MARKET_ID, price).unwrap().count
}

/// `cancelOrderSigned` for ALICE: the 102-byte message rebuilt from the ABI docs.
fn signed_cancel(ctx: &mut TestCtx, id: Id) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(b"perpdex_v1_cancel");
    msg.extend_from_slice(&ctx.chain_id().to_be_bytes());
    msg.extend_from_slice(ALICE.as_slice());
    msg.extend_from_slice(&id);
    msg.extend_from_slice(&MARKET_ID.to_be_bytes());
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.push(1); // keyId
    assert_eq!(msg.len(), 102);
    cancelOrderSignedCall {
        account: ALICE,
        orderId: id.into(),
        marketId: MARKET_ID,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 1,
        signature: key().sign(&msg).to_bytes().to_vec().into(),
    }
    .abi_encode()
}

/// `cancelOrder` refuses the id of an UNTRIGGERED conditional order, on all three of its
/// entrypoints, before any write. Such a record has status Open, so without the guard the
/// book-cancel path ran on it: it lowered the live count of the REAL level at (side, price) —
/// dropping the price from the index once it reached 0 — and only then failed on the missing
/// per-user entry, after those writes, which commit-only #23 keeps. The record survived, so
/// repeating the call let any account empty any level for one cancel per resting order.
///
/// BOB rests three bids of 1 at 9_900. ALICE, flat, holds a reduce-only limit STOP BUY at 9_900
/// (trigger 10_100); then, short 1, an activated and a pending trailing BUY (whose records carry
/// price 0, so the old path walked level 0). Every refusal is write-clean and log-clean and
/// leaves the 9_900 level counting 3; in a batch each is a genuine `NotCancellable` (3) item
/// next to an accepted one; the signed one is refused after its burn and is then a duplicate.
/// Afterwards `cancelAlgoOrder` still cancels the stop, and ERIN's market SELL 3 fills all of
/// BOB's bids.
#[test]
fn fix_cancel_order_refuses_an_untriggered_conditional_id() {
    let mut ctx = fresh();
    let bob: Vec<Id> = (0..3).map(|_| rest(&mut ctx, BOB, BUY, 9_900, 1)).collect();
    let stop = place_algo(&mut ctx, ALICE, &algo_call(STOP, BUY, 10_100, 9_900, 1, GTC, RO)).0;
    for _ in 0..3 {
        rejected(&mut ctx, ALICE, &cancel_order_call(stop), E_CANCEL_CONDITIONAL);
    }
    // The owner check still answers first, as on `cancelAlgoOrder`.
    rejected(&mut ctx, BOB, &cancel_order_call(stop), "cancelOrder: not owner");
    assert_eq!(bid_level_count(&mut ctx, 9_900), 3);
    assert_eq!(storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(), vec![9_900]);

    seed_pos(&mut ctx, ALICE, -1, 1);
    let act = trail(&mut ctx, ALICE, BUY, 0, 100, 1);
    let pend = trail(&mut ctx, ALICE, BUY, 9_500, 100, 1);
    for id in [act, pend] {
        rejected(&mut ctx, ALICE, &cancel_order_call(id), E_CANCEL_CONDITIONAL);
    }

    // batchCancelOrders: three genuine rejects beside one accepted cancel of a real order.
    let own = rest(&mut ctx, ALICE, BUY, 9_800, 1);
    let c = ok(
        &mut ctx,
        ALICE,
        &batchCancelOrdersCall { orderIds: vec![stop.into(), act.into(), pend.into(), own.into()] }
            .abi_encode(),
    );
    let blob = batchCancelOrdersCall::abi_decode_returns(&c.out.bytes).unwrap();
    let records: Vec<(u8, Id, u8)> =
        blob.chunks(34).map(|r| (r[0], r[1..33].try_into().unwrap(), r[33])).collect();
    assert_eq!(records, vec![(0, stop, 3), (0, act, 3), (0, pend, 3), (1, own, 0)]);
    assert_eq!(book_cancels(&c.logs), vec![(own, R_USER)]);
    assert!(algo_event_count(&c.logs) == 0);

    // cancelOrderSigned: refused after the burn (the only writes), then a duplicate.
    register_alice_key(&mut ctx);
    let input = signed_cancel(&mut ctx, stop);
    let _ = take_logs(&mut ctx);
    let before = world(&mut ctx);
    let w = ctx.perp_write_count();
    let out = run_perp_dex_call(&input, GAS, DAVE, U256::ZERO, false, &mut ctx).unwrap();
    assert_eq!(revert_reason(&out), E_CANCEL_CONDITIONAL);
    assert!(ctx.perp_write_count() > w, "the signature was burned");
    assert!(take_logs(&mut ctx).is_empty());
    assert!(world(&mut ctx) == before, "only the seen-signature set moved");
    rejected(&mut ctx, DAVE, &input, "cancelOrderSigned: duplicate signature (already submitted)");

    assert_eq!(bid_level_count(&mut ctx, 9_900), 3);
    assert_eq!(storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(), vec![9_900]);
    assert_eq!(open_algos(&mut ctx, ALICE), vec![stop, act, pend]);
    let c = cancel_algo(&mut ctx, ALICE, stop);
    assert_eq!(algo_cancels(&c.logs), vec![(stop, R_USER)]);
    let c = take(&mut ctx, ERIN, SELL, 3);
    assert_eq!(trades(&c.logs), bob.iter().map(|_| (ERIN, BOB, 9_900, 1)).collect::<Vec<_>>());
    assert!(storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
}

/// A relayed signed submission refused by `order would immediately trigger`, the burn's measured
/// cost, then `duplicate signature`.
fn refused_after_the_burn(ctx: &mut TestCtx, input: &[u8]) {
    let _ = take_logs(ctx);
    let before = world(ctx);
    let w = ctx.perp_write_count();
    let out = run_perp_dex_call(input, GAS, DAVE, U256::ZERO, false, ctx).unwrap();
    assert_eq!(revert_reason(&out), E_IMMEDIATE);
    assert!(ctx.perp_write_count() > w, "the signature was burned");
    assert!(take_logs(ctx).is_empty());
    assert!(world(ctx) == before, "only the seen-signature set moved");
    rejected(ctx, DAVE, input, E_DUP_SIG);
}

/// "Would immediately trigger" reads the stored mark, which moves every tick, so the refusal is
/// TRANSIENT and must spend a signed submission (the rule `placeOrderSigned` states: a refused
/// signature left live lets an observer land it later, at a moment its owner believed it had
/// failed). Before the fix it was checked before the burn: refused with 0 writes, then accepted
/// from the same calldata once the mark moved back.
///
/// ALICE (long 10) signs, at mark 10_000, a SELL trailing stop activating at 10_100 and a SELL
/// STOP_MARKET at 9_950 — both fine at that mark. The first is relayed after a tick to 10_150
/// (activation already reached), the second after a tick to 9_950 (trigger already satisfied):
/// each is refused after its burn, and with the mark back at 10_050 / 10_000 the same calldata
/// is a duplicate and nothing was placed.
#[test]
fn fix_a_mark_dependent_signed_refusal_burns_the_signature() {
    let mut ctx = trail_fixture();
    register_alice_key(&mut ctx);
    let (trail_in, _) = signed_trail(&mut ctx, SELL, 10_100, 100, 1, 1);
    let (stop_in, _) = signed_algo(&mut ctx, STOP_MARKET, SELL, 9_950, 0, 1, RO, 2);

    tick(&mut ctx, 10_150);
    refused_after_the_burn(&mut ctx, &trail_in);
    tick(&mut ctx, 10_050);
    rejected(&mut ctx, DAVE, &trail_in, E_DUP_SIG);

    tick(&mut ctx, 9_950);
    refused_after_the_burn(&mut ctx, &stop_in);
    tick(&mut ctx, 10_000);
    rejected(&mut ctx, DAVE, &stop_in, E_DUP_SIG);
    assert!(open_algos(&mut ctx, ALICE).is_empty());

    // A static refusal still costs nothing and gives the same answer again (the callback grid).
    let (bad, _) = signed_trail(&mut ctx, SELL, 10_100, 15, 1, 3);
    let reason = format!("placeAlgoOrderSigned: {E_CALLBACK}");
    rejected(&mut ctx, DAVE, &bad, &reason);
    rejected(&mut ctx, DAVE, &bad, &reason);
}

fn update_market_active(ctx: &mut TestCtx, active: bool) {
    use crate::interface::IPerpDex::updateMarketCall;
    ok(
        ctx,
        ADMIN,
        &updateMarketCall {
            marketId: MARKET_ID,
            tickSize: 1,
            stepSize: 1,
            minQuantity: 1,
            maxQuantity: 1_000_000,
            maxPrice: 1_000_000,
            priceUpdateInterval: INTERVAL,
            active,
            fundingInterval: 0,
            interestRate: 0,
            liquidationFeeRateBps: 0,
            priceBandBps: 0,
        }
        .abi_encode(),
    );
}

/// A DEACTIVATED market converts no conditional order. The oracle sweep used to run its trigger
/// legs whatever `Market.active` said: in a debug build the first crossing panicked on `fire`'s
/// `debug_assert!(market.active)`; in a release build `place_order_core` refused the fired order
/// ("market not active") and `fire` SPENT it as `TriggerRejected` (10) — a pause destroyed every
/// stop the next ticks crossed, though nothing traded. (Kept `#[ignore]`d as
/// `bug_an_oracle_tick_on_a_deactivated_market_does_not_panic` until this fix.)
///
/// ALICE (long 10) holds a reduce-only STOP_MARKET SELL 1 at 9_900, a SELL trailer activated now
/// (b 100) and a pending one activating at 10_100; BOB bids 2 @ 9_900. Paused, the ticks go to
/// 10_200 and then 9_900: that crosses the fixed stop, the activated trailer's stop (H 10_200 →
/// ⌊10_098⌋) and the pending one's activation — and nothing fires, activates or is cancelled, no
/// deferred work is flagged, and the activated trailer still trails (peak 10_200). Reactivated, the
/// next tick at 9_900 fires the fixed stop and then the trailer into BOB's bid; the pending one is
/// re-tested at 9_900 (below 10_100) and keeps waiting.
#[test]
fn fix_a_deactivated_market_fires_and_activates_nothing_until_reactivated() {
    let mut ctx = trail_fixture();
    let stop = place_algo(&mut ctx, ALICE, &ro_stop_call(SELL, 9_900, 1)).0;
    let act = trail(&mut ctx, ALICE, SELL, 0, 100, 1);
    let pend = trail(&mut ctx, ALICE, SELL, 10_100, 100, 1);
    rest(&mut ctx, BOB, BUY, 9_900, 2);
    update_market_active(&mut ctx, false);

    for mark in [10_200, 9_900] {
        let c = tick(&mut ctx, mark);
        assert_eq!(algo_event_count(&c.logs), 0, "paused at {mark}");
        assert!(placed(&c.logs).is_empty() && trades(&c.logs).is_empty());
        assert!(!deferred(&mut ctx), "a skipped leg is not deferred work");
    }
    let (more, c) = drain(&mut ctx);
    assert!(!more && algo_event_count(&c.logs) == 0);
    assert_eq!(open_algos(&mut ctx, ALICE), vec![stop, act, pend]);
    assert_eq!(trail_state(&mut ctx, act), (true, 10_200, 10_098), "the peak follows the mark");
    assert_eq!(trail_state(&mut ctx, pend), (false, 0, 0));
    assert_eq!(amount(&mut ctx, ALICE), 10);

    update_market_active(&mut ctx, true);
    let c = tick(&mut ctx, 9_900);
    assert_eq!(triggered(&c.logs), vec![(stop, 9_900, 9_900), (act, 10_098, 9_900)]);
    assert!(algo_cancels(&c.logs).is_empty() && activations(&c.logs).is_empty());
    assert_eq!(trades(&c.logs), vec![(ALICE, BOB, 9_900, 1), (ALICE, BOB, 9_900, 1)]);
    assert_eq!(amount(&mut ctx, ALICE), 8);
    assert_eq!(open_algos(&mut ctx, ALICE), vec![pend]);
    assert_eq!(trail_state(&mut ctx, pend), (false, 0, 0));
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Mutation survivors — each test names the mutant of this series it kills
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// Kills T19 — "activation leg runs BEFORE the fire leg". The spec fixes the per-tick order
/// (… fixed triggers → trailing fires → activations → liquidations), and L4 makes it irrelevant
/// for whether a NEW activation fires — but NOT for the lazy expiry at activation: a fire in the
/// same tick can END the position a pending trailer is bound to (here as the maker of the fired
/// order), and only fires-then-activations sees that.
///
/// ALICE long 10 holds an activated SELL trailer (H 10_000, b 100 → stop 9_900); CAROL short 10
/// holds a PENDING BUY trailer (activation 9_950) and the BUY 10 @ 9_900 that ALICE's fire hits.
/// Tick 9_900: ALICE's trailer fires and closes CAROL's short; CAROL's trailer is then reached —
/// and spent as 11 at activation, never activated (no `AlgoOrderActivated`, gone from her list).
/// With the legs swapped it activates first (bound to a live generation) and lingers, activated,
/// on a flat account.
///
/// Second half: with unrelated orders the stream carries the fire leg's `AlgoOrderTriggered`
/// BEFORE the activation leg's `AlgoOrderActivated` (DAVE's BUY activates at 9_900, stop
/// ⌈9_900·1.01⌉ = 9_999).
#[test]
fn kill_t19_a_fire_that_ends_a_position_runs_before_that_positions_activation() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10, 1);
    seed_pos(&mut ctx, CAROL, -10, 1);
    let a = trail(&mut ctx, ALICE, SELL, 0, 100, 10);
    let c_pend = trail(&mut ctx, CAROL, BUY, 9_950, 100, 10);
    rest(&mut ctx, CAROL, BUY, 9_900, 10);
    let c = tick(&mut ctx, 9_900);
    assert_eq!(triggered(&c.logs), vec![(a, 9_900, 9_900)]);
    assert_eq!(trades(&c.logs), vec![(ALICE, CAROL, 9_900, 10)]);
    assert_eq!((amount(&mut ctx, ALICE), amount(&mut ctx, CAROL)), (0, 0));
    assert!(activations(&c.logs).is_empty(), "CAROL's short ended in the fire leg");
    assert_eq!(algo_cancels(&c.logs), vec![(c_pend, R_POSITION_CLOSED)]);
    algo_gone(&mut ctx, c_pend);
    assert!(open_algos(&mut ctx, CAROL).is_empty());

    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 10, 1);
    seed_pos(&mut ctx, DAVE, -10, 1);
    let a = trail(&mut ctx, ALICE, SELL, 0, 100, 10);
    let d = trail(&mut ctx, DAVE, BUY, 9_950, 100, 10);
    rest(&mut ctx, BOB, BUY, 9_900, 10);
    let c = tick(&mut ctx, 9_900);
    assert_eq!(triggered(&c.logs), vec![(a, 9_900, 9_900)]);
    assert_eq!(activations(&c.logs), vec![(d, 9_900, 9_999)]);
    let at = |sig: B256| c.logs.iter().position(|l| l.data.topics().first() == Some(&sig)).unwrap();
    assert!(
        at(AlgoOrderTriggered::SIGNATURE_HASH) < at(AlgoOrderActivated::SIGNATURE_HASH),
        "fire leg before activation leg"
    );
}

/// Kills C2j — "generation not bumped inside a liquidation close". The generation must move on
/// EVERY path that takes a position to zero, including a MAKER whose resting order is hit by a
/// LIQUIDATION's close order (the liquidation runs its own market order through `match_order`,
/// not `placeOrder`). DAVE is short 3 with a closePosition BUY stop and an activated trailing BUY,
/// and rests the BUY 3 that ALICE's liquidation sells into: DAVE goes flat as the liquidation's
/// maker. He then reopens a short 3 the same way round. Both bound orders belong to the ENDED
/// short and must be spent with 11, never close the new one.
///
/// Numbers: ALICE long 3 at 5x (margin $60) is under water at 9_100 (60 − 3·9 = 33 < 3·91/6 =
/// 45.5) and her liquidation sells 3 into DAVE's bid. The trailer's low is 9_100 after that tick,
/// so its stop is ⌈9_100·1.01⌉ = 9_191 and the next tick (10_000) fires it. The closePosition
/// stop (BUY STOP_MARKET 10_500) fires on the tick to 10_500, with BOB's ask there for a wrong
/// close to hit.
#[test]
fn kill_c2j_a_maker_closed_by_a_liquidation_ends_its_position_for_bound_orders() {
    let mut ctx = fresh();
    seed_pos(&mut ctx, ALICE, 3, 5);
    seed_pos(&mut ctx, DAVE, -3, 1);
    let cp = place_algo(&mut ctx, DAVE, &cp_stop_call(BUY, 10_500)).0;
    let tr = trail(&mut ctx, DAVE, BUY, 0, 100, 3);
    rest(&mut ctx, DAVE, BUY, 9_100, 3);

    // ALICE is liquidated into DAVE's bid: DAVE is flat, and his bound orders are untouched.
    let c = tick(&mut ctx, 9_100);
    let liq = evs::<Liquidation>(&c.logs);
    assert_eq!(liq.len(), 1);
    assert_eq!(liq[0].user, ALICE);
    assert_eq!(trades(&c.logs).len(), 1);
    assert_eq!((trades(&c.logs)[0].1, trades(&c.logs)[0].3), (DAVE, 3), "DAVE is the maker");
    assert_eq!(amount(&mut ctx, DAVE), 0);
    assert!(algo_cancels(&c.logs).is_empty() && triggered(&c.logs).is_empty());
    assert_eq!(open_algos(&mut ctx, DAVE), vec![cp, tr]);

    // A NEW short 3, same way round.
    rest(&mut ctx, ERIN, BUY, 9_100, 3);
    take(&mut ctx, DAVE, SELL, 3);
    assert_eq!(amount(&mut ctx, DAVE), -3);

    // The trailer (low 9_100 → stop 9_191) fires at 10_000 — spent as 11, nothing placed.
    let c = tick(&mut ctx, 10_000);
    assert_eq!(triggered(&c.logs), vec![(tr, 9_191, 10_000)]);
    assert_eq!(algo_cancels(&c.logs), vec![(tr, R_POSITION_CLOSED)]);
    assert!(placed(&c.logs).is_empty(), "nothing placed for an order of the ended short");
    assert_eq!(amount(&mut ctx, DAVE), -3);

    // The closePosition stop fires at 10_500, liquidity right there — spent as 11 too.
    rest(&mut ctx, BOB, SELL, 10_500, 10);
    let c = tick(&mut ctx, 10_500);
    assert_eq!(triggered(&c.logs), vec![(cp, 10_500, 10_500)]);
    assert_eq!(algo_cancels(&c.logs), vec![(cp, R_POSITION_CLOSED)]);
    assert!(placed(&c.logs).is_empty() && trades(&c.logs).is_empty());
    assert_eq!(amount(&mut ctx, DAVE), -3, "the new short is untouched");
    assert!(open_algos(&mut ctx, DAVE).is_empty());
}

/// Kills T34 — "trailing fire deferral not propagated to `deferred_work`". In every bracket-style
/// test the liquidation sweep's CONSERVATIVE deferral (budget 0 with an open position left in the
/// registry) masks it. Here nothing is open at all: 60 activated trailers outlived their positions
/// (both owners closed against ERIN, who is flat too), so the tick spends its 50 actions expiring
/// 50 of them (`AlgoOrderTriggered` + 11) with an EMPTY position registry — the trailing leg is the
/// only one that can raise the flag, and the drain must find the other 10.
#[test]
fn kill_t34_a_trailing_overflow_alone_raises_the_deferred_flag() {
    let mut ctx = fresh();
    let ids = crowd(&mut ctx, 60, 0);
    seed_pos(&mut ctx, ERIN, -200, 1);
    for k in 0..2u16 {
        let u = extra(k);
        rest(&mut ctx, u, SELL, MARK, 100);
        take(&mut ctx, ERIN, BUY, 100);
        assert_eq!(amount(&mut ctx, u), 0);
    }
    assert_eq!(amount(&mut ctx, ERIN), 0);
    assert!(storage::load_position_registry(&mut ctx, MARKET_ID).unwrap().is_empty());

    let c = tick(&mut ctx, 9_900);
    let want: Vec<(Id, u64, u64)> = ids.iter().map(|id| (*id, 9_900, 9_900)).collect();
    assert_eq!(triggered(&c.logs), want[..BUDGET].to_vec());
    assert_eq!(
        algo_cancels(&c.logs),
        ids[..BUDGET].iter().map(|id| (*id, R_POSITION_CLOSED)).collect::<Vec<_>>()
    );
    assert!(deferred(&mut ctx), "10 due trailers left on the budget: deferred work");
    let (more, c) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(triggered(&c.logs), want[BUDGET..].to_vec());
    assert!(!deferred(&mut ctx));
}
