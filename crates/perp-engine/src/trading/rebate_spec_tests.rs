//! **Maker rebates and ppm trading-fee rates, written from the spec (`SPEC_maker_rebate.md`, §1–§3
//! and the §7 test list) by an independent writer — NOT from the implementation's own tests**
//! (`trading::fee_spec_tests`, the `perp-core` fee-unit tests).
//!
//! Every call goes through the full shell (`run_perp_dex_call`), so the gas table, the dispatch,
//! the commit-only #23 tripwire (a `debug_assert` on any write-then-revert), the debug event guards
//! and the end-of-call `AccountBalanceChanged` drain are all in the loop.
//!
//! ## What every successful call is checked against ([`ok`])
//!
//! Four spec identities are asserted on EVERY successful call in this file, whatever it is
//! (a placement, a batch, an oracle tick that fires a conditional, a liquidation, a rate change):
//!
//! * **conservation** (§7) — `Σ (perp wallet + position margin + vQuote)` over every watched account
//!   plus the insurance fund is EXACTLY unchanged. A fill moves `calc_value` from one side's vQuote
//!   to the other's and fees/rebates only move cash between the parties and the fee recipient, so
//!   nothing may be minted or burned — not even one unit of rounding;
//! * **fee-recipient identity** (§2) — the admin's wallet delta equals the `market_fee_total` delta,
//!   which is `≥ 0` (the admin is never debited; the total stays monotone);
//! * **the events add up** (§2, §4) — that delta also equals `Σ (Trade.takerFee + Trade.makerFee)`
//!   over the call's `Trade`s (`makerFee` signed: `+` a commission, `−` a rebate);
//! * **the cap** (§2) — no `Trade` reports a rebate larger than its own `takerFee` share.
//!
//! On top of those, each test pins EXACT numbers computed by hand from the spec formulas and
//! written out in the comments next to them:
//!
//! * `n_i = calc_value(P_i, q_i)` — on the base-0 / price-2 market here `n = P·q·10^4`;
//! * taker fee `F = floor(Σ n_i·t / 1e6)`, charged once per match;
//! * taker share `share_i = floor(C_i·t/1e6) − floor(C_{i−1}·t/1e6)`, `C_i = Σ_{j≤i} n_j`;
//! * a positive maker rate: the whole-order difference `fee(A) − fee(A − q)` (unchanged);
//! * a negative maker rate: `rebate_i = min(floor(n_i·|m|/1e6), share_i)`, to the perp WALLET.
//!
//! Rates sit off the 100-ppm grid on purpose (`t = 333`, `|m| = 250`): on this market a whole-bps
//! rate never truncates (`n·b/1e4 = P·q·b`), so only sub-bp rates make the floors bite — and the
//! prices are chosen so the telescoped share and the independent per-fill floor DIFFER on the
//! fills that matter.
//!
//! Exact reject reasons come from the implementation's report (the spec fixes the rules, not the
//! words). Spec items are cited inline (`§1`…`§7`).

use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::call::PerpOutput;
use crate::host::PerpHost;
use crate::interface::IPerpDex::{
    asSubAccountCall, batchPlaceOrdersCall, createSubAccountCall, getMarketFeeTotalCall,
    getMaxMakerRebateCall, getUserFeeRatesCall, liquidateCall, placeAlgoOrderCall,
    placeOrderCall, setMaxMakerRebateCall, setUserFeeRatesCall, updateIndexPriceCall,
    AlgoOrderTriggered, Liquidation, MaxMakerRebateUpdated, OrderCancelled, PlaceItem, Trade,
    UserFeeRatesUpdated,
};
use crate::types::{AccountUpdateReason, MarginTiers, Market, PerpPosition};
use crate::{run_perp_dex_call, storage, PERP_DEX_ADDRESS, USDC_ADDRESS};

// ── Fixture ──────────────────────────────────────────────────────────────────────────────────

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;
type Log = primitives::Log;

/// The taker in almost every scenario.
const ALICE: Address = address!("1111111111111111111111111111111111111111");
/// Makers.
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
const DAVE: Address = address!("4444444444444444444444444444444444444444");
/// A bystander: liquidator, or the holder of an offsetting seeded position.
const ERIN: Address = address!("5555555555555555555555555555555555555555");
/// The admin, which is also the fee recipient and the oracle.
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const MARKET_ID: u64 = 1;
/// $100.00 at price_decimals 2.
const MARK: u64 = 10_000;
/// One unit at $100 in quote micro-units on the base-0 market.
const UNIT_VALUE: i64 = 100_000_000;
/// $1e9 — margin never interferes unless a test plants the wallet itself.
const WALLET: i64 = 1_000_000_000_000_000;
const GAS: u64 = 30_000_000;

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MKT: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;
const FOK: u8 = 2;
/// `flags` bit 0 (also the conditional's reduce-only bit).
const RO: u8 = 0b0001;
/// AlgoOrderType.
const STOP_MARKET: u8 = 1;

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, CAROL, DAVE, ERIN, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

/// Market 1 at a $100 mark (`base_decimals` / price 2 decimals, tick 1, step 1, min 1, no
/// funding, no clearance fee, the default ±10% fill band), the admin set, and a $1e9 perp wallet
/// for ALICE, BOB, CAROL, DAVE and ERIN. The admin's wallet starts at 0. No fee rates, R = 0.
fn market_with(base_decimals: u32) -> TestCtx {
    EXTRA_WATCHED.with(|e| e.borrow_mut().clear());
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(
        &mut ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals,
            price_decimals: 2,
            tick_size: 1,
            step_size: 1,
            min_quantity: 1,
            max_quantity: 1_000_000,
            max_price: 1_000_000,
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
    for u in [ALICE, BOB, CAROL, DAVE, ERIN] {
        set_wallet(&mut ctx, u, WALLET);
    }
    let _ = take_logs(&mut ctx);
    ctx
}

fn fresh() -> TestCtx {
    market_with(0)
}

/// Plants `perp` as `user`'s perp wallet, keeping every other account field (rates, master, …).
fn set_wallet(ctx: &mut TestCtx, user: Address, perp: i64) {
    let mut a = storage::load_account(ctx, user).unwrap();
    a.perp_wallet_balance = perp;
    storage::save_account(ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
    let _ = take_logs(ctx);
}

/// Seeds `user` with `amount` contracts entered at $100 OFF-BOOK at `leverage`
/// (`margin = |amount|·$100 / leverage`).
fn seed_pos(ctx: &mut TestCtx, user: Address, amount: i64, leverage: u64) {
    storage::save_position(
        ctx,
        user,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance: -amount * UNIT_VALUE,
            margin: amount.abs() * UNIT_VALUE / leverage as i64,
            leverage,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let _ = take_logs(ctx);
}

thread_local! {
    /// Extra addresses (sub-accounts) a test wants in the conservation / world snapshots.
    static EXTRA_WATCHED: std::cell::RefCell<Vec<Address>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn watch(addr: Address) {
    EXTRA_WATCHED.with(|e| e.borrow_mut().push(addr));
}

fn watched() -> Vec<Address> {
    let mut users = vec![ALICE, BOB, CAROL, DAVE, ERIN, ADMIN];
    EXTRA_WATCHED.with(|e| users.extend(e.borrow().iter().copied()));
    users
}

// ── Reject reasons (§1; wording from the implementation's report) ─────────────────────────────

const E_NOT_ADMIN: &str = "not authorised: caller is not admin";
const E_TAKER_MAX: &str = "setUserFeeRates: taker fee exceeds 100000 ppm (10%)";
const E_MAKER_MAX: &str = "setUserFeeRates: maker fee exceeds 100000 ppm (10%)";
const E_SUB: &str = "setUserFeeRates: user is a sub-account; set its master's rates";
const E_R_CEIL: &str = "setMaxMakerRebate: exceeds 10000 ppm (1%)";
const E_MARGIN: &str = "placeOrder: insufficient perp wallet for margin";

fn e_rebate(r: u64) -> String {
    format!("setUserFeeRates: maker rebate exceeds maxMakerRebate ({r} ppm)")
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

/// One account's money and exposure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row {
    wallet: i64,
    margin: i64,
    vq: i64,
    amount: i64,
}

#[derive(Clone, Debug)]
struct Snap {
    rows: Vec<(Address, Row)>,
    fee_total: u64,
    insurance: u64,
}

impl Snap {
    fn row(&self, u: Address) -> Row {
        self.rows.iter().find(|(a, _)| *a == u).map(|(_, r)| *r).expect("watched")
    }
    /// `Σ (wallet + margin + vQuote) + insurance fund` — the §7 conservation quantity.
    fn value(&self) -> i128 {
        self.rows
            .iter()
            .map(|(_, r)| r.wallet as i128 + r.margin as i128 + r.vq as i128)
            .sum::<i128>()
            + self.insurance as i128
    }
}

fn snap(ctx: &mut TestCtx) -> Snap {
    let rows = watched()
        .into_iter()
        .map(|u| {
            let a = storage::load_account(ctx, u).unwrap();
            let p = storage::load_position(ctx, u, MARKET_ID).unwrap();
            (
                u,
                Row {
                    wallet: a.perp_wallet_balance,
                    margin: p.margin,
                    vq: p.v_quote_balance,
                    amount: p.amount,
                },
            )
        })
        .collect();
    Snap {
        rows,
        fee_total: storage::load_market_fee_total(ctx, MARKET_ID).unwrap(),
        insurance: storage::load_insurance_fund(ctx).unwrap(),
    }
}

/// One `Trade`, as the spec reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fill {
    taker: Address,
    maker: Address,
    price: u64,
    qty: u64,
    taker_fee: u64,
    maker_fee: i64,
}

fn fills(logs: &[Log]) -> Vec<Fill> {
    evs::<Trade>(logs)
        .into_iter()
        .map(|e| Fill {
            taker: e.taker,
            maker: e.maker,
            price: e.price,
            qty: e.quantity,
            taker_fee: e.takerFee,
            maker_fee: e.makerFee,
        })
        .collect()
}

/// `(maker, price, qty, takerFee, makerFee)` — the per-fill fee facts a test pins.
fn fee_rows(logs: &[Log]) -> Vec<(Address, u64, u64, u64, i64)> {
    fills(logs)
        .into_iter()
        .map(|f| (f.maker, f.price, f.qty, f.taker_fee, f.maker_fee))
        .collect()
}

fn evs<E: SolEvent>(logs: &[Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

/// A successful call with the snapshots either side of it.
struct Call {
    out: PerpOutput,
    logs: Vec<Log>,
    before: Snap,
    after: Snap,
}

impl Call {
    /// Δ(perp wallet + position margin) — what a party PAID (−) or RECEIVED (+) in fees when the
    /// fill opens or closes at its own entry price.
    fn d_wm(&self, u: Address) -> i64 {
        let (b, a) = (self.before.row(u), self.after.row(u));
        (a.wallet + a.margin) - (b.wallet + b.margin)
    }
    fn d_wallet(&self, u: Address) -> i64 {
        self.after.row(u).wallet - self.before.row(u).wallet
    }
    fn d_fee_total(&self) -> i64 {
        self.after.fee_total as i64 - self.before.fee_total as i64
    }
    fn fee_rows(&self) -> Vec<(Address, u64, u64, u64, i64)> {
        fee_rows(&self.logs)
    }
    fn fills(&self) -> Vec<Fill> {
        fills(&self.logs)
    }
}

/// One SUCCESSFUL shell call, checked against the four call-wide identities (module doc).
fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Call {
    let _ = take_logs(ctx);
    let before = snap(ctx);
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(!out.reverted, "unexpected revert: {}", revert_reason(&out));
    let logs = take_logs(ctx);
    let after = snap(ctx);
    let call = Call {
        out,
        logs,
        before,
        after,
    };
    // §7 conservation: not one unit minted or burned.
    assert_eq!(
        call.after.value(),
        call.before.value(),
        "value not conserved (Σ wallet + margin + vQuote + IF)"
    );
    // §2: the fee recipient is only ever credited, and by exactly the fee-total rise.
    assert!(call.after.fee_total >= call.before.fee_total, "market_fee_total fell");
    assert_eq!(
        call.d_wallet(ADMIN),
        call.d_fee_total(),
        "admin wallet delta != market_fee_total delta"
    );
    // §2/§4: …which is Σ (takerFee + makerFee) over the call's Trades.
    let fills = call.fills();
    let event_sum: i64 = fills.iter().map(|f| f.taker_fee as i64 + f.maker_fee).sum();
    assert_eq!(event_sum, call.d_fee_total(), "Σ(Trade.takerFee + Trade.makerFee) != fee delta");
    // §2 the per-fill cap, as the events report it.
    for f in &fills {
        assert!(
            f.maker_fee >= 0 || f.maker_fee.unsigned_abs() <= f.taker_fee,
            "a rebate exceeded its own fill's taker share: {f:?}"
        );
    }
    call
}

fn view(ctx: &mut TestCtx, input: &[u8]) -> PerpOutput {
    let out = run_perp_dex_call(input, GAS, ERIN, U256::ZERO, true, ctx).unwrap();
    assert!(!out.reverted, "view reverted: {}", revert_reason(&out));
    out
}

/// Everything a rejected call must leave bit-identical: every watched account / position / order
/// list (encoded), the fee total, the insurance fund, and R.
fn world(ctx: &mut TestCtx) -> Vec<Vec<u8>> {
    let mut w = Vec::new();
    for u in watched() {
        w.push(enc(&storage::load_account(ctx, u).unwrap()));
        w.push(enc(&storage::load_position(ctx, u, MARKET_ID).unwrap()));
        w.push(enc(&storage::load_buy_orders(ctx, u, MARKET_ID).unwrap()));
        w.push(enc(&storage::load_sell_orders(ctx, u, MARKET_ID).unwrap()));
    }
    w.push(enc(&storage::load_bid_prices(ctx, MARKET_ID).unwrap()));
    w.push(enc(&storage::load_ask_prices(ctx, MARKET_ID).unwrap()));
    w.push(enc(&storage::load_market_fee_total(ctx, MARKET_ID).unwrap()));
    w.push(enc(&storage::load_insurance_fund(ctx).unwrap()));
    w.push(enc(&storage::load_max_maker_rebate(ctx).unwrap()));
    w
}

fn enc<T: serde::Serialize>(v: &T) -> Vec<u8> {
    perp_core::codec::encode(v).unwrap()
}

/// A REJECTED shell call: the exact reason, no perp write, no log, the world bit-identical.
fn rejected(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
    let _ = take_logs(ctx);
    let before = world(ctx);
    let writes = ctx.perp_write_count();
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(out.reverted, "expected the revert `{expect}`, but the call SUCCEEDED");
    assert_eq!(revert_reason(&out), expect);
    assert_eq!(ctx.perp_write_count(), writes, "[{expect}] the rejected call wrote perp state");
    assert!(take_logs(ctx).is_empty(), "[{expect}] the rejected call logged");
    assert!(world(ctx) == before, "[{expect}] the rejected call changed stored state");
}

// ── Calldata ─────────────────────────────────────────────────────────────────────────────────

fn place(side: u8, price: u64, qty: u64, order_type: u8, tif: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

fn limit(side: u8, price: u64, qty: u64, tif: u8) -> Vec<u8> {
    place(side, price, qty, LIMIT, tif, 0)
}

fn market(side: u8, qty: u64) -> Vec<u8> {
    place(side, 0, qty, MKT, GTC, 0)
}

fn fees_call(user: Address, maker_ppm: i64, taker_ppm: u64) -> Vec<u8> {
    setUserFeeRatesCall {
        user,
        makerFeePpm: maker_ppm,
        takerFeePpm: taker_ppm,
    }
    .abi_encode()
}

fn r_call(ppm: u64) -> Vec<u8> {
    setMaxMakerRebateCall {
        maxMakerRebatePpm: ppm,
    }
    .abi_encode()
}

fn as_sub(sub: Address, inner: Vec<u8>) -> Vec<u8> {
    asSubAccountCall {
        subAccount: sub,
        data: inner.into(),
    }
    .abi_encode()
}

fn item(side: u8, price: u64, qty: u64, tif: u8) -> PlaceItem {
    PlaceItem {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: LIMIT,
        tif,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
}

/// A STOP_MARKET conditional, reduce-only.
fn stop_market(side: u8, trigger: u64, qty: u64) -> Vec<u8> {
    placeAlgoOrderCall {
        algoType: 0,
        marketId: MARKET_ID,
        side,
        orderType: STOP_MARKET,
        triggerPrice: trigger,
        price: 0,
        quantity: qty,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags: RO,
    }
    .abi_encode()
}

// ── Shortcuts ────────────────────────────────────────────────────────────────────────────────

fn set_fees(ctx: &mut TestCtx, user: Address, maker_ppm: i64, taker_ppm: u64) {
    ok(ctx, ADMIN, &fees_call(user, maker_ppm, taker_ppm));
}

fn set_r(ctx: &mut TestCtx, ppm: u64) {
    ok(ctx, ADMIN, &r_call(ppm));
}

fn get_r(ctx: &mut TestCtx) -> u64 {
    let out = view(ctx, &getMaxMakerRebateCall {}.abi_encode());
    getMaxMakerRebateCall::abi_decode_returns(&out.bytes).unwrap()
}

fn get_rates(ctx: &mut TestCtx, user: Address) -> (i64, u64) {
    let out = view(ctx, &getUserFeeRatesCall { user }.abi_encode());
    let r = getUserFeeRatesCall::abi_decode_returns(&out.bytes).unwrap();
    (r.makerFeePpm, r.takerFeePpm)
}

fn get_fee_total(ctx: &mut TestCtx) -> u64 {
    let out = view(ctx, &getMarketFeeTotalCall { marketId: MARKET_ID }.abi_encode());
    getMarketFeeTotalCall::abi_decode_returns(&out.bytes).unwrap()
}

/// A resting order that crossed nothing.
fn rest(ctx: &mut TestCtx, user: Address, side: u8, price: u64, qty: u64) {
    let c = ok(ctx, user, &limit(side, price, qty, GTC));
    assert!(c.fills().is_empty(), "{user} {side}@{price} traded while resting");
}

fn row(ctx: &mut TestCtx, u: Address) -> Row {
    snap(ctx).row(u)
}

/// The resting entries' snapshotted maker rates for `user` on `side`.
fn snapshots(ctx: &mut TestCtx, user: Address, side: u8) -> Vec<i64> {
    let l = if side == BUY {
        storage::load_buy_orders(ctx, user, MARKET_ID).unwrap()
    } else {
        storage::load_sell_orders(ctx, user, MARKET_ID).unwrap()
    };
    l.iter().map(|e| e.maker_fee_ppm).collect()
}

/// The oracle tick through the shell at `ts = 31`. It must SUCCEED.
fn tick(ctx: &mut TestCtx, index: u64) -> Call {
    ok(
        ctx,
        ADMIN,
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: index,
            timestamp: 31,
        }
        .abi_encode(),
    )
}

/// A walk direction, named by the TAKER's side.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Dir {
    Buy,
    Sell,
}

const DIRS: [Dir; 2] = [Dir::Buy, Dir::Sell];

impl Dir {
    fn taker(self) -> u8 {
        match self {
            Dir::Buy => BUY,
            Dir::Sell => SELL,
        }
    }
    fn maker(self) -> u8 {
        1 - self.taker()
    }
    /// The taker's signed position change per unit filled.
    fn sign(self) -> i64 {
        match self {
            Dir::Buy => 1,
            Dir::Sell => -1,
        }
    }
    /// `(buy-taker value, sell-taker value)`.
    fn pick<T>(self, buy: T, sell: T) -> T {
        match self {
            Dir::Buy => buy,
            Dir::Sell => sell,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// §1 / §7 "rates": units, bounds, R
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/// §1: `0 ≤ taker ≤ 100_000`, `−R ≤ maker ≤ 100_000`, R defaults to 0 so EVERY negative maker rate
/// is refused (even −1, even i64::MIN), the ceilings themselves are accepted, the setter is admin
/// only, and an unset user reads `(0, 0)`. Event payload: `UserFeeRatesUpdated(user, int64,
/// uint64)`, one per account written.
#[test]
fn rate_bounds_and_the_default_r_of_zero_refuse_any_negative_maker() {
    let mut ctx = fresh();
    assert_eq!(get_r(&mut ctx), 0, "R defaults to 0 (§1)");
    assert_eq!(get_rates(&mut ctx, BOB), (0, 0), "unset rates read (0, 0)");

    for m in [-1, -2, -100, -10_000, i64::MIN] {
        rejected(&mut ctx, ADMIN, &fees_call(BOB, m, 0), &e_rebate(0));
        rejected(&mut ctx, ADMIN, &fees_call(BOB, m, 500), &e_rebate(0));
    }
    // Upper bounds, exactly at and one past.
    rejected(&mut ctx, ADMIN, &fees_call(BOB, 100_001, 0), E_MAKER_MAX);
    rejected(&mut ctx, ADMIN, &fees_call(BOB, i64::MAX, 0), E_MAKER_MAX);
    rejected(&mut ctx, ADMIN, &fees_call(BOB, 0, 100_001), E_TAKER_MAX);
    rejected(&mut ctx, ADMIN, &fees_call(BOB, 0, u64::MAX), E_TAKER_MAX);
    // Admin only.
    rejected(&mut ctx, BOB, &fees_call(BOB, 0, 0), E_NOT_ADMIN);
    rejected(&mut ctx, ALICE, &fees_call(BOB, 100, 100), E_NOT_ADMIN);

    for (m, t) in [(0i64, 0u64), (100_000, 100_000), (0, 100_000), (100_000, 0), (1, 333)] {
        let c = ok(&mut ctx, ADMIN, &fees_call(BOB, m, t));
        let ev = evs::<UserFeeRatesUpdated>(&c.logs);
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0].user, ev[0].makerFeePpm, ev[0].takerFeePpm), (BOB, m, t));
        assert_eq!(c.logs.len(), 1, "a rate change logs exactly its UserFeeRatesUpdated");
        assert_eq!(get_rates(&mut ctx, BOB), (m, t));
    }
    // No cross-account effect, and no cross-check between maker and taker (§1): a maker rate far
    // above a 0 taker rate, and a 0 maker with a max taker, were both accepted above.
    assert_eq!(get_rates(&mut ctx, CAROL), (0, 0));
}

/// §1: `setMaxMakerRebate` is admin only, capped at the 10_000 ppm (1%) ceiling (inclusive),
/// emits `MaxMakerRebateUpdated(ppm)`, and `getMaxMakerRebate` reads it back. The maker bound is
/// `−R` exactly: `−R` accepted, `−R − 1` refused, with R read AT CALL TIME.
#[test]
fn the_rebate_bound_r_is_admin_only_ceilinged_evented_and_read_at_call_time() {
    let mut ctx = fresh();
    rejected(&mut ctx, BOB, &r_call(1), E_NOT_ADMIN);
    rejected(&mut ctx, ADMIN, &r_call(10_001), E_R_CEIL);
    rejected(&mut ctx, ADMIN, &r_call(u64::MAX), E_R_CEIL);
    assert_eq!(get_r(&mut ctx), 0);

    for r in [10_000u64, 300, 1, 0, 250] {
        let c = ok(&mut ctx, ADMIN, &r_call(r));
        let ev = evs::<MaxMakerRebateUpdated>(&c.logs);
        assert_eq!(ev.len(), 1, "R = {r}");
        assert_eq!(ev[0].maxMakerRebatePpm, r);
        assert_eq!(c.logs.len(), 1, "a bound change logs exactly its MaxMakerRebateUpdated");
        assert_eq!(get_r(&mut ctx), r);
    }

    // §4 "flat per selector": the new setter and view each cost the same whatever the argument.
    let gas = |ctx: &mut TestCtx, input: Vec<u8>, is_static: bool| {
        let caller = if is_static { ERIN } else { ADMIN };
        let out = run_perp_dex_call(&input, GAS, caller, U256::ZERO, is_static, ctx).unwrap();
        assert!(!out.reverted, "{}", revert_reason(&out));
        let _ = take_logs(ctx);
        out.gas_used
    };
    let g1 = gas(&mut ctx, r_call(1), false);
    let g2 = gas(&mut ctx, r_call(10_000), false);
    let g3 = gas(&mut ctx, r_call(250), false);
    assert!(g1 > 0 && g1 == g2 && g2 == g3, "setMaxMakerRebate gas is flat: {g1} {g2} {g3}");
    let v1 = gas(&mut ctx, getMaxMakerRebateCall {}.abi_encode(), true);
    let v2 = gas(&mut ctx, getMaxMakerRebateCall {}.abi_encode(), true);
    assert!(v1 > 0 && v1 == v2, "getMaxMakerRebate gas is flat");
    assert_eq!(get_r(&mut ctx), 250);

    // R = 250 now: −250 is in, −251 is out.
    rejected(&mut ctx, ADMIN, &fees_call(BOB, -251, 7), &e_rebate(250));
    let c = ok(&mut ctx, ADMIN, &fees_call(BOB, -250, 7));
    let ev = evs::<UserFeeRatesUpdated>(&c.logs);
    assert_eq!((ev[0].user, ev[0].makerFeePpm, ev[0].takerFeePpm), (BOB, -250, 7));
    assert_eq!(get_rates(&mut ctx, BOB), (-250, 7));

    // Raise R: the new bound applies to the very next call.
    set_r(&mut ctx, 10_000);
    ok(&mut ctx, ADMIN, &fees_call(CAROL, -10_000, 0));
    assert_eq!(get_rates(&mut ctx, CAROL), (-10_000, 0));
    rejected(&mut ctx, ADMIN, &fees_call(DAVE, -10_001, 0), &e_rebate(10_000));
}

/// §1 + §3: lowering R does NOT touch rates already set below −R (they read back unchanged and a
/// NEW order still snapshots them), nor an already-resting order's snapshot — even after the
/// owner's rate is changed to a positive one. Both are still rebated, and still capped per fill.
/// A positive rate set later applies only to orders that START resting afterwards (§3 "snapshot").
#[test]
fn lowering_r_keeps_rates_already_set_and_the_snapshots_already_resting() {
    let mut ctx = fresh();
    set_r(&mut ctx, 400);
    set_fees(&mut ctx, BOB, -400, 0);
    set_fees(&mut ctx, CAROL, -300, 0);
    set_fees(&mut ctx, ALICE, 0, 333);
    rest(&mut ctx, BOB, SELL, 10_000, 2);
    assert_eq!(snapshots(&mut ctx, BOB, SELL), vec![-400]);

    set_r(&mut ctx, 100);
    assert_eq!(get_rates(&mut ctx, BOB), (-400, 0), "lowering R must not touch a stored rate");
    assert_eq!(get_rates(&mut ctx, CAROL), (-300, 0), "lowering R must not touch a stored rate");
    rejected(&mut ctx, ADMIN, &fees_call(DAVE, -101, 0), &e_rebate(100));
    ok(&mut ctx, ADMIN, &fees_call(DAVE, -100, 0));

    // BOB's rate goes positive; his resting order keeps the −400 it snapshotted.
    set_fees(&mut ctx, BOB, 200, 0);
    assert_eq!(snapshots(&mut ctx, BOB, SELL), vec![-400], "the resting snapshot is kept");
    // CAROL (still −300, now below −R) rests a NEW order: it snapshots −300.
    rest(&mut ctx, CAROL, SELL, 10_001, 1);
    assert_eq!(snapshots(&mut ctx, CAROL, SELL), vec![-300]);

    // ALICE (t = 333) buys 3:
    //   fill 1: BOB 2 @ 10_000, n = 200_000_000, C = 200_000_000 → share 66_600;
    //           rebate min(floor(2e8·400/1e6) = 80_000, 66_600) = 66_600 — the stale −400 is
    //           still paid, and still CAPPED
    //   fill 2: CAROL 1 @ 10_001, n = 100_010_000, C = 300_010_000 → floor(99_903.33) − 66_600 =
    //           33_303; rebate min(floor(100_010_000·300/1e6) = 30_003, 33_303) = 30_003
    //   F = 99_903, admin = 99_903 − 66_600 − 30_003 = 3_300
    let c = ok(&mut ctx, ALICE, &market(BUY, 3));
    assert_eq!(
        c.fee_rows(),
        vec![(BOB, 10_000, 2, 66_600, -66_600), (CAROL, 10_001, 1, 33_303, -30_003)]
    );
    assert_eq!(c.d_wm(ALICE), -99_903);
    assert_eq!(c.d_wm(BOB), 66_600);
    assert_eq!(c.d_wm(CAROL), 30_003);
    assert_eq!(c.d_wallet(ADMIN), 3_300);

    // A new BOB order now snapshots +200 and PAYS: n = 1e8, maker 20_000, taker share 33_300.
    rest(&mut ctx, BOB, SELL, 10_000, 1);
    assert_eq!(snapshots(&mut ctx, BOB, SELL), vec![200]);
    let c = ok(&mut ctx, ALICE, &market(BUY, 1));
    assert_eq!(c.fee_rows(), vec![(BOB, 10_000, 1, 33_300, 20_000)]);
    assert_eq!(c.d_wm(BOB), -20_000);
    assert_eq!(c.d_wallet(ADMIN), 53_300);
    assert_eq!(get_fee_total(&mut ctx), 56_600);
}

/// §1: sub-accounts carry the master's rates — copied at `createSubAccount` and pushed down by
/// `setUserFeeRates(master)` (one `UserFeeRatesUpdated` per account, master first), a negative
/// maker included; a sub's own rates are refused; a refused master change writes nobody; and
/// lowering R leaves the whole family's rates alone.
#[test]
fn sub_accounts_inherit_and_receive_a_negative_maker_rate_from_their_master() {
    let mut ctx = fresh();
    set_r(&mut ctx, 300);
    let c = ok(&mut ctx, ALICE, &createSubAccountCall { name: "a".into() }.abi_encode());
    let sa0 = createSubAccountCall::abi_decode_returns(&c.out.bytes).unwrap();
    watch(sa0);
    assert_eq!(get_rates(&mut ctx, sa0), (0, 0));

    let c = ok(&mut ctx, ADMIN, &fees_call(ALICE, -250, 500));
    let ev: Vec<_> = evs::<UserFeeRatesUpdated>(&c.logs)
        .into_iter()
        .map(|e| (e.user, e.makerFeePpm, e.takerFeePpm))
        .collect();
    assert_eq!(ev, vec![(ALICE, -250, 500), (sa0, -250, 500)], "master first, then its sub");
    assert_eq!(get_rates(&mut ctx, sa0), (-250, 500));

    // A sub created afterwards inherits.
    let c = ok(&mut ctx, ALICE, &createSubAccountCall { name: "b".into() }.abi_encode());
    let sa1 = createSubAccountCall::abi_decode_returns(&c.out.bytes).unwrap();
    watch(sa1);
    assert_eq!(get_rates(&mut ctx, sa1), (-250, 500));

    rejected(&mut ctx, ADMIN, &fees_call(sa0, -1, 0), E_SUB);
    rejected(&mut ctx, ADMIN, &fees_call(sa1, 0, 0), E_SUB);
    // Past −R for the master: nobody is written (rejected() checks the subs' blobs too).
    rejected(&mut ctx, ADMIN, &fees_call(ALICE, -301, 500), &e_rebate(300));

    set_r(&mut ctx, 0);
    for u in [ALICE, sa0, sa1] {
        assert_eq!(get_rates(&mut ctx, u), (-250, 500), "lowering R must not touch {u}");
    }
    // At R = 0 the push-down still works for non-negative rates.
    let c = ok(&mut ctx, ADMIN, &fees_call(ALICE, 0, 200));
    assert_eq!(evs::<UserFeeRatesUpdated>(&c.logs).len(), 3);
    for u in [ALICE, sa0, sa1] {
        assert_eq!(get_rates(&mut ctx, u), (0, 200));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// §2 / §7 "rebate": the per-fill rebate and its cap
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/// §2 + §7 "t ≥ |m| ⇒ full per-fill rebate": BOB rests 3 at m = −250 and ALICE (t = 300) takes one
/// unit at a time, three matches. Each fill's rebate is the INDEPENDENT floor
/// `floor(n·250/1e6)` — the same every time — NOT the whole-order difference the positive path
/// uses, which would pay the second fill one more unit:
///
/// * buy:  n = 100_010_000 → rebate floor(25_002.5) = 25_002; share floor(30_003) = 30_003.
///   Whole-order difference: fee(3) = 75_007, fee(2) = 50_005, fee(1) = 25_002 → 25_002, **25_003**,
///   25_002.
/// * sell: n = 99_990_000 → rebate 24_997; share 29_997. Whole-order: 24_997, **24_998**, 24_997.
#[test]
fn a_covered_rebate_is_paid_in_full_as_an_independent_per_fill_floor() {
    for d in DIRS {
        let mut ctx = fresh();
        set_r(&mut ctx, 250);
        set_fees(&mut ctx, BOB, -250, 0);
        set_fees(&mut ctx, ALICE, 0, 300);
        let (price, n, share, rebate) =
            d.pick((10_001, 100_010_000i64, 30_003u64, 25_002i64), (9_999, 99_990_000, 29_997, 24_997));
        rest(&mut ctx, BOB, d.maker(), price, 3);
        let bob0 = row(&mut ctx, BOB);
        for k in 0..3 {
            let c = ok(&mut ctx, ALICE, &market(d.taker(), 1));
            assert_eq!(c.fee_rows(), vec![(BOB, price, 1, share, -rebate)], "{d:?} fill {k}");
            assert_eq!(c.d_wm(BOB), rebate, "{d:?} fill {k}: the rebate, and nothing else");
            assert_eq!(c.d_wm(ALICE), -(share as i64), "{d:?} fill {k}");
            assert_eq!(c.d_wallet(ADMIN), share as i64 - rebate, "{d:?} fill {k}");
        }
        // The rebate went to the WALLET: margin is exactly the opening margin (leverage 1 ⇒ IM = n),
        // untouched by any fee.
        let bob = row(&mut ctx, BOB);
        assert_eq!(bob.amount, -3 * d.sign());
        assert_eq!(bob.margin, 3 * n, "{d:?}: a rebate is never credited to margin");
        assert_eq!(bob.wallet - bob0.wallet, -3 * n + 3 * rebate, "{d:?}");
        assert_eq!(get_fee_total(&mut ctx), 3 * (share - rebate as u64), "{d:?}");
    }
}

/// §2 at `|m| = t` inside ONE multi-fill match: the cap never binds (`floor(n_i·t) ≤ share_i`
/// always), each maker gets its full floor, and the protocol keeps exactly the telescoping residue.
///
/// * buy (t = |m| = 333): BOB 1 @ 10_003 (n 100_030_000, floor 33_309.99), CAROL 1 @ 10_101
///   (n 101_010_000, floor 33_636.33). Shares: 33_309, floor(201_040_000·333/1e6) = 66_946 − 33_309
///   = **33_637**. Rebates 33_309 + 33_636; F = 66_946; admin = 1.
/// * sell: BOB 1 @ 9_993 (n 99_930_000 → 33_276.69), CAROL 1 @ 9_899 (n 98_990_000 → 32_963.67).
///   Shares 33_276, 66_240 − 33_276 = **32_964**. Rebates 33_276 + 32_963; F = 66_240; admin = 1.
#[test]
fn at_equal_rates_the_cap_never_binds_and_the_protocol_keeps_only_the_rounding_residue() {
    for d in DIRS {
        let mut ctx = fresh();
        set_r(&mut ctx, 333);
        set_fees(&mut ctx, BOB, -333, 0);
        set_fees(&mut ctx, CAROL, -333, 0);
        set_fees(&mut ctx, ALICE, 0, 333);
        let (p1, p2) = d.pick((10_003, 10_101), (9_993, 9_899));
        rest(&mut ctx, BOB, d.maker(), p1, 1);
        rest(&mut ctx, CAROL, d.maker(), p2, 1);
        let c = ok(&mut ctx, ALICE, &market(d.taker(), 2));
        let expect = d.pick(
            vec![(BOB, p1, 1, 33_309u64, -33_309i64), (CAROL, p2, 1, 33_637, -33_636)],
            vec![(BOB, p1, 1, 33_276, -33_276), (CAROL, p2, 1, 32_964, -32_963)],
        );
        assert_eq!(c.fee_rows(), expect, "{d:?}");
        let f = d.pick(66_946, 66_240);
        assert_eq!(c.d_wm(ALICE), -f, "{d:?}: F = floor(Σn·t/1e6), charged once");
        assert_eq!(c.d_wallet(ADMIN), 1, "{d:?}: the residue F − Σ rebates");
    }
}

/// The core §7 multi-fill scenario: one taker (t = 333) through three makers — a covered rebate
/// (−250), a positive maker (+200) and an UNDER-covered rebate (−500, the cap binds) — in one match.
///
/// buy, asks: BOB 1 @ 10_001, CAROL 1 @ 10_003, DAVE 2 @ 10_007.
///   n = 100_010_000 / 100_030_000 / 200_140_000; C = 100_010_000 / 200_040_000 / 400_180_000
///   floor(C·333/1e6) = 33_303 / 66_613 / 133_259 → shares 33_303 / **33_310** / 66_646
///   (independent floors: 33_303 / 33_309 / 66_646 — Σ 133_258 ≠ F)
///   BOB  rebate min(25_002, 33_303)  = 25_002          (full)
///   CAROL fee   floor(100_030_000·200/1e6) = 20_006     (whole order, filled at once)
///   DAVE rebate min(100_070, 66_646) = 66_646          (capped)
///   F = 133_259; Σ rebates = 91_648; admin = 133_259 − 91_648 + 20_006 = 61_617
/// sell, bids: BOB 1 @ 9_999, CAROL 1 @ 9_997, DAVE 2 @ 9_993.
///   n = 99_990_000 / 99_970_000 / 199_860_000; floor(C·333/1e6) = 33_296 / 66_586 / 133_140
///   shares 33_296 / 33_290 / **66_554** (independent 66_553)
///   BOB 24_997 (full); CAROL fee 19_994; DAVE min(99_930, 66_554) = 66_554 (capped)
///   F = 133_140; admin = 133_140 − 91_551 + 19_994 = 61_583
#[test]
fn a_multi_fill_match_telescopes_the_taker_fee_and_caps_each_rebate_at_its_own_share() {
    for d in DIRS {
        let mut ctx = fresh();
        set_r(&mut ctx, 500);
        set_fees(&mut ctx, BOB, -250, 0);
        set_fees(&mut ctx, CAROL, 200, 0);
        set_fees(&mut ctx, DAVE, -500, 0);
        set_fees(&mut ctx, ALICE, 0, 333);
        let (pb, pc, pd) = d.pick((10_001, 10_003, 10_007), (9_999, 9_997, 9_993));
        rest(&mut ctx, BOB, d.maker(), pb, 1);
        rest(&mut ctx, CAROL, d.maker(), pc, 1);
        rest(&mut ctx, DAVE, d.maker(), pd, 2);
        let fee0 = get_fee_total(&mut ctx);

        let c = ok(&mut ctx, ALICE, &market(d.taker(), 4));
        let (rows, f, admin) = d.pick(
            (
                vec![
                    (BOB, pb, 1, 33_303u64, -25_002i64),
                    (CAROL, pc, 1, 33_310, 20_006),
                    (DAVE, pd, 2, 66_646, -66_646),
                ],
                133_259i64,
                61_617i64,
            ),
            (
                vec![
                    (BOB, pb, 1, 33_296, -24_997),
                    (CAROL, pc, 1, 33_290, 19_994),
                    (DAVE, pd, 2, 66_554, -66_554),
                ],
                133_140,
                61_583,
            ),
        );
        assert_eq!(c.fee_rows(), rows, "{d:?}");
        assert!(c.fills().iter().all(|x| x.taker == ALICE));

        // Σ shares == F == what the taker actually paid (§2).
        let sum_shares: u64 = rows.iter().map(|r| r.3).sum();
        assert_eq!(sum_shares as i64, f, "{d:?}: Σ Trade.takerFee == F");
        assert_eq!(c.d_wm(ALICE), -f, "{d:?}: the taker pays F once");
        // Σ rebates ≤ F; admin == F − Σ rebates + Σ positive maker fees == fee-total delta.
        let rebates: i64 = rows.iter().filter(|r| r.4 < 0).map(|r| -r.4).sum();
        let charges: i64 = rows.iter().filter(|r| r.4 > 0).map(|r| r.4).sum();
        assert!(rebates <= f);
        assert_eq!(c.d_wallet(ADMIN), f - rebates + charges, "{d:?}");
        assert_eq!(c.d_wallet(ADMIN), admin, "{d:?}");
        assert_eq!(get_fee_total(&mut ctx) - fee0, admin as u64, "{d:?}: getMarketFeeTotal is net");

        // Where each fee landed. Leverage 1 ⇒ opening margin = n.
        let (nb, nc, nd) = d.pick(
            (100_010_000i64, 100_030_000i64, 200_140_000i64),
            (99_990_000, 99_970_000, 199_860_000),
        );
        let (rb, fc, rd) = (-rows[0].4, rows[1].4, -rows[2].4);
        // Rebates: wallet −IM + rebate, margin = IM (no fee_from_margin for a rebate, §2).
        assert_eq!((c.d_wallet(BOB), c.after.row(BOB).margin), (-nb + rb, nb), "{d:?} BOB");
        assert_eq!((c.d_wallet(DAVE), c.after.row(DAVE).margin), (-nd + rd, nd), "{d:?} DAVE");
        // Positive maker: unchanged algorithm — out of the opening margin first.
        assert_eq!((c.d_wallet(CAROL), c.after.row(CAROL).margin), (-nc, nc - fc), "{d:?} CAROL");
        // Taker: out of its opening margin first.
        let total_n = nb + nc + nd;
        assert_eq!(c.after.row(ALICE).margin, total_n - f, "{d:?} ALICE margin");
        assert_eq!(c.after.row(ALICE).amount, 4 * d.sign());
    }
}

/// §2/§3 "t = 0 ⇒ 0": a 0-fee taker's share is 0 on every fill, so a rebate maker gets nothing,
/// while a positive maker in the same match still pays its commission — to the admin.
///
/// buy: BOB (−250) 1 @ 10_001 → 0; CAROL (+200) 1 @ 10_003 → 20_006.
/// sell: BOB 1 @ 9_999 → 0; CAROL 1 @ 9_997 → floor(99_970_000·200/1e6) = 19_994.
#[test]
fn a_zero_fee_taker_funds_no_rebate_while_a_positive_maker_still_pays() {
    for d in DIRS {
        let mut ctx = fresh();
        set_r(&mut ctx, 250);
        set_fees(&mut ctx, BOB, -250, 0);
        set_fees(&mut ctx, CAROL, 200, 0);
        // ALICE stays at (0, 0).
        let (pb, pc) = d.pick((10_001, 10_003), (9_999, 9_997));
        rest(&mut ctx, BOB, d.maker(), pb, 1);
        rest(&mut ctx, CAROL, d.maker(), pc, 1);
        let c = ok(&mut ctx, ALICE, &market(d.taker(), 2));
        let fc = d.pick(20_006i64, 19_994);
        assert_eq!(c.fee_rows(), vec![(BOB, pb, 1, 0, 0), (CAROL, pc, 1, 0, fc)], "{d:?}");
        assert_eq!(c.d_wm(BOB), 0, "{d:?}: no rebate without a taker fee to fund it");
        assert_eq!(c.d_wm(ALICE), 0);
        assert_eq!(c.d_wm(CAROL), -fc);
        assert_eq!(c.d_wallet(ADMIN), fc);
    }
}

/// §3 "liquidation close": the liquidated user's taker fee is waived even though its rate is
/// non-zero (t = 500), so every rebate in that match is 0 — while a positive maker still pays.
///
/// long ALICE 3 @ $100 at 3x (margin $100) with the mark at $75 (equity $25 < MM $37.5): the close
/// sells into BOB (−250) 2 @ 7_500 and CAROL (+200) 1 @ 7_499 → CAROL pays
/// floor(74_990_000·200/1e6) = 14_998.
/// short ALICE 3 at the mark $125 (equity $25 < MM $62.5): buys BOB 2 @ 12_500 and CAROL 1 @ 12_501
/// → CAROL pays floor(125_010_000·200/1e6) = 25_002.
#[test]
fn a_liquidation_close_pays_no_rebate_while_a_positive_maker_still_pays() {
    for long in [true, false] {
        let mut ctx = fresh();
        set_r(&mut ctx, 250);
        set_fees(&mut ctx, BOB, -250, 0);
        set_fees(&mut ctx, CAROL, 200, 0);
        set_fees(&mut ctx, ALICE, 0, 500);
        let (mark, pb, pc, fc) = if long {
            (7_500, 7_500, 7_499, 14_998i64)
        } else {
            (12_500, 12_500, 12_501, 25_002)
        };
        seed_pos(&mut ctx, ALICE, if long { 3 } else { -3 }, 3);
        seed_pos(&mut ctx, ERIN, if long { -3 } else { 3 }, 1);
        storage::save_mark_price(&mut ctx, MARKET_ID, mark).unwrap();
        let maker_side = if long { BUY } else { SELL };
        rest(&mut ctx, BOB, maker_side, pb, 2);
        rest(&mut ctx, CAROL, maker_side, pc, 1);

        let c = ok(&mut ctx, ERIN, &liquidateCall { user: ALICE, marketId: MARKET_ID }.abi_encode());
        assert_eq!(evs::<Liquidation>(&c.logs).len(), 1, "long={long}: ALICE was liquidated");
        assert_eq!(
            c.fee_rows(),
            vec![(BOB, pb, 2, 0, 0), (CAROL, pc, 1, 0, fc)],
            "long={long}"
        );
        assert_eq!(c.after.row(ALICE).amount, 0);
        assert_eq!(c.d_wm(BOB), 0, "long={long}: a liquidation close funds no rebate");
        assert_eq!(c.d_wm(CAROL), -fc, "long={long}");
        assert_eq!(c.d_wallet(ADMIN), fc, "long={long}");
    }
}

/// §2 "where the rebate lands": on a PURE full close the maker's position goes flat with margin 0
/// (a margin credit would strand money on a flat position) and the rebate is in the WALLET:
/// BOB holds 2 at $100 (margin $200) and rests the closing order at 10_000; ALICE (t = 500) takes
/// it: n = 2e8, share 100_000, rebate floor(2e8·250/1e6) = 50_000 →
/// BOB wallet + $200 released + 50_000, margin 0, vQuote 0.
///
/// And on a FLIP (BOB holds 1, rests 3): close 1 (release $100), open 2 (IM $200), rebate
/// floor(3e8·250/1e6) = 75_000 to the wallet; margin is the new IM exactly.
#[test]
fn a_rebate_on_a_full_close_lands_in_the_wallet_and_the_flat_position_holds_no_margin() {
    for d in DIRS {
        let mut ctx = fresh();
        set_r(&mut ctx, 250);
        set_fees(&mut ctx, BOB, -250, 0);
        set_fees(&mut ctx, ALICE, 0, 500);
        // BOB holds the TAKER's side (long 2 against a buying taker): his resting order on the
        // other side closes it.
        seed_pos(&mut ctx, BOB, 2 * d.sign(), 1);
        seed_pos(&mut ctx, ERIN, -2 * d.sign(), 1);
        rest(&mut ctx, BOB, d.maker(), MARK, 2);
        let w0 = row(&mut ctx, BOB).wallet;
        let c = ok(&mut ctx, ALICE, &market(d.taker(), 2));
        assert_eq!(c.fee_rows(), vec![(BOB, MARK, 2, 100_000, -50_000)], "{d:?}");
        let bob = c.after.row(BOB);
        assert_eq!(
            bob,
            Row {
                wallet: w0 + 2 * UNIT_VALUE + 50_000,
                margin: 0,
                vq: 0,
                amount: 0
            },
            "{d:?}: flat, margin 0, rebate in the wallet"
        );

        // The flip.
        let mut ctx = fresh();
        set_r(&mut ctx, 250);
        set_fees(&mut ctx, BOB, -250, 0);
        set_fees(&mut ctx, ALICE, 0, 500);
        seed_pos(&mut ctx, BOB, d.sign(), 1);
        seed_pos(&mut ctx, ERIN, -d.sign(), 1);
        rest(&mut ctx, BOB, d.maker(), MARK, 3);
        let w0 = row(&mut ctx, BOB).wallet;
        let c = ok(&mut ctx, ALICE, &market(d.taker(), 3));
        assert_eq!(c.fee_rows(), vec![(BOB, MARK, 3, 150_000, -75_000)], "{d:?} flip");
        let bob = c.after.row(BOB);
        assert_eq!(bob.amount, -2 * d.sign(), "{d:?} flip");
        assert_eq!(bob.margin, 2 * UNIT_VALUE, "{d:?} flip: margin is the new IM, no rebate in it");
        assert_eq!(bob.wallet, w0 + UNIT_VALUE - 2 * UNIT_VALUE + 75_000, "{d:?} flip");
    }
}

/// §2: the rebate is credited AFTER `apply_position_fill` and AFTER K9, so it neither enlarges the
/// M1 cash-at-hand cap for its own fill nor changes the K9 verdict.
///
/// BOB rests 1 at m = −250 with a $1e9 wallet, then the wallet is planted at `W` (below the
/// opening requirement, so M1 caps the opening margin at `W`). ALICE (t = 333) market-takes 1.
///
/// buy: BOB short 1 @ 10_003 at mark 10_000: equity = −1e8 + 100_030_000 + margin =
///   margin + 30_000; MM = floor(1e8/6) = 16_666_666 ⇒ passes iff margin ≥ 16_636_666.
///   rebate = min(floor(100_030_000·250/1e6) = 25_007, share 33_309) = 25_007.
/// sell: BOB long 1 @ 9_993: equity = margin + 70_000 ⇒ passes iff margin ≥ 16_596_666.
///   rebate = min(24_982, 33_276) = 24_982.
///
/// * `W` = the boundary: fills; margin == W EXACTLY (not W + rebate: the rebate was not cash at
///   hand), wallet == the rebate.
/// * `W` = boundary − 1: K9 rejects and BOB's order is cancelled — although `W + rebate` would have
///   passed, so a rebate credited before the guard (to margin, or to the wallet ahead of the M1
///   cap) flips this verdict. The taker walks on to CAROL (m = 0), and since the rejected maker is
///   NOT a fill, the single real fill's share is the whole fee, `floor(n_C·333/1e6)`:
///   buy 10_101 → 33_636 (a share that had "consumed" BOB's notional first would be 33_637);
///   sell 9_899 → 32_963 (would be 32_964).
#[test]
fn the_rebate_changes_neither_the_k9_verdict_nor_the_m1_cash_cap_of_its_own_fill() {
    for d in DIRS {
        let (pb, pc, boundary, rebate, share_b, f_c) = d.pick(
            (10_003, 10_101, 16_636_666i64, 25_007i64, 33_309u64, 33_636i64),
            (9_993, 9_899, 16_596_666, 24_982, 33_276, 32_963),
        );
        for w in [boundary, boundary - 1] {
            let tag = format!("{d:?} W={w}");
            let mut ctx = fresh();
            set_r(&mut ctx, 250);
            set_fees(&mut ctx, BOB, -250, 0);
            set_fees(&mut ctx, ALICE, 0, 333);
            rest(&mut ctx, BOB, d.maker(), pb, 1);
            rest(&mut ctx, CAROL, d.maker(), pc, 1);
            set_wallet(&mut ctx, BOB, w);
            let c = ok(&mut ctx, ALICE, &market(d.taker(), 1));
            let bob = c.after.row(BOB);
            if w == boundary {
                assert_eq!(c.fee_rows(), vec![(BOB, pb, 1, share_b, -rebate)], "{tag}");
                assert_eq!(bob.margin, w, "{tag}: M1 cap is W — the rebate is not cash at hand");
                assert_eq!(bob.wallet, rebate, "{tag}: the rebate lands in the wallet, after");
                assert_eq!(bob.amount, -d.sign());
            } else {
                assert!(
                    c.fills().iter().all(|f| f.maker != BOB),
                    "{tag}: K9 must still reject — the rebate cannot rescue the fill"
                );
                assert!(evs::<OrderCancelled>(&c.logs).iter().any(|e| e.user == BOB), "{tag}");
                assert_eq!(bob, Row { wallet: w, margin: 0, vq: 0, amount: 0 }, "{tag}");
                assert!(snapshots(&mut ctx, BOB, d.maker()).is_empty(), "{tag}");
                assert_eq!(c.fee_rows(), vec![(CAROL, pc, 1, f_c as u64, 0)], "{tag}");
                assert_eq!(c.d_wm(ALICE), -f_c, "{tag}: a rejected maker consumes no share");
                assert_eq!(c.d_wallet(ADMIN), f_c, "{tag}");
            }
        }
    }
}

/// §3 "FOK self-trade": FOK skips STP, so the same account is taker and maker. Its net is
/// `share − rebate ≥ 0` to the protocol, and the position ends flat.
///
/// (−250, 500): buy at 10_001: share floor(100_010_000·500/1e6) = 50_005, rebate 25_002 → ALICE
///   −25_003, admin +25_003. sell at 9_999: share 49_995, rebate 24_997 → −24_998.
/// (−500, 300): share 30_003 / 29_997, rebate capped to the share → net 0, admin 0.
#[test]
fn a_fok_self_trade_nets_the_rebate_against_its_own_taker_fee() {
    for d in DIRS {
        for (m, t) in [(-250i64, 500u64), (-500, 300)] {
            let tag = format!("{d:?} m={m} t={t}");
            let mut ctx = fresh();
            set_r(&mut ctx, 500);
            set_fees(&mut ctx, ALICE, m, t);
            let p = d.pick(10_001, 9_999);
            rest(&mut ctx, ALICE, d.maker(), p, 1);
            let c = ok(&mut ctx, ALICE, &limit(d.taker(), p, 1, FOK));
            let (share, rebate) = match (d, m) {
                (Dir::Buy, -250) => (50_005u64, 25_002i64),
                (Dir::Sell, -250) => (49_995, 24_997),
                (Dir::Buy, _) => (30_003, 30_003),
                (Dir::Sell, _) => (29_997, 29_997),
            };
            let f = c.fills();
            assert_eq!(f.len(), 1, "{tag}");
            assert_eq!((f[0].taker, f[0].maker), (ALICE, ALICE), "{tag}: a self-trade");
            assert_eq!((f[0].taker_fee, f[0].maker_fee), (share, -rebate), "{tag}");
            assert_eq!(c.d_wm(ALICE), rebate - share as i64, "{tag}");
            assert_eq!(c.d_wallet(ADMIN), share as i64 - rebate, "{tag}: ≥ 0");
            let a = c.after.row(ALICE);
            assert_eq!((a.amount, a.margin, a.vq), (0, 0, 0), "{tag}: flat");
        }
    }
}

/// §3 "family trades": master and sub (same rates, pushed down) trade with each other — STP is by
/// address only — and each fill's net protocol fee is `share − rebate ≥ 0`.
///
/// sub rests, master takes (buy @ 10_001): share 50_005, rebate 25_002 → admin +25_003.
/// master rests, sub takes (sell @ 9_999): share 49_995, rebate 24_997 → admin +24_998.
#[test]
fn a_master_sub_family_trade_nets_to_a_non_negative_protocol_fee() {
    for sub_is_maker in [true, false] {
        let mut ctx = fresh();
        set_r(&mut ctx, 250);
        let c = ok(&mut ctx, ALICE, &createSubAccountCall { name: "mm".into() }.abi_encode());
        let sa0 = createSubAccountCall::abi_decode_returns(&c.out.bytes).unwrap();
        watch(sa0);
        set_wallet(&mut ctx, sa0, WALLET);
        set_fees(&mut ctx, ALICE, -250, 500);
        assert_eq!(get_rates(&mut ctx, sa0), (-250, 500));

        let (maker, taker, share, rebate, price) = if sub_is_maker {
            let c = ok(&mut ctx, ALICE, &as_sub(sa0, limit(SELL, 10_001, 1, GTC)));
            assert!(c.fills().is_empty());
            (sa0, ALICE, 50_005u64, 25_002i64, 10_001)
        } else {
            rest(&mut ctx, ALICE, BUY, 9_999, 1);
            (ALICE, sa0, 49_995, 24_997, 9_999)
        };
        let c = if sub_is_maker {
            ok(&mut ctx, ALICE, &market(BUY, 1))
        } else {
            ok(&mut ctx, ALICE, &as_sub(sa0, market(SELL, 1)))
        };
        let f = c.fills();
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].taker, f[0].maker, f[0].price), (taker, maker, price));
        assert_eq!((f[0].taker_fee, f[0].maker_fee), (share, -rebate));
        assert_eq!(c.d_wm(maker), rebate);
        assert_eq!(c.d_wm(taker), -(share as i64));
        assert_eq!(c.d_wm(ALICE) + c.d_wm(sa0), rebate - share as i64, "the family's net");
        assert_eq!(c.d_wallet(ADMIN), share as i64 - rebate, "≥ 0 for the protocol");
    }
}

/// §3 "batches": each batch item is its own match with its own registry — its own taker fee, its
/// own telescoping, its own cap. Batch [buy 1 @ 10_001 IOC, buy 1 @ 10_003 IOC] (t = 333) against
/// BOB (−500) 1 @ 10_001 and CAROL (−250) 1 @ 10_003:
///
/// * item 1: share floor(100_010_000·333/1e6) = 33_303; rebate min(50_005, 33_303) = 33_303.
/// * item 2: share floor(100_030_000·333/1e6) = **33_309** (a batch-wide running notional would give
///   33_310); rebate min(25_007, 33_309) = 25_007.
/// * ALICE pays 33_303 + 33_309 = 66_612 (one fee per item; a batch-wide fee would be 66_613);
///   admin = 0 + 8_302.
///
/// sell (bids 9_999 / 9_997): shares 33_296 / 33_290; rebates 33_296 / 24_992 → admin 8_298.
#[test]
fn each_batch_item_is_its_own_match_with_its_own_cap() {
    for d in DIRS {
        let mut ctx = fresh();
        set_r(&mut ctx, 500);
        set_fees(&mut ctx, BOB, -500, 0);
        set_fees(&mut ctx, CAROL, -250, 0);
        set_fees(&mut ctx, ALICE, 0, 333);
        let (p1, p2) = d.pick((10_001, 10_003), (9_999, 9_997));
        rest(&mut ctx, BOB, d.maker(), p1, 1);
        rest(&mut ctx, CAROL, d.maker(), p2, 1);
        let c = ok(
            &mut ctx,
            ALICE,
            &batchPlaceOrdersCall {
                orders: vec![item(d.taker(), p1, 1, IOC), item(d.taker(), p2, 1, IOC)],
            }
            .abi_encode(),
        );
        let blob = batchPlaceOrdersCall::abi_decode_returns(&c.out.bytes).unwrap();
        assert!(blob.chunks(34).all(|r| r[0] != 0), "{d:?}: no item rejected");
        let (rows, paid, admin) = d.pick(
            (
                vec![(BOB, p1, 1, 33_303u64, -33_303i64), (CAROL, p2, 1, 33_309, -25_007)],
                66_612i64,
                8_302i64,
            ),
            (
                vec![(BOB, p1, 1, 33_296, -33_296), (CAROL, p2, 1, 33_290, -24_992)],
                66_586,
                8_298,
            ),
        );
        assert_eq!(c.fee_rows(), rows, "{d:?}");
        assert_eq!(c.d_wm(ALICE), -paid, "{d:?}");
        assert_eq!(c.d_wallet(ADMIN), admin, "{d:?}");
    }
}

/// §3 "triggered conditionals": a fired conditional is its own match with the same cap. ALICE
/// (t = 333) holds 10 against the move; a reduce-only STOP_MARKET for 3 fires on the tick.
///
/// buy (ALICE short 10; trigger 10_500, tick 10_600): BOB (−400) 1 @ 10_700, CAROL (−100)
///   2 @ 10_800. n = 107_000_000 / 216_000_000 → shares 35_631 / 71_928 (F 107_559);
///   BOB min(42_800, 35_631) = 35_631 (capped); CAROL min(21_600, 71_928) = 21_600.
///   admin = 107_559 − 57_231 = 50_328.
/// sell (ALICE long 10; trigger 9_500, tick 9_400): BOB 1 @ 9_300, CAROL 2 @ 9_200.
///   n = 93_000_000 / 184_000_000 → shares 30_969 / 61_272 (F 92_241); BOB min(37_200, 30_969) =
///   30_969 (capped); CAROL 18_400. admin = 92_241 − 49_369 = 42_872.
#[test]
fn a_fired_conditional_is_its_own_match_with_the_same_cap() {
    for d in DIRS {
        let mut ctx = fresh();
        set_r(&mut ctx, 400);
        set_fees(&mut ctx, BOB, -400, 0);
        set_fees(&mut ctx, CAROL, -100, 0);
        set_fees(&mut ctx, ALICE, 0, 333);
        seed_pos(&mut ctx, ALICE, -10 * d.sign(), 1);
        seed_pos(&mut ctx, ERIN, 10 * d.sign(), 1);
        let (trigger, index, pb, pc) = d.pick((10_500, 10_600, 10_700, 10_800), (9_500, 9_400, 9_300, 9_200));
        rest(&mut ctx, BOB, d.maker(), pb, 1);
        rest(&mut ctx, CAROL, d.maker(), pc, 2);
        let c = ok(&mut ctx, ALICE, &stop_market(d.taker(), trigger, 3));
        assert!(c.fills().is_empty(), "{d:?}: not triggered yet");

        let c = tick(&mut ctx, index);
        assert_eq!(evs::<AlgoOrderTriggered>(&c.logs).len(), 1, "{d:?}: the stop fired");
        let (rows, f, admin) = d.pick(
            (
                vec![(BOB, pb, 1, 35_631u64, -35_631i64), (CAROL, pc, 2, 71_928, -21_600)],
                107_559i64,
                50_328i64,
            ),
            (
                vec![(BOB, pb, 1, 30_969, -30_969), (CAROL, pc, 2, 61_272, -18_400)],
                92_241,
                42_872,
            ),
        );
        assert_eq!(c.fee_rows(), rows, "{d:?}");
        assert!(c.fills().iter().all(|x| x.taker == ALICE));
        // ALICE closes 3 of 10: her wallet + margin moves by the released margin and the realized
        // loss as well as the fee, so pin the fee through the admin and the events instead.
        assert_eq!(c.d_wallet(ADMIN), admin, "{d:?}");
        let sum_shares: u64 = rows.iter().map(|r| r.3).sum();
        assert_eq!(sum_shares as i64, f, "{d:?}");
        assert_eq!(c.after.row(ALICE).amount, -7 * d.sign());
        assert_eq!(c.d_wm(BOB), -rows[0].4, "{d:?}");
        assert_eq!(c.d_wm(CAROL), -rows[1].4, "{d:?}");
    }
}

/// §2 commit-only: the walk writes nothing, so a taker rejected after it (here: it cannot fund the
/// fills) leaves no rebate behind — no maker credited, nothing written, nothing logged.
#[test]
fn a_rejected_taker_leaks_no_rebate() {
    for d in DIRS {
        let mut ctx = fresh();
        set_r(&mut ctx, 250);
        set_fees(&mut ctx, BOB, -250, 0);
        set_fees(&mut ctx, ALICE, 0, 500);
        rest(&mut ctx, BOB, d.maker(), d.pick(10_001, 9_999), 1);
        set_wallet(&mut ctx, ALICE, 1_000);
        rejected(&mut ctx, ALICE, &market(d.taker(), 1), E_MARGIN);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════════════════════
// §1 / §7 "byte-identity": positive rates in ppm reproduce the old bps results (×100)
// ═══════════════════════════════════════════════════════════════════════════════════════════════

/// The pre-change (bps) fee, written here from the old definition: `floor(n·bps/10_000)`.
fn old_bps_fee(notional: u64, bps: u64) -> u64 {
    (notional as u128 * bps as u128 / 10_000) as u64
}

/// §1: for every whole-bps rate 0..=1000 and a spread of notionals (small, ragged, near the u64
/// top), the ppm fee at ×100 is exactly the old bps fee — for the taker formula and for the maker's
/// whole-order quantity form.
#[test]
fn ppm_at_100x_is_the_old_bps_fee_for_every_whole_bps_rate() {
    let mut notionals: Vec<u64> = vec![0, 1, 99, 9_999, 10_000, 10_001, 123_487_035, 999_999_999];
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..64 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        notionals.push(x >> 8); // ≤ 2^56: n·MAX_FEE_PPM stays far inside u128
    }
    notionals.push(u64::MAX / 100_000);
    for bps in 0..=1_000u64 {
        for &n in &notionals {
            assert_eq!(
                perp_core::math::calc_trading_fee(n, bps * 100).unwrap(),
                old_bps_fee(n, bps),
                "n={n} bps={bps}"
            );
        }
    }
    let m = Market {
        market_id: MARKET_ID,
        base_decimals: 4,
        price_decimals: 2,
        tick_size: 1,
        step_size: 1,
        min_quantity: 1,
        max_quantity: 1_000_000,
        max_price: 1_000_000,
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
    };
    for bps in [0u64, 1, 2, 3, 5, 7, 9, 10, 33, 999, 1_000] {
        for (p, q) in [(10_003u64, 31_234u64), (10_007, 20_007), (1, 1), (999_999, 999_999)] {
            // base 4 / price 2: n = P·q.
            assert_eq!(
                perp_core::math::calc_maker_fee_for_order_qty(p, q, bps * 100, &m).unwrap(),
                old_bps_fee(p * q, bps),
                "P={p} q={q} bps={bps}"
            );
        }
    }
}

/// §1 + §7 byte-identity through the engine: a positive-rate scenario on a market where bps fees
/// DO truncate (base 4 / price 2 ⇒ `n = P·q`) settles exactly as the OLD bps code would have.
/// Rates 3 / 7 / 9 bps become 300 / 700 / 900 ppm. Old-model numbers (python, from the old
/// definitions: taker `floor(Σn·b/1e4)` once per match, maker whole-order difference):
///
/// CAROL (3 bps) rests 31_234 @ 10_003; DAVE (7 bps) rests 20_007 @ 10_007; ALICE (9 bps):
/// * match 1, buy 12_345: n = 123_487_035; CAROL fee(31_234) − fee(18_889) = 93_730 − 56_684 =
///   37_046; F = floor(123_487_035·9/1e4) = 111_138.
/// * match 2, buy 38_896: CAROL 18_889 → n 188_946_667, fee 56_684 − 0 = 56_684; DAVE 20_007 →
///   n 200_210_049, fee floor(140_147.03) = 140_147; F = floor(389_156_716·9/1e4) = 350_241.
/// * totals: ALICE −461_379, CAROL −93_730, DAVE −140_147, admin +695_256.
#[test]
fn positive_ppm_rates_settle_exactly_as_the_old_bps_rates() {
    let mut ctx = market_with(4);
    set_fees(&mut ctx, CAROL, 300, 0);
    set_fees(&mut ctx, DAVE, 700, 0);
    set_fees(&mut ctx, ALICE, 0, 900);
    rest(&mut ctx, CAROL, SELL, 10_003, 31_234);
    rest(&mut ctx, DAVE, SELL, 10_007, 20_007);
    let s0 = snap(&mut ctx);

    let c1 = ok(&mut ctx, ALICE, &market(BUY, 12_345));
    let maker_fees: Vec<_> = c1.fills().iter().map(|f| (f.maker, f.qty, f.maker_fee)).collect();
    assert_eq!(maker_fees, vec![(CAROL, 12_345, 37_046)]);
    assert_eq!(c1.d_wm(ALICE), -111_138);
    let c2 = ok(&mut ctx, ALICE, &market(BUY, 38_896));
    let maker_fees: Vec<_> = c2.fills().iter().map(|f| (f.maker, f.qty, f.maker_fee)).collect();
    assert_eq!(maker_fees, vec![(CAROL, 18_889, 56_684), (DAVE, 20_007, 140_147)]);
    assert_eq!(c2.d_wm(ALICE), -350_241);

    let s1 = snap(&mut ctx);
    let d_wm = |u: Address| {
        let (a, b) = (s1.row(u), s0.row(u));
        (a.wallet + a.margin) - (b.wallet + b.margin)
    };
    assert_eq!(d_wm(ALICE), -461_379);
    assert_eq!(d_wm(CAROL), -93_730);
    assert_eq!(d_wm(DAVE), -140_147);
    assert_eq!(s1.row(ADMIN).wallet - s0.row(ADMIN).wallet, 695_256);
    assert_eq!(s1.fee_total - s0.fee_total, 695_256);
}
