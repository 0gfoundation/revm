//! **Cross margin P2 — an independent spec suite.**
//!
//! Written from `SPEC_cross_margin.md` (owner decisions Q1–Q12, the P2 ground rules) and the design
//! report `CROSS_MARGIN_explore.md` (§1.1–§1.3 Binance's balance fields, §1.5 the cross liquidation
//! condition and LP1, §1.8 funding / leverage / codes, §2 G1–G16, §3.0 grafts J1–J7, §3.2 the
//! formulas, §3.3 items 1–11, §3.5 the API), NOT from the implementation's own tests
//! (`cross_accounting_tests`, `cross_settlement_tests`, `cross_gates_tests`) nor from its
//! internals. Every call goes through the full shell (`run_perp_dex_call`); state is only READ
//! directly, to feed the model its inputs (positions, wallets, marks, funding indices, order
//! lists).
//!
//! # The model (§3.2), written from the formulas
//!
//! ```text
//! W            perp_wallet_balance (signed)
//! N_k          trunc(amount_k · mark_k)            (one contract at price p is p·10^6 here)
//! uPnL_k       N_k + vQuote_k                      (0 for a cross leg at mark 0 — J4)
//! PF_k         −amount_k · (I_k − anchor_k)        (the FundingState index, NOT the Market mirror)
//! PIM_k        ⌈|N_k| / L_k⌉
//! IM_k         ⌈max(|N_k + Bid_k|, |N_k − Ask_k|) / L_k⌉        ooIM_k = IM_k − PIM_k
//! MM_k         slice form over the tier table set through the ABI (explicit or derived rate)
//! cWB          W + Σ_C PF_k
//! E            cWB + Σ_C uPnL_k
//! A            cWB + Σ_C (uPnL_k − PIM_k) − Σ_{C∪I} ooIM_k
//! X            max(0, min(cWB − Σ_I ooIM_k − Σ_C MM_k, A, E − Σ_I ooIM_k − Σ_C MM_k))
//! LP (cross k) the mark of k at which E < Σ_C MM, every other cross leg held (LP1)
//! LP (iso k)   the mark of k at which margin_k + uPnL_k < MM_k
//! ```
//!
//! After EVERY successful call ([`ok`]) the model is compared with `getAccount` (every scalar and
//! every row) and with `getPositionRisk` for every market, for every enrolled account; the custody
//! identity `Σ (spot + W + Σ (margin + vQuote + PF)) + IF` must not move; `Σ amount == 0` per
//! market; each account's own custody moves by exactly its trade cash and fees (`Trade` events) on
//! every call that is not a liquidation; the insurance fund moves only on the calls that may move
//! it; `open_position_count` counts the open positions; and a position CROSSED in force holds no
//! margin. Every REFUSED call ([`rejected`]) must name the exact reason with zero perp writes, no
//! log and a bit-identical world.
//!
//! Markets use base and price decimals 0, so every value, fee and funding payment is an exact
//! integer and the identity closes to the unit (funding epochs included).
//!
//! # Map (test prefix → spec item)
//!
//! * `walk_*` — multi-market scripted and randomised walks: cross + isolated mixed, marks moving
//!   one market at a time, funding epochs, tier crossings, partial closes, flips, maker and taker,
//!   fees, a negative W.
//! * `f_*` — §3.2 identities: isolated-only accounts, cross profit opens but does not withdraw,
//!   pending funding in cWB, the zero mark (J4), the mirror surviving market admin calls.
//! * `fill_*` — §3.3 item 2: a cross fill moves no cash, settles realized PnL into W either sign,
//!   no per-fill bad debt, fees from W, flips; the open-position count.
//! * `fund_*` — §3.3 item 6 / F32: cross funding into W with the B-only event shape.
//! * `gate_*` — §3.3 items 3, 5, 7, 8, 9, 10, J1, J6, Q9, Q11: every boundary to the unit.
//! * `liq_*` — the sweep and `liquidate()` judge a CROSSED position on its account (P3; the P2
//!   interim skipped it), ADL with cross winners.
//! * `gated_*` — gated off, an isolated multi-market scenario is byte-identical to the same
//!   scenario with the gate on and every account explicitly ISOLATED.

use std::cell::{Cell, RefCell};

use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use ed25519_dalek::{Signer, SigningKey};
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::call::PerpOutput;
use crate::host::PerpHost;
use crate::interface::IPerpDex::{
    addPositionMarginCall, asSubAccountCall, batchPlaceOrdersCall, cancelOrderCall,
    createSubAccountCall, depositInsuranceFundCall, drainDeferredWorkCall, getAccountCall,
    getAccountReturn, getPositionRiskCall, getSymbolConfigCall, internalTransferCall, liquidateCall,
    placeAlgoOrderCall, placeOrderCall, placeOrderSignedCall, registerApiKeyCall,
    removePositionMarginCall, setBasisModeCall, setLeverageCall, setMarginTiersCall,
    setMarginTypeCall, setMarginTypeSignedCall, setUserFeeRatesCall, transferFromPerpCall,
    transferToPerpCall, updateIndexPriceCall, updateMarketCall, withdrawInsuranceFundCall,
    AccountBalanceChanged, AccountPosition, Adl, CrossLiquidationEnded, CrossLiquidationStarted,
    FundingSettled, Liquidation, OrderCancelled, PlaceItem, PositionChanged, Trade,
};
use crate::types::{AccountUpdateReason, MarginTiers, Market};
use crate::{run_perp_dex_call, storage, PERP_DEX_ADDRESS, USDC_ADDRESS};

// ── Fixture ──────────────────────────────────────────────────────────────────────────────────

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;
type Log = primitives::Log;
type Id = [u8; 32];

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
/// The market maker: CROSSED everywhere in gate-on scenarios (never liquidated in P2, so a mark
/// move never sweeps the liquidity provider), ISOLATED in the gated scenarios.
const DAVE: Address = address!("4444444444444444444444444444444444444444");
/// A bystander / keeper / second winner.
const ERIN: Address = address!("5555555555555555555555555555555555555555");
/// Admin = oracle = fee recipient.
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

const M1: u64 = 1;
const M2: u64 = 2;
const M3: u64 = 3;
const MARKETS: [u64; 3] = [M1, M2, M3];
/// Opening marks, whole dollars (price decimals 0).
const MARK0: [u64; 3] = [1_000, 500, 200];

/// One dollar, in quote units (6 dp) — and the value of one contract at a price of 1.
const USD: i128 = 1_000_000;
const SCALE: i128 = USD;
const GAS: u64 = 30_000_000;
const INTERVAL: u64 = 15;
/// $1e9: margin never binds unless a test sets the wallet.
const BIG: i64 = 1_000_000_000 * USD as i64;

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MKT: u8 = 1;
const GTC: u8 = 0;
const RO: u8 = 0b0001;

const CROSSED: u8 = 0;
const ISOLATED: u8 = 1;

/// Fee rates (ppm of notional) for ALICE, BOB, CAROL and ERIN; DAVE and ADMIN trade fee-free.
const TAKER_PPM: u64 = 500;
const MAKER_PPM: i64 = 200;

// Refusal strings — the ABI docs' wording and the report's Binance codes (§1.8, F29).
/// The insufficient-margin refusal of an order on a position CROSSED in force — Binance's `-2019`
/// (the isolated twin is `placeOrder: insufficient perp wallet for margin`).
const E_MARGIN: &str =
    "placeOrder: -2019 Margin is insufficient. The available balance does not cover the order";
const E_J1: &str = "placeOrder: -2019 Margin is insufficient. The open would leave the cross \
                    margin balance below the cross maintenance margin";
const E_J6: &str = "placeOrder: -2019 Margin is insufficient. An ISOLATED position is funded from \
                    the wallet balance, never from cross unrealized profit";
const E_2023: &str = "placeOrder: -2023 User in liquidation mode now. Only reduce-only and \
                      closePosition orders are accepted";
const E_2023_MT: &str = "setMarginType: -2023 User in liquidation mode now.";
const E_2028: &str =
    "setLeverage: -2028 Leverage is smaller than permitted: insufficient margin balance.";
const E_ISO_LEV: &str = "setLeverage: cannot reduce leverage with open position";
const E_4049_ADD: &str = "addPositionMargin: -4049 Add margin only support for isolated position.";
const E_4049_REMOVE: &str =
    "removePositionMargin: -4049 Remove margin only support for isolated position.";
const E_4050: &str = "addPositionMargin: -4050 Cross balance insufficient.";
const E_OUT: &str = "transferFromPerp: insufficient perp wallet balance";
const E_OUT_INTERNAL: &str = "internalTransfer: insufficient perp wallet balance";
const E_OUT_IF: &str = "depositInsuranceFund: insufficient perp wallet balance";
const E_LIQ_CROSS_HEALTHY: &str = "liquidate: account is above cross maintenance margin";
const E_LIQ_NO_POSITION: &str = "liquidate: no open position";
const E_HEALTHY: &str = "liquidate: position is above maintenance margin";

/// Binance's `-2023` batch reason (`PerpBatchReason::UserInLiquidation`).
const BATCH_IN_LIQUIDATION: u8 = 35;
/// `OrderCancelled.reason` 3 = Liquidation.
const CANCEL_LIQUIDATION: u8 = 3;

/// One tier row as `setMarginTiers` takes it (`ppm == 0` = derived `1/(2·lev)`); `lb` in dollars.
#[derive(Clone, Copy)]
struct Band {
    lb: u64,
    lev: u32,
    ppm: u32,
}

/// M1: three derived brackets — 2.5 % / 5 % / 12.5 %.
const M1_TIERS: [Band; 3] = [
    Band { lb: 0, lev: 20, ppm: 0 },
    Band { lb: 50_000, lev: 10, ppm: 0 },
    Band { lb: 200_000, lev: 4, ppm: 0 },
];
/// M2: an explicit column — 5 % then 15 %. The second rate is ABOVE the first bracket's `1/L`, so a
/// leverage-10 position the mark carries past $40k has `MM > PIM` (the J1 case, §3.0).
const M2_TIERS: [Band; 2] =
    [Band { lb: 0, lev: 10, ppm: 50_000 }, Band { lb: 20_000, lev: 5, ppm: 150_000 }];
/// M3: one derived bracket, 10 %.
const M3_TIERS: [Band; 1] = [Band { lb: 0, lev: 5, ppm: 0 }];

fn table(m: u64) -> &'static [Band] {
    match m {
        M1 => &M1_TIERS,
        M2 => &M2_TIERS,
        M3 => &M3_TIERS,
        _ => unreachable!(),
    }
}

thread_local! {
    /// Every account that can hold value — the conservation and model checks run over these.
    static USERS: RefCell<Vec<Address>> = const { RefCell::new(Vec::new()) };
    /// Whether [`ok`] runs the full view comparison (the randomised walk thins it).
    static VIEWS: Cell<bool> = const { Cell::new(true) };
    /// When `Some`, [`attempt`] records every call's outcome (the gated comparison).
    static RECORD: RefCell<Option<Vec<Outcome>>> = const { RefCell::new(None) };
}

/// One call's complete observable outcome: reverted, return / revert bytes, gas, logs.
type Outcome = (bool, Vec<u8>, u64, Vec<Log>);

fn users() -> Vec<Address> {
    USERS.with(|u| u.borrow().clone())
}

/// The gate in force on this thread. The model reads the engine's own switch — the `const`, or a
/// test's `#[cfg(test)]` override — so the two can never disagree about it.
fn gate() -> bool {
    crate::margin_type::cross_margin_enabled()
}


fn make_ctx() -> TestCtx {
    let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
    for a in [USDC_ADDRESS, PERP_DEX_ADDRESS, ADMIN, Address::ZERO] {
        JournalTr::load_account(ctx.journal_mut(), a).unwrap();
    }
    ctx
}

fn the_market(id: u64) -> Market {
    Market {
        market_id: id,
        base_decimals: 0,
        price_decimals: 0,
        tick_size: 1,
        step_size: 1,
        min_quantity: 1,
        max_quantity: 100_000,
        max_price: 1_000_000,
        price_update_interval: INTERVAL,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 0,
        // ≥ 100 %: no fill-time band, so a scenario places liquidity anywhere around the mark.
        price_band_bps: 10_000,
        mark_price: 0,
        tiers: MarginTiers::default(),
        basis_mode: 0,
        deferred_work: 0,
        cumulative_funding_index: 0,
    }
}

/// Three markets with their tier tables set THROUGH THE ABI, fee rates set through the ABI, every
/// account enrolled with a $1e9 perp wallet. Every account's stored margin type is the default
/// (CROSSED), in force under the real gate.
fn fresh() -> TestCtx {
    USERS.with(|u| u.borrow_mut().clear());
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    for (i, m) in MARKETS.into_iter().enumerate() {
        storage::save_market(&mut ctx, &the_market(m)).unwrap();
        storage::save_mark_price(&mut ctx, m, MARK0[i]).unwrap();
    }
    for u in [ALICE, BOB, CAROL, DAVE, ERIN, ADMIN] {
        enroll(&mut ctx, u, BIG);
    }
    // A funded insurance fund: an isolated liquidation that closes past bankruptcy (the band is
    // off here) is absorbed rather than left unbacked, so the identity closes on every path.
    storage::save_insurance_fund(&mut ctx, BIG as u64).unwrap();
    for m in MARKETS {
        let t = table(m);
        ok(
            &mut ctx,
            ADMIN,
            &setMarginTiersCall {
                marketId: m,
                lowerBounds: t.iter().map(|b| b.lb * USD as u64).collect(),
                maxLeverages: t.iter().map(|b| b.lev).collect(),
                maintMarginRatesPpm: t.iter().map(|b| b.ppm).collect(),
                notionalCap: 0,
            }
            .abi_encode(),
        );
    }
    for u in [ALICE, BOB, CAROL, ERIN] {
        ok(
            &mut ctx,
            ADMIN,
            &setUserFeeRatesCall { user: u, makerFeePpm: MAKER_PPM, takerFeePpm: TAKER_PPM }
                .abi_encode(),
        );
    }
    ctx
}

fn enroll(ctx: &mut TestCtx, u: Address, w: i64) {
    JournalTr::load_account(ctx.journal_mut(), u).unwrap();
    set_wallet(ctx, u, w);
    USERS.with(|us| {
        if !us.borrow().contains(&u) {
            us.borrow_mut().push(u);
        }
    });
}

/// Fixture only: overwrite `u`'s perp wallet (the boundary tests put it exactly at a predicate's
/// edge). Not a call — the conservation check of the NEXT call starts from the new value.
fn set_wallet(ctx: &mut TestCtx, u: Address, w: i64) {
    let mut a = storage::load_account(ctx, u).unwrap();
    a.perp_wallet_balance = w;
    storage::save_account(ctx, u, a, AccountUpdateReason::Adjustment).unwrap();
    let _ = take_logs(ctx);
}

/// Fixture only: `u`'s spot (`usdcBalance`).
fn set_spot(ctx: &mut TestCtx, u: Address, spot: u64) {
    let mut a = storage::load_account(ctx, u).unwrap();
    a.usdc_balance = U256::from(spot).into();
    storage::save_account(ctx, u, a, AccountUpdateReason::Adjustment).unwrap();
    let _ = take_logs(ctx);
}

/// Fixture only: the P3 liquidation flag (the spec's stand-in writer — P2 has no production one).
fn flag(ctx: &mut TestCtx, u: Address, on: bool) {
    storage::set_in_liquidation_for_test(ctx, u, on).unwrap();
    let _ = take_logs(ctx);
}

/// The setup most scenarios use, under the real gate (cross margin as it ships): DAVE CROSSED
/// everywhere (the default), CAROL ISOLATED everywhere, ALICE CROSSED on M1/M2 and ISOLATED on M3,
/// BOB CROSSED on M1/M3 and ISOLATED on M2, ERIN and ADMIN at the default. Leverage: ALICE
/// 20/10/5, BOB 10/5/5, CAROL 10/5/5, ERIN 10/5/5, DAVE 1/1/1.
fn crossed_world() -> TestCtx {
    let mut ctx = fresh();
    for m in MARKETS {
        set_type(&mut ctx, CAROL, m, ISOLATED);
    }
    set_type(&mut ctx, ALICE, M3, ISOLATED);
    set_type(&mut ctx, BOB, M2, ISOLATED);
    for (m, l) in [(M1, 20), (M2, 10), (M3, 5)] {
        set_lev(&mut ctx, ALICE, m, l);
    }
    for u in [BOB, CAROL, ERIN] {
        for (m, l) in [(M1, 10), (M2, 5), (M3, 5)] {
            set_lev(&mut ctx, u, m, l);
        }
    }
    ctx
}

// ── The spec model (§3.2) ───────────────────────────────────────────────────────────────────

fn val(p: u64, q: u64) -> i128 {
    p as i128 * q as i128 * SCALE
}

/// `ppm` of a notional, floored — Binance's commission on this grid (exact: `p·q·ppm`).
fn fee(notional: i128, ppm: u64) -> i128 {
    notional * ppm as i128 / 1_000_000
}

fn ceil_div(n: i128, d: i128) -> i128 {
    assert!(n >= 0 && d > 0);
    (n + d - 1) / d
}

/// The slice form (D10) over the table the fixture set: every bracket the notional reaches adds
/// `⌊width · rate⌋`, `rate = ppm / 10^6`, or `1 / (2 · maxLev)` for a derived (`0`) entry.
fn slice_mm(m: u64, n: i128) -> i128 {
    let t = table(m);
    let n = n.abs();
    let mut mm = 0;
    for (i, b) in t.iter().enumerate() {
        let lo = b.lb as i128 * USD;
        if n <= lo {
            break;
        }
        let hi = t.get(i + 1).map_or(i128::MAX, |x| x.lb as i128 * USD);
        let w = n.min(hi) - lo;
        mm += if b.ppm == 0 { w / (2 * b.lev as i128) } else { w * b.ppm as i128 / 1_000_000 };
    }
    mm
}

/// One market's row of the model.
#[derive(Clone, Copy, Debug, Default)]
struct Leg {
    market: u64,
    /// CROSSED in force: the gate is on and the stored choice is CROSSED.
    cross: bool,
    /// In the account's market index: a position or a resting order.
    active: bool,
    amount: i64,
    vq: i64,
    margin: i64,
    lev: u64,
    cr: i64,
    mark: u64,
    bid: i128,
    ask: i128,
    n: i128,
    /// `uPnL` as the account fold counts it (0 for a cross leg at mark 0).
    upnl: i128,
    pim: i128,
    im: i128,
    oo: i128,
    mm: i128,
    pf: i128,
}

#[derive(Clone, Debug, Default)]
struct Model {
    w: i128,
    spot: i128,
    legs: Vec<Leg>,
    c_pf: i128,
    c_upnl: i128,
    c_pim: i128,
    c_mm: i128,
    oo_all: i128,
    i_oo: i128,
    i_wallet: i128,
    cwb: i128,
    e: i128,
    a: i128,
    x: i128,
}

impl Model {
    fn leg(&self, m: u64) -> &Leg {
        self.legs.iter().find(|l| l.market == m).unwrap()
    }

    /// The liquidation price the row must report: LP1 for a cross leg (the account's other terms as
    /// the effective margin), the isolated search on the stored margin otherwise.
    fn lp(&self, m: u64) -> u64 {
        let l = self.leg(m);
        if l.amount == 0 {
            return 0;
        }
        let eff = if l.cross {
            self.cwb
                + self
                    .legs
                    .iter()
                    .filter(|o| o.cross && o.market != m)
                    .map(|o| o.upnl - o.mm)
                    .sum::<i128>()
        } else {
            l.margin as i128
        };
        lp(m, l.amount, l.vq, eff)
    }

    /// `E < Σ_C MM` — the account-level liquidation condition (§3.2, strict).
    fn below_maintenance(&self) -> bool {
        self.legs.iter().any(|l| l.cross && l.amount != 0) && self.e < self.c_mm
    }
}

/// The mark at which `eff + N(p) + vq < MM(N(p))` flips: for a LONG the greatest liquidatable
/// price, for a SHORT the least; `0` when none exists in the evaluable domain `|N(p)| ≤ i64::MAX −
/// |vq| − |eff|`. A plain bisection of the predicate, written from the condition.
fn lp(m: u64, amount: i64, vq: i64, eff: i128) -> u64 {
    let liq = |p: u64| {
        let n = amount as i128 * p as i128 * SCALE;
        eff + n + (vq as i128) < slice_mm(m, n)
    };
    let cap = i64::MAX as i128 - (vq as i128).abs() - eff.abs();
    let top = (cap / (amount.unsigned_abs() as i128 * SCALE)) as u64;
    if amount > 0 {
        if !liq(1) {
            return 0;
        }
        if liq(top) {
            return top;
        }
        let (mut lo, mut hi) = (1u64, top);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if liq(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    } else {
        if liq(1) {
            return 1;
        }
        if !liq(top) {
            return 0;
        }
        let (mut lo, mut hi) = (1u64, top);
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if liq(mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        hi
    }
}

fn leg_of(ctx: &mut TestCtx, u: Address, m: u64) -> Leg {
    let p = storage::load_position(ctx, u, m).unwrap();
    let mark = storage::load_mark_price(ctx, m).unwrap();
    // The SOURCE of truth for the funding index — the engine prices pending funding off a mirror.
    let index = storage::load_funding_state(ctx, m).unwrap().cumulative_funding_index;
    let mut bid = 0;
    for e in storage::load_buy_orders(ctx, u, m).unwrap() {
        bid += val(e.price, e.amount); // a buy's assuming price is its limit (§1.3, F8)
    }
    let mut ask = 0;
    for e in storage::load_sell_orders(ctx, u, m).unwrap() {
        assert!(e.assuming_price >= e.price, "a sell's assuming price is at least its limit");
        ask += val(e.assuming_price, e.amount);
    }
    let cross = gate() && p.margin_type == CROSSED;
    let n = p.amount as i128 * mark as i128 * SCALE;
    let lev = p.leverage.max(1) as i128;
    let pim = ceil_div(n.abs(), lev);
    let im = ceil_div((n + bid).abs().max((n - ask).abs()), lev);
    Leg {
        market: m,
        cross,
        active: p.amount != 0 || bid != 0 || ask != 0,
        amount: p.amount,
        vq: p.v_quote_balance,
        margin: p.margin,
        lev: lev as u64,
        cr: p.cumulative_realized_pnl,
        mark,
        bid,
        ask,
        n,
        upnl: if cross && mark == 0 { 0 } else { n + p.v_quote_balance as i128 },
        pim,
        im,
        oo: im - pim,
        mm: slice_mm(m, n),
        pf: if p.amount == 0 { 0 } else { -(p.amount as i128) * (index - p.last_funding_index) },
    }
}

fn model(ctx: &mut TestCtx, u: Address) -> Model {
    let acct = storage::load_account(ctx, u).unwrap();
    let spot: U256 = acct.usdc_balance.clone().into();
    let mut md = Model {
        w: acct.perp_wallet_balance as i128,
        spot: spot.to::<u128>() as i128,
        ..Model::default()
    };
    for m in MARKETS {
        let l = leg_of(ctx, u, m);
        md.oo_all += l.oo;
        if l.cross {
            md.c_pf += l.pf;
            md.c_upnl += l.upnl;
            md.c_pim += l.pim;
            md.c_mm += l.mm;
        } else {
            md.i_oo += l.oo;
            md.i_wallet += l.margin as i128;
        }
        md.legs.push(l);
    }
    md.cwb = md.w + md.c_pf;
    md.e = md.cwb + md.c_upnl;
    md.a = md.cwb + md.c_upnl - md.c_pim - md.oo_all;
    // Binance's two terms (§3.2), and the maintenance floor no money out may cross (review fix:
    // without it a withdrawal could take a healthy account under `Σ_C MM` once a leg's MM > PIM).
    md.x = (md.cwb - md.i_oo - md.c_mm).min(md.a).min(md.e - md.i_oo - md.c_mm).max(0);
    md
}

/// One account's custody: spot + W + Σ (margin + vQuote + PF). `with_pf = false` drops the pending
/// funding term (what an oracle tick must leave untouched).
fn custody(ctx: &mut TestCtx, u: Address, with_pf: bool) -> i128 {
    let md = model(ctx, u);
    md.spot
        + md.w
        + md.legs
            .iter()
            .map(|l| l.margin as i128 + l.vq as i128 + if with_pf { l.pf } else { 0 })
            .sum::<i128>()
}

fn total_custody(ctx: &mut TestCtx) -> i128 {
    let mut v = storage::load_insurance_fund(ctx).unwrap() as i128;
    for u in users() {
        v += custody(ctx, u, true);
    }
    v
}

/// Compares every surface with the model for `u`: getAccount's scalars and rows, and
/// getPositionRisk on every market.
fn check_views(ctx: &mut TestCtx, u: Address) {
    let md = model(ctx, u);
    let got = account(ctx, u);
    let t = format!("{u}");
    let i = |v: i64| v as i128;
    let w = |v: u64| v as i128;
    let wallet_total = md.cwb + md.i_wallet;
    let upnl_total: i128 = md.legs.iter().map(|l| l.n + l.vq as i128).sum();
    assert_eq!(got.usdcBalance, U256::from(md.spot as u128), "{t} usdcBalance");
    assert_eq!(i(got.totalCrossWalletBalance), md.cwb, "{t} totalCrossWalletBalance = cWB");
    assert_eq!(i(got.totalWalletBalance), wallet_total, "{t} totalWalletBalance = cWB + Σ iw");
    assert_eq!(i(got.totalUnrealizedProfit), upnl_total, "{t} totalUnrealizedProfit");
    assert_eq!(i(got.totalMarginBalance), wallet_total + upnl_total, "{t} totalMarginBalance");
    let sum = |f: fn(&Leg) -> i128| md.legs.iter().map(f).sum::<i128>();
    assert_eq!(w(got.totalInitialMargin), sum(|l| l.im), "{t} totalInitialMargin");
    assert_eq!(w(got.totalPositionInitialMargin), sum(|l| l.pim), "{t} totalPIM");
    assert_eq!(w(got.totalOpenOrderInitialMargin), md.oo_all, "{t} totalOpenOrderIM");
    assert_eq!(w(got.totalMaintMargin), sum(|l| l.mm), "{t} totalMaintMargin");
    assert_eq!(i(got.availableBalance), md.a, "{t} availableBalance = A");
    assert_eq!(i(got.totalCrossUnPnl), md.c_upnl, "{t} totalCrossUnPnl = Σ_C uPnL");
    assert_eq!(w(got.maxWithdrawAmount), md.x, "{t} maxWithdrawAmount = X");
    assert_eq!(i(got.crossMarginBalance), md.e, "{t} crossMarginBalance = E");
    assert_eq!(w(got.totalCrossMaintMargin), md.c_mm, "{t} totalCrossMaintMargin = Σ_C MM");
    let active: Vec<Leg> = md.legs.iter().copied().filter(|l| l.active).collect();
    let ids: Vec<u64> = got.positions.iter().map(|r| r.marketId).collect();
    assert_eq!(ids, active.iter().map(|l| l.market).collect::<Vec<_>>(), "{t} row set");
    for (row, l) in got.positions.iter().zip(&active) {
        check_row(row, l, md.lp(l.market), &format!("{t} getAccount row {}", l.market));
    }
    for m in MARKETS {
        let row = risk(ctx, u, m);
        check_row(&row, md.leg(m), md.lp(m), &format!("{t} getPositionRisk {m}"));
    }
}

fn check_row(r: &AccountPosition, l: &Leg, lp: u64, t: &str) {
    let entry =
        if l.amount == 0 { 0 } else { (-(l.vq as i128) / (l.amount as i128 * SCALE)) as u64 };
    let upnl = l.n + l.vq as i128;
    assert_eq!(r.marketId, l.market, "{t} marketId");
    assert_eq!(r.markPrice, l.mark, "{t} markPrice");
    assert_eq!((r.positionAmt, r.vQuoteBalance), (l.amount, l.vq), "{t} position");
    assert_eq!(r.leverage, l.lev, "{t} leverage");
    assert_eq!((r.bidNotional as i128, r.askNotional as i128), (l.bid, l.ask), "{t} Bid/Ask");
    assert_eq!(r.entryPrice, entry, "{t} entryPrice");
    assert_eq!(r.notional as i128, l.n.abs(), "{t} notional");
    assert_eq!(r.unrealizedProfit as i128, upnl, "{t} unrealizedProfit");
    let (iso_margin, iso_wallet) =
        if l.cross { (0, 0) } else { (l.margin as i128 + upnl, l.margin as i128) };
    assert_eq!(r.isolatedMargin as i128, iso_margin, "{t} isolatedMargin");
    assert_eq!(r.isolatedWallet as i128, iso_wallet, "{t} isolatedWallet");
    assert_eq!(r.positionInitialMargin as i128, l.pim, "{t} positionInitialMargin");
    assert_eq!(r.initialMargin as i128, l.im, "{t} initialMargin");
    assert_eq!(r.openOrderInitialMargin as i128, l.oo, "{t} openOrderInitialMargin");
    assert_eq!(r.maintMargin as i128, l.mm, "{t} maintMargin");
    assert_eq!(r.liquidationPrice, lp, "{t} liquidationPrice");
    assert_eq!(r.cumulativeRealizedPnl, l.cr, "{t} cumulativeRealizedPnl");
    assert_eq!(r.marginType, if l.cross { CROSSED } else { ISOLATED }, "{t} marginType");
}

/// Stored invariants of P2: the open-position count, and `CROSSED ⇒ margin == 0`.
fn check_invariants(ctx: &mut TestCtx, u: Address) {
    let md = model(ctx, u);
    let open = md.legs.iter().filter(|l| l.amount != 0).count() as u8;
    let acct = storage::load_account(ctx, u).unwrap();
    assert_eq!(acct.open_position_count, open, "{u} open_position_count");
    for l in &md.legs {
        if l.cross {
            assert_eq!(l.margin, 0, "{u} CROSSED position on {} holds margin", l.market);
        }
    }
}

fn check_all(ctx: &mut TestCtx) {
    for m in MARKETS {
        let mut s: i128 = 0;
        for u in users() {
            s += storage::load_position(ctx, u, m).unwrap().amount as i128;
        }
        assert_eq!(s, 0, "Σ amount on market {m}");
    }
    for u in users() {
        check_invariants(ctx, u);
        if VIEWS.with(Cell::get) {
            check_views(ctx, u);
        }
    }
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

fn run(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap()
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

fn is<E: SolEvent>(log: &Log) -> bool {
    log.data.topics().first() == Some(&E::SIGNATURE_HASH)
}

fn decode<E: SolEvent>(log: &Log) -> E {
    E::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
}

/// Each account's trade cash and fees in `logs`: a buyer pays `p·q`, a seller receives it, the
/// taker pays `takerFee`, the maker `makerFee` (a rebate when negative), the fee recipient (ADMIN)
/// receives both.
fn trade_flows(logs: &[Log], u: Address) -> i128 {
    let mut f = 0;
    for t in evs::<Trade>(logs) {
        let cash = val(t.price, t.quantity);
        let taker_buys = t.takerSide == BUY;
        if t.taker == u {
            f += if taker_buys { -cash } else { cash };
            f -= t.takerFee as i128;
        }
        if t.maker == u {
            f += if taker_buys { cash } else { -cash };
            f -= t.makerFee as i128;
        }
        if u == ADMIN {
            f += t.takerFee as i128 + t.makerFee as i128;
        }
    }
    f
}

fn selector_of(input: &[u8]) -> [u8; 4] {
    input[..4].try_into().unwrap()
}

struct Call {
    out: PerpOutput,
    logs: Vec<Log>,
}

/// Runs one shell call and checks it whichever way it goes.
///
/// REFUSED: zero perp writes, no log, a bit-identical world — returns the reason.
///
/// ACCEPTED: the custody identity (to the unit, pending funding included); the insurance fund moves
/// only on a liquidation path or an insurance-fund call; every account's own custody moves by
/// exactly its trade cash and fees (an oracle tick: by nothing but pending funding) unless a
/// liquidation ran; then [`check_all`] — `Σ amount == 0`, the stored invariants and the model
/// against every surface.
fn attempt(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Result<Call, String> {
    let _ = take_logs(ctx);
    let world_before = world(ctx);
    let writes = ctx.perp_write_count();
    let before = total_custody(ctx);
    let if_before = storage::load_insurance_fund(ctx).unwrap();
    let per_user: Vec<(Address, i128, i128)> =
        users().into_iter().map(|u| (u, custody(ctx, u, true), custody(ctx, u, false))).collect();
    let out = run(ctx, caller, input);
    let logs = take_logs(ctx);
    RECORD.with(|r| {
        if let Some(v) = r.borrow_mut().as_mut() {
            v.push((out.reverted, out.bytes.to_vec(), out.gas_used, logs.clone()));
        }
    });
    if out.reverted {
        let reason = revert_reason(&out);
        assert_eq!(ctx.perp_write_count() - writes, 0, "[{reason}] the refused call wrote");
        assert!(logs.is_empty(), "[{reason}] the refused call logged");
        assert!(world(ctx) == world_before, "[{reason}] the refused call changed stored state");
        return Err(reason);
    }
    let after = total_custody(ctx);
    assert_eq!(
        after,
        before,
        "custody not conserved (Σ spot + W + margin + vQ + PF + IF): caller {caller} selector {:?}",
        selector_of(input)
    );

    let sel = selector_of(input);
    let liquidation_path = sel == updateIndexPriceCall::SELECTOR
        || sel == liquidateCall::SELECTOR
        || sel == drainDeferredWorkCall::SELECTOR;
    let if_call =
        sel == depositInsuranceFundCall::SELECTOR || sel == withdrawInsuranceFundCall::SELECTOR;
    let if_after = storage::load_insurance_fund(ctx).unwrap();
    if !liquidation_path && !if_call {
        assert_eq!(if_after, if_before, "the insurance fund moved on an ordinary call");
    }
    let liquidated = !evs::<Liquidation>(&logs).is_empty() || !evs::<Adl>(&logs).is_empty();
    if !liquidated {
        let internal = evs::<crate::interface::IPerpDex::InternalTransfer>(&logs);
        for (u, with_pf, without_pf) in per_user {
            if sel == updateIndexPriceCall::SELECTOR {
                assert_eq!(custody(ctx, u, false), without_pf, "{u}: an oracle tick moved custody");
                continue;
            }
            let mut flow = trade_flows(&logs, u);
            for t in &internal {
                if t.from == u {
                    flow -= t.amount as i128;
                }
                if t.to == u {
                    flow += t.amount as i128;
                }
            }
            if if_call && u == caller {
                flow -= if_after as i128 - if_before as i128;
            }
            assert_eq!(
                custody(ctx, u, true) - with_pf,
                flow,
                "{u}: custody moved by other than its own trade cash and fees"
            );
        }
    }
    check_all(ctx);
    Ok(Call { out, logs })
}

/// One SUCCESSFUL call, every [`attempt`] check passed.
fn ok(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Call {
    attempt(ctx, caller, input).unwrap_or_else(|e| panic!("unexpected revert: {e}"))
}

/// One REFUSED call: the exact reason, zero perp writes, no log, a bit-identical world.
fn rejected(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
    match attempt(ctx, caller, input) {
        Ok(_) => panic!("expected the revert `{expect}`, but the call SUCCEEDED"),
        Err(e) => assert_eq!(e, expect),
    }
}

fn enc<T: serde::Serialize>(v: &T) -> Vec<u8> {
    perp_core::codec::encode(v).unwrap()
}

/// Everything a refused call must leave bit-identical. `normalize_type` blanks the stored margin
/// type byte (the gated comparison stores different bytes for the same type in force).
fn world_of(ctx: &mut TestCtx, normalize_type: bool) -> Vec<Vec<u8>> {
    let mut w = Vec::new();
    for u in users() {
        w.push(enc(&storage::load_account(ctx, u).unwrap()));
        for m in MARKETS {
            let mut p = storage::load_position(ctx, u, m).unwrap();
            if normalize_type {
                p.margin_type = 0;
            }
            w.push(enc(&p));
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
        w.push(enc(&storage::load_funding_state(ctx, m).unwrap()));
    }
    w.push(enc(&storage::load_insurance_fund(ctx).unwrap()));
    w
}

fn world(ctx: &mut TestCtx) -> Vec<Vec<u8>> {
    world_of(ctx, false)
}

/// A signed call refused AFTER its signature burn: the exact reason, writes (the burn) but no log
/// and no business state moved.
fn rejected_after_burn(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
    let _ = take_logs(ctx);
    let before = world(ctx);
    let writes = ctx.perp_write_count();
    let out = run(ctx, caller, input);
    assert!(out.reverted, "expected the revert `{expect}`, but the call SUCCEEDED");
    assert_eq!(revert_reason(&out), expect);
    assert!(ctx.perp_write_count() > writes, "[{expect}] the signature was not burned");
    assert!(take_logs(ctx).is_empty(), "[{expect}] the refused call logged");
    assert!(world(ctx) == before, "[{expect}] the refused call moved business state");
}

// ── Calldata and shortcuts ───────────────────────────────────────────────────────────────────

fn order(m: u64, side: u8, price: u64, qty: u64, kind: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: m,
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

fn mkt(m: u64, side: u8, qty: u64) -> Vec<u8> {
    order(m, side, 0, qty, MKT, 0)
}

fn set_type(ctx: &mut TestCtx, u: Address, m: u64, t: u8) {
    ok(ctx, u, &setMarginTypeCall { marketId: m, marginType: t }.abi_encode());
}

fn set_lev(ctx: &mut TestCtx, u: Address, m: u64, l: u64) {
    ok(ctx, u, &setLeverageCall { marketId: m, leverage: l }.abi_encode());
}

/// A GTC limit that must rest untouched; returns its id.
fn rest(ctx: &mut TestCtx, u: Address, m: u64, side: u8, price: u64, qty: u64) -> Id {
    let c = ok(ctx, u, &order(m, side, price, qty, LIMIT, 0));
    assert!(evs::<Trade>(&c.logs).is_empty(), "{u} {side}@{price} traded while resting");
    placeOrderCall::abi_decode_returns(&c.out.bytes).unwrap().0
}

/// `taker` trades `qty` (signed: + buys, − sells) at `price` by market order against liquidity DAVE
/// rests for it there.
fn fill(ctx: &mut TestCtx, taker: Address, m: u64, qty: i64, price: u64) -> Call {
    let (maker_side, taker_side) = if qty > 0 { (SELL, BUY) } else { (BUY, SELL) };
    rest(ctx, DAVE, m, maker_side, price, qty.unsigned_abs());
    let c = ok(ctx, taker, &mkt(m, taker_side, qty.unsigned_abs()));
    let t = evs::<Trade>(&c.logs);
    assert_eq!(t.iter().map(|t| t.quantity).sum::<u64>(), qty.unsigned_abs(), "{taker} filled");
    assert!(t.iter().all(|t| t.price == price && t.taker == taker && t.maker == DAVE));
    c
}

/// `maker` rests `qty` (signed) at `price` and DAVE takes it by market order. Returns DAVE's call.
fn make(ctx: &mut TestCtx, maker: Address, m: u64, qty: i64, price: u64) -> Call {
    let (maker_side, taker_side) = if qty > 0 { (BUY, SELL) } else { (SELL, BUY) };
    rest(ctx, maker, m, maker_side, price, qty.unsigned_abs());
    let c = ok(ctx, DAVE, &mkt(m, taker_side, qty.unsigned_abs()));
    let t = evs::<Trade>(&c.logs);
    assert_eq!(t.iter().map(|t| t.quantity).sum::<u64>(), qty.unsigned_abs(), "{maker} made");
    assert!(t.iter().all(|t| t.price == price && t.maker == maker));
    c
}

/// One oracle tick on `m` publishing `index` (last traded cleared first, so the contract-price leg
/// sits on the index). Returns the call and the new mark.
fn tick(ctx: &mut TestCtx, m: u64, index: u64) -> Call {
    storage::save_last_traded_price(ctx, m, 0).unwrap();
    let ts = storage::load_index_price_state(ctx, m).unwrap().timestamp + INTERVAL;
    let input = updateIndexPriceCall { marketId: m, indexPrice: index, timestamp: ts }.abi_encode();
    ok(ctx, ADMIN, &input)
}

/// Walks `m`'s index to `to` one tick at a time, `step` per tick.
fn walk_mark(ctx: &mut TestCtx, m: u64, to: u64, step: u64) {
    let mut cur = storage::load_mark_price(ctx, m).unwrap();
    while cur != to {
        cur = if cur < to { (cur + step).min(to) } else { cur.saturating_sub(step).max(to) };
        tick(ctx, m, cur);
    }
}

fn mark(ctx: &mut TestCtx, m: u64) -> u64 {
    storage::load_mark_price(ctx, m).unwrap()
}

/// Turns funding on for `m`: one epoch per oracle tick at `interest` ppm (updateMarket, admin).
fn enable_funding(ctx: &mut TestCtx, m: u64, interest: i64) {
    let mk = the_market(m);
    ok(
        ctx,
        ADMIN,
        &updateMarketCall {
            marketId: m,
            tickSize: mk.tick_size,
            stepSize: mk.step_size,
            minQuantity: mk.min_quantity,
            maxQuantity: mk.max_quantity,
            maxPrice: mk.max_price,
            priceUpdateInterval: INTERVAL,
            active: true,
            fundingInterval: INTERVAL,
            interestRate: interest,
            liquidationFeeRateBps: 0,
            priceBandBps: mk.price_band_bps,
        }
        .abi_encode(),
    );
}

fn risk(ctx: &mut TestCtx, u: Address, m: u64) -> AccountPosition {
    let out = view(ctx, &getPositionRiskCall { user: u, marketId: m }.abi_encode());
    assert!(!out.reverted, "getPositionRisk reverted: {}", revert_reason(&out));
    getPositionRiskCall::abi_decode_returns(&out.bytes).unwrap()
}

fn account(ctx: &mut TestCtx, u: Address) -> getAccountReturn {
    let out = view(ctx, &getAccountCall { user: u }.abi_encode());
    assert!(!out.reverted, "getAccount reverted: {}", revert_reason(&out));
    getAccountCall::abi_decode_returns(&out.bytes).unwrap()
}

fn pos(ctx: &mut TestCtx, u: Address, m: u64) -> crate::types::PerpPosition {
    storage::load_position(ctx, u, m).unwrap()
}

fn wallet(ctx: &mut TestCtx, u: Address) -> i128 {
    storage::load_account(ctx, u).unwrap().perp_wallet_balance as i128
}

fn insurance(ctx: &mut TestCtx) -> i128 {
    storage::load_insurance_fund(ctx).unwrap() as i128
}

/// Cancels every resting order `u` holds on `m`.
fn cancel_all(ctx: &mut TestCtx, u: Address, m: u64) {
    let ids: Vec<Id> = storage::load_buy_orders(ctx, u, m)
        .unwrap()
        .iter()
        .chain(storage::load_sell_orders(ctx, u, m).unwrap().iter())
        .map(|e| e.order_id)
        .collect();
    for id in ids {
        ok(ctx, u, &cancelOrderCall { orderId: id.into(), marketId: m }.abi_encode());
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// The model itself
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// The spec models reproduce the report's numbers before they are trusted against the engine:
/// the slice MM on each table, the bracket rates, and Binance's LP1 closed form on a two-market
/// cross account (§1.5:
/// `LP1 = (WB − TMM1 + UPNL1 + cumB − Side1·Pos1·EP1) / (Pos1·MMR_B − Side1·Pos1)`).
#[test]
fn model_reproduces_the_reports_formulas() {
    // M1 (2.5 / 5 / 12.5 %): $60k = 50k·2.5 % + 10k·5 % = 1,250 + 500.
    assert_eq!(slice_mm(M1, 60_000 * USD), 1_750 * USD);
    // M2 explicit (5 % / 15 %), cum 2,000: MM = N·15 % − 2,000 above $20k.
    assert_eq!(slice_mm(M2, 42_900 * USD), (42_900 * 15 / 100 - 2_000) * USD);
    assert_eq!(slice_mm(M3, 10_000 * USD), 1_000 * USD);
    // LP1 on a long of 10 M1 entered at 1,000 with WB = 2,000, another cross leg with uPnL −300
    // and MM 100: inside tier 0 (MMR 2.5 %, cum 0) the closed form is
    // (2,000 − 100 − 300 + 0 − 10·1,000) / (10·0.025 − 10) = 8,400 / 9.75 = 861.53… → the greatest
    // liquidatable whole price is 861.
    let eff = (2_000 - 300 - 100) * USD;
    assert_eq!(lp(M1, 10, -10_000 * USD as i64, eff), 861);
    // The short mirror: (WB − TMM + UPNL + cum + Pos·EP) / (Pos·MMR + Pos) = 11,600 / 10.25 =
    // 1,131.7 → the least liquidatable whole price is 1,132.
    assert_eq!(lp(M1, -10, 10_000 * USD as i64, eff), 1_132);
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Walks
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// A scripted multi-market walk, the model compared with every surface after every call: cross
/// and isolated legs on one account, taker and maker fills with fees, marks moving one market at a
/// time with funding epochs on two markets, a partial close, a flip, an M1 position the mark
/// carries into its second bracket, resting orders on both sides, cancels, cross leverage moves, a
/// negative W backed by cross profit, and a final close.
#[test]
fn walk_scripted_three_markets_mixed_cross_and_isolated() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    set_wallet(ctx, ALICE, (50_000 * USD) as i64);
    set_wallet(ctx, BOB, (20_000 * USD) as i64);
    set_wallet(ctx, CAROL, (20_000 * USD) as i64);
    enable_funding(ctx, M1, 300);
    enable_funding(ctx, M2, -200);

    fill(ctx, ALICE, M1, 20, 1_000); // cross long, taker
    make(ctx, ALICE, M2, -30, 500); // cross short, maker
    fill(ctx, ALICE, M3, 50, 200); // isolated long, taker — the silo is funded from W
    make(ctx, BOB, M1, 10, 999); // cross long, maker
    fill(ctx, CAROL, M1, 8, 1_001); // isolated long, taker
    fill(ctx, BOB, M3, 20, 201); // cross long, taker
    make(ctx, CAROL, M2, -15, 499); // isolated short, maker

    // Marks, one market per tick. A funding market's first tick opens its epoch; every later one
    // folds an epoch into the index, pending until each position's next touch.
    tick(ctx, M1, 1_010);
    tick(ctx, M2, 505);
    tick(ctx, M1, 1_020);
    tick(ctx, M3, 198);
    tick(ctx, M2, 510);
    tick(ctx, M1, 1_025);
    assert!(model(ctx, ALICE).c_pf != 0, "funding is pending on ALICE's cross legs");

    // A partial close at a profit: funding then realized PnL land in W, net of the fee; the
    // position holds no margin before or after.
    let w0 = wallet(ctx, ALICE);
    let cr0 = pos(ctx, ALICE, M1).cumulative_realized_pnl;
    let c = fill(ctx, ALICE, M1, -8, 1_030);
    let settled: i128 = evs::<FundingSettled>(&c.logs)
        .iter()
        .filter(|f| f.user == ALICE)
        .map(|f| f.amount as i128)
        .sum();
    assert!(settled != 0, "the touch settled ALICE's M1 funding");
    let realized = (pos(ctx, ALICE, M1).cumulative_realized_pnl - cr0) as i128;
    assert_eq!(realized, 8 * 30 * USD, "8 contracts closed 30 above the 1,000 entry");
    assert_eq!(
        wallet(ctx, ALICE) - w0,
        settled + realized - fee(val(1_030, 8), TAKER_PPM),
        "ΔW = funding + realized − fee"
    );

    // A flip in one fill: short 30 → long 30.
    fill(ctx, ALICE, M2, 60, 510);
    assert_eq!(pos(ctx, ALICE, M2).amount, 30);

    // Into M1's second bracket: 37 contracts walked from 1,025 to 1,400 one tick at a time.
    fill(ctx, ALICE, M1, 25, 1_025);
    walk_mark(ctx, M1, 1_400, 75);
    assert!(model(ctx, ALICE).leg(M1).n > 50_000 * USD, "the M1 leg crossed into bracket 2");

    // Resting orders on both sides of a cross long (ooIM is the joint max), then cancelled.
    rest(ctx, BOB, M1, SELL, 1_450, 5);
    rest(ctx, BOB, M1, BUY, 1_200, 3);
    rest(ctx, ALICE, M2, SELL, 560, 4);
    cancel_all(ctx, BOB, M1);

    // A partial maker fill on an isolated leg, then the remainder cancelled.
    rest(ctx, CAROL, M3, SELL, 205, 10);
    ok(ctx, DAVE, &mkt(M3, BUY, 4));
    cancel_all(ctx, CAROL, M3);

    // Cross leverage lowered where A covers the added IM; raised back where the bracket allows.
    set_lev(ctx, ALICE, M1, 10);
    set_lev(ctx, ALICE, M2, 5);
    set_lev(ctx, ALICE, M2, 10);
    cancel_all(ctx, ALICE, M2);

    // W negative, backed by cross profit: all but $100 of X leaves, then an M2 loss lands in W.
    let x = model(ctx, ALICE).x;
    ok(ctx, ALICE, &transferFromPerpCall { amount: (x - 100 * USD) as u64 }.abi_encode());
    walk_mark(ctx, M2, 380, 40);
    fill(ctx, ALICE, M2, -30, 380); // closes the long 30 at a loss
    let md = model(ctx, ALICE);
    assert!(md.w < 0, "the M2 loss took W negative: {}", md.w);
    assert!(md.a > 0 && md.x == 0, "cross profit backs orders (A > 0) but nothing withdraws");
    rejected(ctx, ALICE, &transferFromPerpCall { amount: 1 }.abi_encode(), E_OUT);
    // …and an opening order is still admitted on A.
    fill(ctx, ALICE, M2, -2, 380);

    // Marks move on, funding keeps accruing, BOB's M1 touch settles his funding into W.
    tick(ctx, M3, 210);
    walk_mark(ctx, M1, 1_300, 50);
    fill(ctx, BOB, M1, -2, 1_300);

    // ALICE's M1 long closed at 1,300; the M3 silo is untouched by all of it.
    let silo = pos(ctx, ALICE, M3).margin;
    fill(ctx, ALICE, M1, -37, 1_300);
    assert_eq!(pos(ctx, ALICE, M1).amount, 0);
    assert_eq!(pos(ctx, ALICE, M3).margin, silo);
}

/// A small deterministic PRNG (splitmix64).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize]
    }
}

/// A randomised mixed walk: three traders with mixed margin types against DAVE's liquidity —
/// market and limit orders, taker and maker closes, cancels, leverage moves both ways, margin
/// moves, money out and back in, oracle ticks (some adverse) with funding epochs of either sign,
/// mode switches, and every hundred steps a stress episode that drains X and realises a loss
/// beyond W. Every call goes through [`attempt`]: accepted under every check (the model against
/// every surface on every third step), or refused write-clean. Where the spec decides a call from
/// the model alone — money out (`X`), `-4049`, `-4050`, a cross leverage move (`A` against `ΔIM`)
/// — the outcome is predicted and asserted. Coverage floors: a negative W and `E < ΣMM` are
/// reached.
#[test]
fn walk_randomised_mixed_accounts_keep_the_model_and_custody() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    for (u, w) in [(ALICE, 6_000), (BOB, 4_000), (CAROL, 4_000)] {
        set_wallet(ctx, u, (w * USD) as i64);
    }
    enable_funding(ctx, M1, 400);
    enable_funding(ctx, M3, -300);
    // Re-seeded (was `0x00c0_ffee_2026_1025`) for P3, whose sweep now liquidates the accounts the
    // walk drives under maintenance and so changes every later random decision.
    let mut rng = Rng(0x00c0_ffee_2026_0018);
    let mut reasons: std::collections::BTreeMap<String, u32> = Default::default();
    let traders = [ALICE, BOB, CAROL];
    let (mut accepted, mut refused, mut predicted) = (0u32, 0u32, 0u32);
    let (mut negative_w, mut below_mm, mut ticks, mut stress) = (0u32, 0u32, 0u32, 0u32);
    let mut cross_episodes = 0u32;
    for step in 0..900u32 {
        VIEWS.with(|v| v.set(step % 3 == 0));
        if step % 100 == 99 {
            // A stress episode: a trader drains X and closes a cross leg as a MAKER 15 % through
            // the mark — the loss lands in W whatever its size (no per-fill bad debt), so W can go
            // negative (money out then refused on X) until a deposit or profit nets it — and only
            // then does the mark follow. (In that order on purpose: with the mark moved first, the
            // tick's sweep would liquidate the drained account before it could close — P3.) The
            // tick that follows judges the account: under cross maintenance, its episode runs.
            let u = rng.pick(&traders);
            if let Some(l) = model(ctx, u).legs.iter().find(|l| l.cross && l.amount != 0).copied() {
                VIEWS.with(|v| v.set(true));
                let x = model(ctx, u).x;
                if x > 0 {
                    ok(ctx, u, &transferFromPerpCall { amount: x as u64 }.abi_encode());
                }
                let mk = mark(ctx, l.market);
                let to = if l.amount > 0 { mk - mk * 15 / 100 } else { mk + mk * 15 / 100 };
                let side = if l.amount > 0 { SELL } else { BUY };
                // Nothing else of `u`'s resting in the market, so the close is provably reducing.
                cancel_all(ctx, u, l.market);
                let a = l.amount.unsigned_abs();
                ok(ctx, u, &order(l.market, side, to, a, LIMIT, RO));
                ok(ctx, DAVE, &mkt(l.market, 1 - side, a));
                assert_eq!(pos(ctx, u, l.market).amount, 0, "closed");
                let md = model(ctx, u);
                negative_w += (md.w < 0) as u32;
                below_mm += md.below_maintenance() as u32;
                let c = tick(ctx, l.market, to);
                cross_episodes += evs::<CrossLiquidationStarted>(&c.logs).len() as u32;
                stress += 1;
            }
        }
        let u = rng.pick(&traders);
        let m = rng.pick(&MARKETS);
        let mk = mark(ctx, m);
        let input: Vec<u8>;
        let mut expect: Option<Result<(), &str>> = None;
        let mut liquidity = false;
        let mut closing = false;
        let mut caller = u;
        match rng.below(20) {
            0..=4 => {
                // A market order against liquidity DAVE rests a few ticks from the mark.
                let qty = 1 + rng.below(5);
                let side = rng.pick(&[BUY, SELL]);
                let off = rng.below(6);
                let price = if side == BUY { mk + off } else { mk.saturating_sub(off).max(1) };
                ok(ctx, DAVE, &order(m, 1 - side, price, qty, LIMIT, 0));
                liquidity = true;
                input = mkt(m, side, qty);
            }
            5 => {
                // A close of the whole position, realising its PnL: as a reduce-only TAKER against
                // DAVE's liquidity, or as a reduce-only MAKER that DAVE takes (a maker fill is
                // never margin-gated, so this is the path that realises a loss beyond W).
                let a = pos(ctx, u, m).amount;
                if a == 0 {
                    continue;
                }
                let side = if a > 0 { SELL } else { BUY };
                let off = rng.below(6);
                let price = if side == BUY { mk + off } else { mk.saturating_sub(off).max(1) };
                if rng.below(2) == 0 {
                    ok(ctx, DAVE, &order(m, 1 - side, price, a.unsigned_abs(), LIMIT, 0));
                    liquidity = true;
                    closing = true;
                    input = order(m, side, 0, a.unsigned_abs(), MKT, RO);
                } else {
                    let close = order(m, side, price, a.unsigned_abs(), LIMIT, RO);
                    if attempt(ctx, u, &close).is_err() {
                        continue;
                    }
                    caller = DAVE;
                    input = mkt(m, 1 - side, a.unsigned_abs());
                }
            }
            6 => {
                // A limit away from the mark: it rests, or is refused on A.
                let side = rng.pick(&[BUY, SELL]);
                let off = 5 + rng.below(30);
                let price = if side == BUY { mk.saturating_sub(off).max(1) } else { mk + off };
                input = order(m, side, price, 1 + rng.below(2), LIMIT, 0);
            }
            7..=8 => {
                let mut ids: Vec<Id> = storage::load_buy_orders(ctx, u, m)
                    .unwrap()
                    .iter()
                    .chain(storage::load_sell_orders(ctx, u, m).unwrap().iter())
                    .map(|e| e.order_id)
                    .collect();
                if ids.is_empty() {
                    continue;
                }
                let id = ids.swap_remove(rng.below(ids.len() as u64) as usize);
                input = cancelOrderCall { orderId: id.into(), marketId: m }.abi_encode();
            }
            9 => {
                // A leverage move; a CROSSED leg is decided from A and ΔIM (§3.3 item 7).
                let l = 1 + rng.below(table(m)[0].lev as u64);
                input = setLeverageCall { marketId: m, leverage: l }.abi_encode();
                let md = model(ctx, u);
                let leg = *md.leg(m);
                let cap =
                    table(m).iter().rev().find(|b| b.lb as i128 * USD <= leg.n.abs()).unwrap();
                if leg.cross && l <= cap.lev as u64 {
                    let im =
                        ceil_div((leg.n + leg.bid).abs().max((leg.n - leg.ask).abs()), l as i128);
                    let delta = im - leg.im;
                    expect = Some(if delta <= 0 || md.a >= delta { Ok(()) } else { Err(E_2028) });
                }
            }
            10..=11 => {
                // Money out, decided by X (§3.3 item 9): around X, either side of it.
                let md = model(ctx, u);
                let amount = match rng.below(3) {
                    0 => md.x,
                    1 => md.x + 1,
                    _ => (1 + rng.below(1_500) as i128) * USD,
                }
                .max(1);
                input = transferFromPerpCall { amount: amount as u64 }.abi_encode();
                expect = Some(if md.x >= amount { Ok(()) } else { Err(E_OUT) });
            }
            13 => {
                // addPositionMargin: -4049 on a CROSSED market, else X decides (-4050).
                let md = model(ctx, u);
                let leg = *md.leg(m);
                let amount = (1 + rng.below(500) as i128) * USD;
                input = addPositionMarginCall { marketId: m, amount: amount as u64 }.abi_encode();
                expect = Some(if leg.cross {
                    Err(E_4049_ADD)
                } else if leg.amount == 0 {
                    Err("addPositionMargin: no open position")
                } else if md.x >= amount {
                    Ok(())
                } else {
                    Err(E_4050)
                });
            }
            14 => {
                // removePositionMargin: -4049 on a CROSSED market; the isolated rule is unchanged.
                let leg = *model(ctx, u).leg(m);
                let amount = (1 + rng.below(50)) * USD as u64;
                input = removePositionMarginCall { marketId: m, amount }.abi_encode();
                if leg.cross {
                    expect = Some(Err(E_4049_REMOVE));
                }
            }
            12 => {
                // Money back in from spot: a deposit nets against a negative W first.
                let spot = model(ctx, u).spot;
                if spot == 0 {
                    continue;
                }
                let amount = 1 + rng.below(spot as u64);
                input = transferToPerpCall { amount }.abi_encode();
                expect = Some(Ok(()));
            }
            15..=18 => {
                // One market's index moves 0.2–5 %, either way (a funding epoch on M1 / M3).
                // One time in three it is an ADVERSE move of 4–10 % against `u`'s cross leg on `m`,
                // which is what takes accounts under maintenance and, once realised, W negative.
                let a = pos(ctx, u, m).amount;
                let adverse = a != 0 && model(ctx, u).leg(m).cross && rng.below(3) == 0;
                let bp = if adverse { 400 + rng.below(600) } else { 20 + rng.below(480) };
                let d = (mk as u128 * bp as u128 / 10_000).max(1) as u64;
                let up = if adverse { a < 0 } else { rng.below(2) == 0 };
                let to = if up { mk + d } else { mk - d.min(mk / 2) };
                let c = tick(ctx, m, to);
                cross_episodes += evs::<CrossLiquidationStarted>(&c.logs).len() as u32;
                ticks += 1;
                continue;
            }
            _ => {
                // A mode switch: legal only flat with no orders (P1) — refusals are expected.
                let t = if pos(ctx, u, m).margin_type == CROSSED { ISOLATED } else { CROSSED };
                input = setMarginTypeCall { marketId: m, marginType: t }.abi_encode();
            }
        }
        let got = attempt(ctx, caller, &input).map(|_| ());
        match (&got, expect) {
            (Ok(()), Some(Err(e))) => panic!("step {step}: {u} accepted; the spec refuses `{e}`"),
            (Err(g), Some(Ok(()))) => panic!("step {step}: {u} refused `{g}`; the spec admits"),
            (Err(g), Some(Err(e))) => assert_eq!(g, e, "step {step}: wrong refusal"),
            _ => {}
        }
        predicted += expect.is_some() as u32;
        match got {
            Ok(()) => accepted += 1,
            Err(e) => {
                refused += 1;
                let key = if closing { format!("[close] {e}") } else { e };
                *reasons.entry(key).or_default() += 1;
            }
        }
        if liquidity {
            cancel_all(ctx, DAVE, m);
        }
        for t in traders {
            let md = model(ctx, t);
            negative_w += (md.w < 0) as u32;
            below_mm += md.below_maintenance() as u32;
        }
    }
    VIEWS.with(|v| v.set(true));
    check_all(ctx);
    eprintln!(
        "random walk: accepted {accepted}, refused {refused}, predicted {predicted}, \
         ticks {ticks}, \
         negative-W account-steps {negative_w}, below-maintenance account-steps {below_mm}, \
         stress episodes {stress}, cross liquidation episodes {cross_episodes}\n\
         refusals {reasons:#?}"
    );
    assert!(accepted > 250 && refused > 100 && predicted > 150 && ticks > 100, "coverage");
    assert!(negative_w > 0 && below_mm > 0 && stress > 4, "no negative W or E < ΣMM reached");
    assert!(cross_episodes > 0, "no cross liquidation episode ran");
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §3.2 identities
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// For an account with no position CROSSED in force every cross term is 0 and `A`, `X` are today's
/// isolated basis: `E = cWB = W`, `A = W − Σ ooIM`, `X = max(0, A)` (§3.2, last line).
#[test]
fn f_an_isolated_only_account_reads_the_isolated_basis() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, CAROL, M1, 10, 1_000);
    rest(ctx, CAROL, M1, BUY, 900, 2);
    make(ctx, CAROL, M2, -4, 500);
    tick(ctx, M1, 1_050);
    let md = model(ctx, CAROL);
    assert!(md.legs.iter().all(|l| !l.cross));
    assert_eq!((md.c_pf, md.c_upnl, md.c_pim, md.c_mm), (0, 0, 0, 0));
    assert_eq!((md.cwb, md.e), (md.w, md.w));
    assert_eq!(md.a, md.w - md.oo_all);
    assert_eq!(md.x, md.a.max(0));
    let got = account(ctx, CAROL);
    assert_eq!((got.totalCrossUnPnl, got.totalCrossMaintMargin), (0, 0));
    assert_eq!(got.crossMarginBalance as i128, md.w);
    assert_eq!(got.maxWithdrawAmount as i128, (got.availableBalance as i128).max(0));
}

/// Cross unrealized profit opens but never withdraws (§1.2, F2–F4): with $1,000 of cross profit and
/// a $300 wallet, `A = 300 + 1,000 − PIM 550 = 750` admits an open, while
/// `X = min(cWB − ΣMM, A) = min(300 − 275, 750) = 25` is all that leaves.
#[test]
fn f_cross_profit_backs_orders_but_does_not_withdraw() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, ALICE, M1, 10, 1_000);
    tick(ctx, M1, 1_100);
    set_wallet(ctx, ALICE, (300 * USD) as i64);
    let md = model(ctx, ALICE);
    assert_eq!(md.c_upnl, 1_000 * USD);
    assert_eq!((md.c_pim, md.c_mm), (550 * USD, 275 * USD));
    assert_eq!(md.a, 750 * USD, "A = cWB + uPnL − PIM");
    assert_eq!(md.x, 25 * USD, "X = cWB − ΣMM: the profit is not withdrawable");
    rejected(ctx, ALICE, &transferFromPerpCall { amount: 25 * USD as u64 + 1 }.abi_encode(), E_OUT);
    ok(ctx, ALICE, &transferFromPerpCall { amount: 25 * USD as u64 }.abi_encode());
    rejected(ctx, ALICE, &transferFromPerpCall { amount: 1 }.abi_encode(), E_OUT);
    // The profit still backs an open far larger than the wallet: 10 more at 1,100 (PIM 550 + fee
    // 5.5 against A = 725).
    fill(ctx, ALICE, M1, 10, 1_100);
    assert_eq!(pos(ctx, ALICE, M1).amount, 20);
    assert!(wallet(ctx, ALICE) < 300 * USD, "the fee came out of W; no silo was funded");
}

/// Pending funding is in `cWB` before anything settles it (D5: lazy settlement, numerically
/// Binance's instant one), so a touch that settles it moves `W` and leaves `cWB`, `E` and `A`
/// exactly where the fill alone puts them. An ISOLATED position's pending funding is not in `cWB`.
#[test]
fn f_pending_funding_is_in_cwb_and_settling_it_moves_no_account_number() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    enable_funding(ctx, M1, 500);
    fill(ctx, ALICE, M1, 10, 1_000);
    fill(ctx, CAROL, M1, 10, 1_000);
    tick(ctx, M1, 1_000); // opens the epoch
    tick(ctx, M1, 1_000); // folds it: index += 1,000 · 500
    let pf = -10 * 1_000 * 500;
    assert_eq!(model(ctx, ALICE).leg(M1).pf, pf, "0.05 % of $10,000 = $5 owed by the long");
    let before = model(ctx, ALICE);
    assert_eq!(before.cwb, before.w + pf, "cWB carries the pending funding");
    let got = account(ctx, ALICE);
    assert_eq!(got.totalCrossWalletBalance as i128, before.w + pf);
    let carol = model(ctx, CAROL);
    assert_eq!(carol.leg(M1).pf, pf);
    assert_eq!(carol.cwb, carol.w, "an isolated position's funding is not the cross wallet's");

    // The touch: one more contract at the mark. E moves by exactly the fee (the new contract's
    // uPnL is 0 at the mark), cWB by the fee, W by the funding and the fee.
    let c = fill(ctx, ALICE, M1, 1, 1_000);
    let fee_paid = fee(val(1_000, 1), TAKER_PPM);
    let after = model(ctx, ALICE);
    assert_eq!(after.w, before.w + pf - fee_paid, "W took the funding and the fee");
    assert_eq!(after.cwb, before.cwb - fee_paid, "settling moved nothing in cWB");
    assert_eq!(after.e, before.e - fee_paid, "settling moved nothing in E");
    assert_eq!(after.leg(M1).pf, 0);
    let settled: Vec<FundingSettled> =
        evs::<FundingSettled>(&c.logs).into_iter().filter(|f| f.user == ALICE).collect();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].amount as i128, pf);
}

/// J4: a cross leg whose market has no price (mark 0 — fixture only; `addMarket` refuses it)
/// contributes `uPnL = MM = PIM = 0`, not its bare entry cost; every surface stays total, and the
/// sweep and ADL — which fold the account — run through it without an error.
#[test]
fn f_a_zero_mark_cross_leg_contributes_nothing_and_stays_total() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    make(ctx, ALICE, M2, -5, 500); // cross short, vQuote +2,500
    fill(ctx, ALICE, M1, -4, 1_000); // cross short: the ADL winner below
    set_lev(ctx, CAROL, M1, 20);
    fill(ctx, CAROL, M1, 4, 1_000); // isolated long, liquidated below
    storage::save_mark_price(ctx, M2, 0).unwrap();
    check_all(ctx);
    let md = model(ctx, ALICE);
    let l2 = md.leg(M2);
    assert_eq!((l2.upnl, l2.mm, l2.pim), (0, 0, 0), "a priceless leg neither props up nor drags");
    assert_eq!(account(ctx, ALICE).totalCrossUnPnl as i128, md.leg(M1).upnl);

    // The sweep liquidates CAROL; with no bids her residual is ADL'd against ALICE, whose account
    // fold carries the priceless M2 leg. No error, no panic; the identity holds (`ok`).
    let c = tick(ctx, M1, 900);
    assert_eq!(evs::<Liquidation>(&c.logs).len(), 1);
    let adl = evs::<Adl>(&c.logs);
    assert!(!adl.is_empty() && adl.iter().all(|a| a.adlUser == ALICE));
    assert_eq!(pos(ctx, CAROL, M1).amount, 0);
    assert_eq!(pos(ctx, ALICE, M1).amount, 0);
    assert_eq!(pos(ctx, ALICE, M1).margin, 0);
}

/// The cross liquidation price of each row is Binance's LP1 (§1.5, report §3.2), computed here in
/// closed form: `LP1 = (WB − TMM1 + UPNL1 + cumB − Side1·Pos1·EP1) / (Pos1·MMR_B − Side1·Pos1)`.
/// ALICE: W = 2,000; long 10 on M1 at 1,000 (MMR 2.5 %); short 4 on M2 at 500 with the mark at 575
/// (uPnL −300, MM 115). And it MOVES with the other market's mark.
#[test]
fn f_the_cross_liquidation_price_is_binances_lp1() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, ALICE, M1, 10, 1_000);
    fill(ctx, ALICE, M2, -4, 500);
    tick(ctx, M2, 575);
    set_wallet(ctx, ALICE, (2_000 * USD) as i64);
    check_all(ctx);
    // M1 row: (2,000 − 115 − 300 − 10,000) / (0.25 − 10) = 863.08… → the greatest liquidatable
    // whole price is 863 (E 330 < ΣMM 330.75 there; at 864, E 340 ≥ 331).
    assert_eq!(risk(ctx, ALICE, M1).liquidationPrice, 863);
    // M2 row (a short, Side −1): (2,000 − 250 + 0 + 4·500) / (4·0.05 + 4) = 892.86… → 893.
    assert_eq!(risk(ctx, ALICE, M2).liquidationPrice, 893);
    // The other leg's mark moves: M2 back to 500 (uPnL 0, MM 100) → M1's
    // LP1 = (2,000 − 100 − 10,000) / (−9.75) = 830.77… → 830.
    tick(ctx, M2, 500);
    assert_eq!(risk(ctx, ALICE, M1).liquidationPrice, 830);
    let row = account(ctx, ALICE).positions.into_iter().find(|r| r.marketId == M1).unwrap();
    assert_eq!(row.liquidationPrice, 830);
}

/// The funding index mirror the account fold prices `PF` from survives every market admin write
/// (updateMarket, setMarginTiers, setBasisMode) — `cWB` keeps matching the FundingState index.
#[test]
fn f_the_funding_mirror_survives_market_admin_calls() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    enable_funding(ctx, M1, 400);
    fill(ctx, ALICE, M1, 10, 1_000);
    tick(ctx, M1, 1_000);
    tick(ctx, M1, 1_010);
    assert!(model(ctx, ALICE).c_pf != 0);
    enable_funding(ctx, M1, -100); // updateMarket — every check runs inside `ok`
    let t = table(M1);
    ok(
        ctx,
        ADMIN,
        &setMarginTiersCall {
            marketId: M1,
            lowerBounds: t.iter().map(|b| b.lb * USD as u64).collect(),
            maxLeverages: t.iter().map(|b| b.lev).collect(),
            maintMarginRatesPpm: t.iter().map(|b| b.ppm).collect(),
            notionalCap: 0,
        }
        .abi_encode(),
    );
    ok(ctx, ADMIN, &setBasisModeCall { marketId: M1, mode: 1 }.abi_encode());
    tick(ctx, M1, 1_020);
    tick(ctx, M1, 1_000);
    assert_eq!(
        storage::load_market_ref(ctx, M1).unwrap().unwrap().cumulative_funding_index,
        storage::load_funding_state(ctx, M1).unwrap().cumulative_funding_index
    );
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §3.3 item 2 — fills
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// A cross open moves NO cash (G1): taker or maker, `pos.margin` stays 0 and the only wallet move
/// is the fee; an isolated open on the same account moves its silo out of `W`.
#[test]
fn fill_a_cross_open_moves_no_cash_and_pays_its_fee_from_w() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    let w0 = wallet(ctx, ALICE);
    fill(ctx, ALICE, M1, 10, 1_000);
    assert_eq!(wallet(ctx, ALICE), w0 - fee(val(1_000, 10), TAKER_PPM));
    assert_eq!(pos(ctx, ALICE, M1).margin, 0);
    let w1 = wallet(ctx, ALICE);
    make(ctx, ALICE, M2, -8, 500);
    assert_eq!(wallet(ctx, ALICE), w1 - fee(val(500, 8), MAKER_PPM as u64));
    assert_eq!(pos(ctx, ALICE, M2).margin, 0);
    let w2 = wallet(ctx, ALICE);
    fill(ctx, ALICE, M3, 5, 200); // ISOLATED: 1,000 / 5 = 200 into the silo, the fee out of it
    let f = fee(val(200, 5), TAKER_PPM);
    assert_eq!(wallet(ctx, ALICE), w2 - 200 * USD);
    assert_eq!(pos(ctx, ALICE, M3).margin as i128, 200 * USD - f);
}

/// A cross close settles its realized PnL into W EITHER SIGN, through zero, with no per-fill bad
/// debt and no insurance-fund move (G2): a loss larger than the wallet leaves W negative, money out
/// stops at X = 0, and a deposit nets against the deficit.
///
/// ALICE keeps an ISOLATED long of 1 on M3 throughout, so the account is never flat: a negative W
/// with NO position left is cleared by the insurance fund at the end of the call (owner decision
/// Q8 — `risk::negative_balance`, pinned by the cross liquidation suite), and this pins the
/// receivable that stands until then.
#[test]
fn fill_a_cross_close_settles_either_sign_into_w_with_no_bad_debt() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, ALICE, M3, 1, 200);
    // Profit, as a taker: 10 long from 1,000 closed at 1,050.
    fill(ctx, ALICE, M1, 10, 1_000);
    let w0 = wallet(ctx, ALICE);
    fill(ctx, ALICE, M1, -10, 1_050);
    assert_eq!(wallet(ctx, ALICE), w0 + 500 * USD - fee(val(1_050, 10), TAKER_PPM));
    assert_eq!((pos(ctx, ALICE, M1).amount, pos(ctx, ALICE, M1).v_quote_balance), (0, 0));

    // Loss beyond the wallet, as a maker (a maker fill is never margin-gated): 10 long from 1,050,
    // the mark falls to 900, the wallet is cut to $200, the long is closed at 900.
    fill(ctx, ALICE, M1, 10, 1_050);
    tick(ctx, M1, 900);
    set_wallet(ctx, ALICE, (200 * USD) as i64);
    let fund = insurance(ctx);
    ok(ctx, ALICE, &order(M1, SELL, 900, 10, LIMIT, RO));
    ok(ctx, DAVE, &mkt(M1, BUY, 10));
    let w = 200 * USD - 1_500 * USD - fee(val(900, 10), MAKER_PPM as u64);
    assert_eq!(wallet(ctx, ALICE), w, "the whole $1,500 loss is in W");
    assert!(w < 0);
    assert_eq!(insurance(ctx), fund, "no bad debt");
    assert_eq!(pos(ctx, ALICE, M1).cumulative_realized_pnl as i128, 500 * USD - 1_500 * USD);
    let md = model(ctx, ALICE);
    assert_eq!((md.a, md.x), (w, 0), "no cross position, no order: A = W < 0, X = 0");
    rejected(ctx, ALICE, &transferFromPerpCall { amount: 1 }.abi_encode(), E_OUT);
    rest(ctx, DAVE, M1, SELL, 900, 1);
    rejected(ctx, ALICE, &mkt(M1, BUY, 1), E_MARGIN);
    cancel_all(ctx, DAVE, M1);
    // A deposit from spot nets against the deficit (Binance's "transfer in to cover the loss").
    set_spot(ctx, ALICE, 2_000 * USD as u64);
    ok(ctx, ALICE, &transferToPerpCall { amount: 2_000 * USD as u64 }.abi_encode());
    assert_eq!(wallet(ctx, ALICE), w + 2_000 * USD);
}

/// A flip in one fill: the closed half's realized PnL into W, the new half opened with no margin,
/// one fee on the whole notional.
#[test]
fn fill_a_cross_flip_closes_and_reopens_in_one_fill() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, ALICE, M1, 10, 1_000);
    let w0 = wallet(ctx, ALICE);
    fill(ctx, ALICE, M1, -25, 980);
    let p = pos(ctx, ALICE, M1);
    assert_eq!((p.amount, p.margin, p.v_quote_balance as i128), (-15, 0, val(980, 15)));
    assert_eq!(wallet(ctx, ALICE), w0 - 200 * USD - fee(val(980, 25), TAKER_PPM));
    // …and back, as a maker this time.
    let w1 = wallet(ctx, ALICE);
    make(ctx, ALICE, M1, 20, 1_000);
    let p = pos(ctx, ALICE, M1);
    assert_eq!((p.amount, p.margin, p.v_quote_balance as i128), (5, 0, -val(1_000, 5)));
    assert_eq!(wallet(ctx, ALICE), w1 - 300 * USD - fee(val(1_000, 20), MAKER_PPM as u64));
}

/// `open_position_count` follows every zero crossing — open, increase, flip, partial and full
/// close, a maker close, either type — and ignores resting orders (Q8 bookkeeping).
#[test]
fn fill_the_open_position_count_follows_every_zero_crossing() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    let count = |ctx: &mut TestCtx| storage::load_account(ctx, ALICE).unwrap().open_position_count;
    rest(ctx, ALICE, M2, BUY, 400, 1);
    assert_eq!(count(ctx), 0, "a resting order is not a position");
    fill(ctx, ALICE, M1, 3, 1_000);
    assert_eq!(count(ctx), 1);
    fill(ctx, ALICE, M1, 2, 1_000);
    assert_eq!(count(ctx), 1);
    fill(ctx, ALICE, M3, 2, 200);
    assert_eq!(count(ctx), 2);
    fill(ctx, ALICE, M1, -9, 1_000); // flip through zero
    assert_eq!(count(ctx), 2);
    fill(ctx, ALICE, M1, 4, 1_000); // back to exactly zero
    assert_eq!((pos(ctx, ALICE, M1).amount, count(ctx)), (0, 1));
    make(ctx, ALICE, M3, -2, 200); // a maker close of the isolated leg
    assert_eq!(count(ctx), 0);
    make(ctx, ALICE, M2, -3, 500);
    assert_eq!(count(ctx), 1);
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §3.3 item 6 — funding
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// Cross funding settles into W (F32): `FundingSettled`, then `AccountBalanceChanged(FUNDING_FEE)`
/// carrying the moved wallet, and NO position row in that group; the isolated counterpart's group
/// carries its `PositionChanged` with the moved silo. A charge beyond the wallet takes it negative.
#[test]
fn fund_cross_funding_is_a_balance_only_push_into_w() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    enable_funding(ctx, M1, 500);
    fill(ctx, ALICE, M1, 10, 1_000);
    fill(ctx, CAROL, M1, 10, 1_000);
    tick(ctx, M1, 1_000);
    for _ in 0..3 {
        tick(ctx, M1, 1_000); // three epochs: 3 · $5
    }
    let pf = -3 * 10 * 1_000 * 500;
    assert_eq!(model(ctx, ALICE).leg(M1).pf, pf);
    set_wallet(ctx, ALICE, (10 * USD) as i64); // the $15 charge is larger than the wallet
    let carol_margin = pos(ctx, CAROL, M1).margin as i128;

    // ALICE's touch: a maker fill, so no taker gate stands in the way of the negative wallet.
    rest(ctx, ALICE, M1, SELL, 1_000, 1);
    let c = ok(ctx, DAVE, &mkt(M1, BUY, 1));
    let i = c.logs.iter().position(|l| is::<FundingSettled>(l)).expect("FundingSettled");
    let f: FundingSettled = decode(&c.logs[i]);
    assert_eq!(
        (f.user, f.marketId, f.amount as i128, f.fundingRate, f.markPrice),
        (ALICE, M1, pf, 500, 1_000)
    );
    let h: AccountBalanceChanged = decode(&c.logs[i + 1]);
    assert_eq!((h.user, h.reason), (ALICE, AccountUpdateReason::FundingFee.code()));
    assert_eq!(h.totalCrossWalletBalance as i128, 10 * USD + pf, "the header: W after funding");
    assert!(h.totalCrossWalletBalance < 0, "the charge took W negative");
    assert!(!is::<PositionChanged>(&c.logs[i + 2]), "a cross funding settle has no position row");
    assert_eq!(pos(ctx, ALICE, M1).margin, 0);
    assert_eq!(wallet(ctx, ALICE), 10 * USD + pf - fee(val(1_000, 1), MAKER_PPM as u64));

    // CAROL's (ISOLATED) touch: the payment lands on her silo and the group has its position row.
    rest(ctx, CAROL, M1, SELL, 1_000, 1);
    let c = ok(ctx, DAVE, &mkt(M1, BUY, 1));
    let i = c.logs.iter().position(|l| is::<FundingSettled>(l)).expect("FundingSettled");
    assert!(is::<AccountBalanceChanged>(&c.logs[i + 1]));
    let row: PositionChanged = decode(&c.logs[i + 2]);
    assert_eq!((row.user, row.margin as i128), (CAROL, carol_margin + pf));
}

fn funding_of(logs: &[Log], u: Address) -> i128 {
    evs::<FundingSettled>(logs).iter().filter(|f| f.user == u).map(|f| f.amount as i128).sum()
}

/// A cross SHORT receives funding into W when the rate is positive, and a negative rate reverses
/// both sides — the sign follows `calc_funding_payment`, on the wallet.
#[test]
fn fund_cross_funding_credits_and_charges_the_wallet_either_sign() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    enable_funding(ctx, M2, 500);
    fill(ctx, ALICE, M2, -10, 500); // short receives at +500
    tick(ctx, M2, 500);
    tick(ctx, M2, 500);
    let w0 = wallet(ctx, ALICE);
    let c = fill(ctx, ALICE, M2, -1, 500);
    let got = funding_of(&c.logs, ALICE);
    assert_eq!(got, 10 * 500 * 500);
    assert_eq!(wallet(ctx, ALICE), w0 + got - fee(val(500, 1), TAKER_PPM));
    enable_funding(ctx, M2, -500);
    tick(ctx, M2, 500);
    let w1 = wallet(ctx, ALICE);
    let c = fill(ctx, ALICE, M2, 1, 500);
    let got = funding_of(&c.logs, ALICE);
    assert_eq!(got, -11 * 500 * 500, "a short pays at a negative rate");
    assert_eq!(wallet(ctx, ALICE), w1 + got - fee(val(500, 1), TAKER_PPM));
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// §3.3 items 3, 5, 7–10, J1, J6, Q9, Q11 — the gates, each to the unit
// ═════════════════════════════════════════════════════════════════════════════════════════════

fn zero_fees(ctx: &mut TestCtx, u: Address) {
    ok(ctx, ADMIN, &setUserFeeRatesCall { user: u, makerFeePpm: 0, takerFeePpm: 0 }.abi_encode());
}

/// §3.3 item 3: a cross open is admitted iff `A_post ≥ fee` — the open's own PIM is inside `A_post`
/// (nothing moves into a silo) — with NO exemption at a zero fee (B1 covers only a non-positive
/// requirement, and a cross open's requirement is its PIM).
#[test]
fn gate_a_cross_taker_open_needs_a_post_to_cover_its_fee() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    // 10 at the mark on M1, leverage 20: uPnL 0, PIM 500, fee 5 — so W ≥ 505.
    let need = ceil_div(val(1_000, 10), 20) + fee(val(1_000, 10), TAKER_PPM);
    assert_eq!(need, 505 * USD);
    rest(ctx, DAVE, M1, SELL, 1_000, 10);
    set_wallet(ctx, ALICE, (need - 1) as i64);
    rejected(ctx, ALICE, &mkt(M1, BUY, 10), E_MARGIN);
    set_wallet(ctx, ALICE, need as i64);
    ok(ctx, ALICE, &mkt(M1, BUY, 10));
    assert_eq!(model(ctx, ALICE).a, 0, "admitted exactly at A_post − fee = 0");
    assert_eq!(wallet(ctx, ALICE), 500 * USD);

    // A zero-fee account: the same open at leverage 10 needs W ≥ PIM = 1,000 — not "anything".
    zero_fees(ctx, ERIN);
    rest(ctx, DAVE, M1, SELL, 1_000, 10);
    set_wallet(ctx, ERIN, (1_000 * USD - 1) as i64);
    rejected(ctx, ERIN, &mkt(M1, BUY, 10), E_MARGIN);
    set_wallet(ctx, ERIN, (1_000 * USD) as i64);
    ok(ctx, ERIN, &mkt(M1, BUY, 10));
}

/// The J1 fixture: ALICE short 39 on M2 at leverage 10 (inside the first bracket at 500), the mark
/// carried to 1,100. Now `N = 42,900`: `MM = 20,000·5 % + 22,900·15 % = 4,435 > PIM = 4,290`, and
/// the leg is $23,400 under water — an account `A` can admit while it sits under maintenance.
fn j1_fixture() -> TestCtx {
    let mut ctx = crossed_world();
    fill(&mut ctx, ALICE, M2, -39, 500);
    tick(&mut ctx, M2, 1_100);
    let md = model(&mut ctx, ALICE);
    assert_eq!((md.leg(M2).mm, md.leg(M2).pim), (4_435 * USD, 4_290 * USD));
    assert_eq!(md.leg(M2).upnl, -23_400 * USD);
    ctx
}

/// J1: an opening taker fill must leave `E_post − draw ≥ Σ_C MM_post` (equality healthy, §1.5 /
/// Q6), checked even where `A` admits it. A cross open of 1 on M1 at the mark: `ΣMM_post = 4,435 +
/// 25`, `draw = fee = 0.5`, so `W ≥ 4,460.5 + 23,400`; one unit less is refused J1 while
/// `A_post = W − 23,400 − 4,290 − 50 ≈ 120 ≥ fee`.
#[test]
fn gate_j1_holds_a_cross_open_to_account_maintenance_to_the_unit() {
    let mut ctx = j1_fixture();
    let ctx = &mut ctx;
    let w_star = 4_435 * USD + 25 * USD + fee(val(1_000, 1), TAKER_PPM) + 23_400 * USD;
    rest(ctx, DAVE, M1, SELL, 1_000, 1);
    set_wallet(ctx, ALICE, (w_star - 1) as i64);
    let a_post = (w_star - 1) - 23_400 * USD - 4_290 * USD - 50 * USD;
    assert!(a_post >= fee(val(1_000, 1), TAKER_PPM), "the available-balance gate alone admits it");
    rejected(ctx, ALICE, &mkt(M1, BUY, 1), E_J1);
    set_wallet(ctx, ALICE, w_star as i64);
    ok(ctx, ALICE, &mkt(M1, BUY, 1));
    let md = model(ctx, ALICE);
    assert_eq!(md.e, md.c_mm, "admitted at E == ΣMM exactly — healthy (the strict `<` condition)");
    assert!(!md.below_maintenance());
}

/// J1 holds an ISOLATED open of an account with cross exposure too: its silo cash leaves W, so E
/// falls by the draw (1 on M3 at leverage 5: 40 into the silo, the 0.1 fee out of it — draw 40).
#[test]
fn gate_j1_holds_an_isolated_open_of_a_cross_account_to_maintenance() {
    let mut ctx = j1_fixture();
    let ctx = &mut ctx;
    let w_star = 4_435 * USD + 40 * USD + 23_400 * USD;
    rest(ctx, DAVE, M3, SELL, 200, 1);
    set_wallet(ctx, ALICE, (w_star - 1) as i64);
    rejected(ctx, ALICE, &mkt(M3, BUY, 1), E_J1);
    set_wallet(ctx, ALICE, w_star as i64);
    ok(ctx, ALICE, &mkt(M3, BUY, 1));
    assert_eq!(wallet(ctx, ALICE), w_star - 40 * USD);
    let md = model(ctx, ALICE);
    assert_eq!(md.e, md.c_mm);
}

/// A pure reduce is not held to J1 (§3.3 item 3: B1 for a pure reduce): a zero-fee cross account
/// under maintenance with `A < 0` reduces by taker order, and stays under; an OPEN is refused.
#[test]
fn gate_a_pure_reduce_is_not_held_to_j1() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    zero_fees(ctx, ERIN);
    set_lev(ctx, ERIN, M2, 10);
    fill(ctx, ERIN, M2, -39, 500);
    tick(ctx, M2, 1_100);
    set_wallet(ctx, ERIN, (20_000 * USD) as i64);
    let md = model(ctx, ERIN);
    assert!(md.below_maintenance() && md.a < 0);
    rest(ctx, DAVE, M2, SELL, 1_100, 10);
    ok(ctx, ERIN, &order(M2, BUY, 0, 10, MKT, RO));
    assert_eq!(pos(ctx, ERIN, M2).amount, -29);
    assert!(model(ctx, ERIN).below_maintenance(), "still under: the reduce was not held to J1");
    rest(ctx, DAVE, M1, BUY, 1_000, 1);
    rejected(ctx, ERIN, &mkt(M1, SELL, 1), E_MARGIN);
}

/// §3.3 item 5: a resting order is admitted iff `A_after ≥ 0` — cross profit included — or it does
/// not raise ooIM. ALICE: +1,000 of cross profit on M2 (PIM 600); a buy of 2 at 900 on M1
/// (leverage 20) adds ooIM 90, so it rests from `W = −310` up, a NEGATIVE wallet.
#[test]
fn gate_the_resting_order_gate_reads_a_with_cross_profit_to_the_unit() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, ALICE, M2, 10, 500);
    tick(ctx, M2, 600);
    let w_star = -310 * USD;
    set_wallet(ctx, ALICE, (w_star - 1) as i64);
    rejected(ctx, ALICE, &order(M1, BUY, 900, 2, LIMIT, 0), E_MARGIN);
    set_wallet(ctx, ALICE, w_star as i64);
    rest(ctx, ALICE, M1, BUY, 900, 2);
    assert_eq!(model(ctx, ALICE).a, 0);
    // An order that does not raise ooIM rests at any A (a sell of 5 against the long of 10).
    set_wallet(ctx, ALICE, (w_star - 1_000 * USD) as i64);
    assert!(model(ctx, ALICE).a < 0);
    rest(ctx, ALICE, M2, SELL, 650, 5);
}

/// §3.3 item 5: the pre-walk filter must not refuse an order the account can afford through cross
/// profit or through the order's own open gain.
/// (a) W = $1 with +1,000 cross profit: a resting buy of 5 at 900 (ooIM 225) is admitted on A.
/// (b) Flat, a crossing buy of 2 at 990 against a sell at 950 with the mark at 1,000: the fill
///     itself is worth +100 uPnL = its own PIM, so `A_post = W ≥ fee 0.95` admits it from W = 0.95.
#[test]
fn gate_the_pre_walk_filter_does_not_refuse_cross_backed_orders() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, ALICE, M2, 10, 500);
    tick(ctx, M2, 600);
    set_wallet(ctx, ALICE, USD as i64);
    rest(ctx, ALICE, M1, BUY, 900, 5);
    assert_eq!(model(ctx, ALICE).a, (1 + 1_000 - 600 - 225) * USD);

    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    let f = fee(val(950, 2), TAKER_PPM);
    rest(ctx, DAVE, M1, SELL, 950, 2);
    set_wallet(ctx, ALICE, (f - 1) as i64);
    rejected(ctx, ALICE, &order(M1, BUY, 990, 2, LIMIT, 0), E_MARGIN);
    set_wallet(ctx, ALICE, f as i64);
    let c = ok(ctx, ALICE, &order(M1, BUY, 990, 2, LIMIT, 0));
    assert_eq!(evs::<Trade>(&c.logs).iter().map(|t| t.quantity).sum::<u64>(), 2);
    assert_eq!(model(ctx, ALICE).a, 0);
}

/// §3.3 item 7: cross leverage rises freely and lowers against A (`-2028`):
/// `ΔIM = ΔPIM + ΔooIM ≤ A`. ISOLATED keeps its rule (no lowering under a position).
#[test]
fn gate_cross_set_leverage_lowers_against_a_and_raises_freely() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, ALICE, M1, 10, 1_000); // PIM 500 at 20×
    // 20× → 10×: ΔIM = 500, so A = W − 500 must cover it: W ≥ 1,000.
    set_wallet(ctx, ALICE, (1_000 * USD - 1) as i64);
    rejected(ctx, ALICE, &setLeverageCall { marketId: M1, leverage: 10 }.abi_encode(), E_2028);
    set_wallet(ctx, ALICE, (1_000 * USD) as i64);
    set_lev(ctx, ALICE, M1, 10);
    assert_eq!(model(ctx, ALICE).a, 0);
    // Raising is admitted at any balance — here A < 0.
    set_wallet(ctx, ALICE, (-1_000 * USD) as i64);
    set_lev(ctx, ALICE, M1, 20);
    // ΔooIM counts: a resting buy of 10 at 400 on flat M2 (IM 400 at 10×, 800 at 5×).
    set_wallet(ctx, ALICE, (900 * USD) as i64);
    rest(ctx, ALICE, M2, BUY, 400, 10);
    let w_star = 500 * USD + 400 * USD + 400 * USD;
    set_wallet(ctx, ALICE, (w_star - 1) as i64);
    rejected(ctx, ALICE, &setLeverageCall { marketId: M2, leverage: 5 }.abi_encode(), E_2028);
    set_wallet(ctx, ALICE, w_star as i64);
    set_lev(ctx, ALICE, M2, 5);
    // ISOLATED: lowering under a position keeps its old refusal; raising is fine.
    fill(ctx, CAROL, M1, 5, 1_000);
    rejected(ctx, CAROL, &setLeverageCall { marketId: M1, leverage: 5 }.abi_encode(), E_ISO_LEV);
    set_lev(ctx, CAROL, M1, 20);
}

/// The X fixture: ALICE cross long 10 on M1 at +$1,000 (MM 275, PIM 550), an ISOLATED long of 5 on
/// M3, W = $1,275 — so `A = 1,275 + 1,000 − 550 = 1,725` but `X = min(1,275 − 275, A) = 1,000`.
fn x_fixture() -> TestCtx {
    let mut ctx = crossed_world();
    fill(&mut ctx, ALICE, M1, 10, 1_000);
    fill(&mut ctx, ALICE, M3, 5, 200);
    tick(&mut ctx, M1, 1_100);
    set_wallet(&mut ctx, ALICE, (1_275 * USD) as i64);
    let md = model(&mut ctx, ALICE);
    assert_eq!((md.a, md.x), (1_725 * USD, 1_000 * USD));
    ctx
}

/// §3.3 item 8: add/remove position margin on a CROSSED market is `-4049` (with or without a
/// position); an isolated add is capped by X (`-4050`), not by A.
#[test]
fn gate_position_margin_is_4049_on_crossed_and_x_capped_on_isolated() {
    let mut ctx = x_fixture();
    let ctx = &mut ctx;
    let add =
        |m: u64, a: i128| addPositionMarginCall { marketId: m, amount: a as u64 }.abi_encode();
    let remove =
        |m: u64, a: i128| removePositionMarginCall { marketId: m, amount: a as u64 }.abi_encode();
    rejected(ctx, ALICE, &add(M1, 1), E_4049_ADD);
    rejected(ctx, ALICE, &remove(M1, 1), E_4049_REMOVE);
    rejected(ctx, ALICE, &add(M2, 1), E_4049_ADD); // flat CROSSED market: -4049 first
    rejected(ctx, ALICE, &remove(M2, 1), E_4049_REMOVE);
    rejected(ctx, ALICE, &add(M3, 1_000 * USD + 1), E_4050);
    let silo = pos(ctx, ALICE, M3).margin as i128;
    ok(ctx, ALICE, &add(M3, 1_000 * USD));
    assert_eq!(pos(ctx, ALICE, M3).margin as i128, silo + 1_000 * USD);
    assert_eq!(model(ctx, ALICE).x, 0);
    rejected(ctx, ALICE, &add(M3, 1), E_4050);
}

/// §3.3 item 9: every money-out gate stops at X to the unit — transferFromPerp, internalTransfer
/// out of the perp wallet, depositInsuranceFund — though A would cover more; a negative W lets
/// nothing out.
#[test]
fn gate_every_money_out_gate_stops_at_x() {
    let mut ctx = x_fixture();
    let ctx = &mut ctx;
    let x = 1_000 * USD;
    rejected(ctx, ALICE, &transferFromPerpCall { amount: (x + 1) as u64 }.abi_encode(), E_OUT);
    let internal = |amount: i128| {
        internalTransferCall { to: ALICE, fromWallet: 1, toWallet: 0, amount: amount as u64 }
            .abi_encode()
    };
    rejected(ctx, ALICE, &internal(x + 1), E_OUT_INTERNAL);
    ok(ctx, ALICE, &internal(x / 2));
    rejected(ctx, ALICE, &transferFromPerpCall { amount: (x / 2 + 1) as u64 }.abi_encode(), E_OUT);
    ok(ctx, ALICE, &transferFromPerpCall { amount: (x / 2) as u64 }.abi_encode());
    rejected(ctx, ALICE, &transferFromPerpCall { amount: 1 }.abi_encode(), E_OUT);
    assert!(model(ctx, ALICE).a > 0, "A still covers more: the profit stays behind");
    set_wallet(ctx, ALICE, -1);
    rejected(ctx, ALICE, &internal(1), E_OUT_INTERNAL);

    // The insurance-fund deposit, by an admin holding the same shape.
    set_lev(ctx, ADMIN, M1, 20);
    fill(ctx, ADMIN, M1, 10, 1_100);
    tick(ctx, M1, 1_210);
    set_wallet(ctx, ADMIN, (1_302_500_000) as i64); // X = W − MM(12,100 · 2.5 % = 302.5) = 1,000
    assert_eq!(model(ctx, ADMIN).x, 1_000 * USD);
    let dep = |a: i128| depositInsuranceFundCall { amount: a as u64 }.abi_encode();
    rejected(ctx, ADMIN, &dep(1_000 * USD + 1), E_OUT_IF);
    ok(ctx, ADMIN, &dep(1_000 * USD));
}

/// J6: the cash an ISOLATED taker open moves into its silo must be in the wallet —
/// `opening margin ≤ max(W, 0)` — whatever cross profit A counts. 3 on M3 at 200, leverage 5: a
/// silo of 120.
#[test]
fn gate_j6_an_isolated_taker_silo_is_funded_from_wallet_cash_only() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, ALICE, M1, 10, 1_000);
    tick(ctx, M1, 1_100); // +1,000 cross profit
    rest(ctx, DAVE, M3, SELL, 200, 3);
    set_wallet(ctx, ALICE, (120 * USD - 1) as i64);
    let md = model(ctx, ALICE);
    assert!(md.a >= 120 * USD && md.e - 120 * USD >= md.c_mm, "A and J1 both admit it");
    rejected(ctx, ALICE, &mkt(M3, BUY, 3), E_J6);
    set_wallet(ctx, ALICE, (120 * USD) as i64);
    ok(ctx, ALICE, &mkt(M3, BUY, 3));
    assert_eq!(wallet(ctx, ALICE), 0);
    assert_eq!(pos(ctx, ALICE, M3).margin as i128, 120 * USD - fee(val(200, 3), TAKER_PPM));
}

/// J6, maker side (unchanged M1 cap): an isolated maker open is funded with `min(requirement,
/// max(W, 0))` — here W = 70 against a requirement of 120 — and never from cross profit.
#[test]
fn gate_j6_an_isolated_maker_silo_is_capped_at_the_cash_at_hand() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, ALICE, M1, 10, 1_000);
    tick(ctx, M1, 1_100);
    set_wallet(ctx, ALICE, (70 * USD) as i64);
    rest(ctx, ALICE, M3, BUY, 200, 3);
    ok(ctx, DAVE, &mkt(M3, SELL, 3));
    assert_eq!(pos(ctx, ALICE, M3).amount, 3);
    assert_eq!(wallet(ctx, ALICE), 0);
    assert_eq!(pos(ctx, ALICE, M3).margin as i128, 70 * USD - fee(val(200, 3), MAKER_PPM as u64));
}

/// Q9 / D13: a cross MAKER fill is not checked against the account — it fills even when it leaves
/// the account under maintenance (a bid at 1,000 hit with the mark at 960).
#[test]
fn gate_a_cross_maker_fill_is_not_checked_against_the_account() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    set_wallet(ctx, ALICE, (500 * USD) as i64);
    rest(ctx, ALICE, M1, BUY, 1_000, 10); // ooIM 500 = A
    tick(ctx, M1, 960);
    let c = ok(ctx, BOB, &mkt(M1, SELL, 10));
    assert!(evs::<Trade>(&c.logs).iter().all(|t| t.maker == ALICE));
    assert_eq!(pos(ctx, ALICE, M1).amount, 10);
    let md = model(ctx, ALICE);
    assert_eq!(md.e, (500 - 2 - 400) * USD);
    assert_eq!(md.c_mm, 240 * USD);
    assert!(md.below_maintenance(), "the fill was admitted although it left E < ΣMM");
}

// ── Signed calldata (the -2023 matrix) ───────────────────────────────────────────────────────

const SIGNED_TS: u64 = 1; // the default test block's timestamp
const SIGNED_RECV: u64 = 60;
const KEY_ID: u8 = 1;

fn register_key(ctx: &mut TestCtx, u: Address) -> SigningKey {
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    ok(
        ctx,
        u,
        &registerApiKeyCall {
            keyId: KEY_ID,
            pubkey: sk.verifying_key().to_bytes().into(),
            expiry: 0,
        }
        .abi_encode(),
    );
    sk
}

#[allow(clippy::too_many_arguments)]
fn signed_order(
    ctx: &TestCtx,
    sk: &SigningKey,
    account: Address,
    m: u64,
    side: u8,
    price: u64,
    qty: u64,
    kind: u8,
    flags: u8,
    salt: u8,
) -> Vec<u8> {
    let mut client = [0u8; 16];
    client[15] = salt;
    let mut msg = b"perpdex_v1_order".to_vec();
    msg.extend_from_slice(&ctx.chain_id().to_be_bytes());
    msg.extend_from_slice(account.as_slice());
    msg.extend_from_slice(&m.to_be_bytes());
    msg.push(side);
    msg.extend_from_slice(&price.to_be_bytes());
    msg.extend_from_slice(&qty.to_be_bytes());
    msg.extend_from_slice(&[kind, GTC]);
    msg.extend_from_slice(&client);
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.extend_from_slice(&[KEY_ID, flags]);
    placeOrderSignedCall {
        account,
        marketId: m,
        side,
        price,
        quantity: qty,
        orderType: kind,
        tif: GTC,
        clientOrderId: client.into(),
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: KEY_ID,
        flags,
        signature: sk.sign(&msg).to_bytes().to_vec().into(),
    }
    .abi_encode()
}

fn signed_margin_type(ctx: &TestCtx, sk: &SigningKey, account: Address, m: u64, t: u8) -> Vec<u8> {
    let mut msg = b"perpdex_v1_margintype".to_vec();
    msg.extend_from_slice(&ctx.chain_id().to_be_bytes());
    msg.extend_from_slice(account.as_slice());
    msg.extend_from_slice(&m.to_be_bytes());
    msg.push(t);
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.push(KEY_ID);
    setMarginTypeSignedCall {
        account,
        marketId: m,
        marginType: t,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: KEY_ID,
        signature: sk.sign(&msg).to_bytes().to_vec().into(),
    }
    .abi_encode()
}

fn item(m: u64, side: u8, price: u64, qty: u64, flags: u8) -> PlaceItem {
    PlaceItem {
        marketId: m,
        side,
        price,
        quantity: qty,
        orderType: LIMIT,
        tif: GTC,
        clientOrderId: FixedBytes::default(),
        flags,
    }
}

/// Q11 / `-2023`, every entrypoint: while the account is IN LIQUIDATION only reduce-only orders are
/// placed (placeOrder, placeOrderSigned, batchPlaceOrders, asSubAccount, placeAlgoOrder), the mode
/// switch is refused (direct and signed), cancels go through, and every RESTING order of the
/// account — reduce-only included — is cancelled when the book reaches it, not filled (§3.1).
/// Nobody else is affected, and clearing the flag lifts it all.
#[test]
fn gate_in_liquidation_only_reducing_orders_on_every_entrypoint() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    let sk = register_key(ctx, ALICE);
    fill(ctx, ALICE, M1, 10, 1_000);
    rest(ctx, ALICE, M1, SELL, 1_100, 2); // a plain resting order
    let ro = placeOrderCall::abi_decode_returns(
        &ok(ctx, ALICE, &order(M1, SELL, 1_150, 1, LIMIT, RO)).out.bytes,
    )
    .unwrap()
    .0;
    rest(ctx, BOB, M1, SELL, 1_101, 2);
    flag(ctx, ALICE, true);

    // placeOrder: refused write-clean, buy or sell, limit or market; reduce-only accepted.
    rejected(ctx, ALICE, &order(M1, BUY, 900, 1, LIMIT, 0), E_2023);
    rejected(ctx, ALICE, &mkt(M1, SELL, 1), E_2023);
    rest(ctx, DAVE, M1, BUY, 1_000, 1);
    ok(ctx, ALICE, &order(M1, SELL, 0, 1, MKT, RO));
    assert_eq!(pos(ctx, ALICE, M1).amount, 9);

    // placeOrderSigned: refused after the burn with the same reason; reduce-only accepted.
    let plain = signed_order(ctx, &sk, ALICE, M1, BUY, 900, 1, LIMIT, 0, 1);
    rejected_after_burn(ctx, ERIN, &plain, E_2023);
    rest(ctx, DAVE, M1, BUY, 1_000, 1);
    ok(ctx, ERIN, &signed_order(ctx, &sk, ALICE, M1, SELL, 0, 1, MKT, RO, 2));
    assert_eq!(pos(ctx, ALICE, M1).amount, 8);

    // batchPlaceOrders: the plain item rejected with reason 35, the reduce-only one placed.
    let c = ok(
        ctx,
        ALICE,
        &batchPlaceOrdersCall {
            orders: vec![item(M1, BUY, 900, 1, 0), item(M1, SELL, 1_200, 1, RO)],
        }
        .abi_encode(),
    );
    let st = batchPlaceOrdersCall::abi_decode_returns(&c.out.bytes).unwrap();
    let st: Vec<(u8, u8)> = st.chunks(34).map(|r| (r[0], r[33])).collect();
    assert_eq!(st, vec![(0, BATCH_IN_LIQUIDATION), (1, 0)]);

    // placeAlgoOrder: a reduce-only stop is accepted.
    ok(
        ctx,
        ALICE,
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: M1,
            side: SELL,
            orderType: 1,
            triggerPrice: 800,
            price: 0,
            quantity: 1,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO,
        }
        .abi_encode(),
    );

    // setMarginType: refused, direct and signed (flat, order-free M2 — every earlier rule passes).
    let switch = setMarginTypeCall { marketId: M2, marginType: ISOLATED }.abi_encode();
    rejected(ctx, ALICE, &switch, E_2023_MT);
    let sig = signed_margin_type(ctx, &sk, ALICE, M2, ISOLATED);
    rejected_after_burn(ctx, ERIN, &sig, E_2023_MT);

    // A cancel is never refused.
    ok(ctx, ALICE, &cancelOrderCall { orderId: ro.into(), marketId: M1 }.abi_encode());

    // The maker freeze: DAVE buys 2 — ALICE's plain sell at 1,100 is cancelled (reason 3), BOB's at
    // 1,101 fills; then 1 more — ALICE's REDUCE-ONLY sell at 1,200 is EXEMPT (P3, R1: it can only
    // reduce) and fills.
    let c = ok(ctx, DAVE, &mkt(M1, BUY, 2));
    let cancels = evs::<OrderCancelled>(&c.logs);
    assert_eq!(cancels.len(), 1);
    assert_eq!((cancels[0].user, cancels[0].reason), (ALICE, CANCEL_LIQUIDATION));
    assert!(evs::<Trade>(&c.logs).iter().all(|t| t.maker == BOB && t.quantity == 2));
    assert_eq!(pos(ctx, ALICE, M1).amount, 8, "a frozen maker is never filled");
    let c = ok(ctx, DAVE, &mkt(M1, BUY, 1));
    assert!(evs::<OrderCancelled>(&c.logs).is_empty(), "the reduce-only maker is not frozen");
    let t = evs::<Trade>(&c.logs);
    assert_eq!((t.len(), t[0].maker, t[0].price, t[0].quantity), (1, ALICE, 1_200, 1));
    assert_eq!(pos(ctx, ALICE, M1).amount, 7, "it reduced the position");

    // Nobody else is affected; clearing the flag lifts everything.
    rest(ctx, BOB, M1, BUY, 900, 1);
    flag(ctx, ALICE, false);
    rest(ctx, ALICE, M1, BUY, 900, 1);
    ok(ctx, ALICE, &setMarginTypeCall { marketId: M2, marginType: ISOLATED }.abi_encode());
}

/// Q11 through `asSubAccount`: a master placing for its sub-account in liquidation is refused the
/// same way; a reduce-only order goes through.
#[test]
fn gate_in_liquidation_reaches_a_sub_account_through_its_master() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    let c = ok(ctx, ALICE, &createSubAccountCall { name: "s".into() }.abi_encode());
    let sub = createSubAccountCall::abi_decode_returns(&c.out.bytes).unwrap();
    USERS.with(|us| us.borrow_mut().push(sub));
    ok(
        ctx,
        ALICE,
        &internalTransferCall { to: sub, fromWallet: 1, toWallet: 1, amount: 10_000 * USD as u64 }
            .abi_encode(),
    );
    let as_sub =
        |inner: Vec<u8>| asSubAccountCall { subAccount: sub, data: inner.into() }.abi_encode();
    rest(ctx, DAVE, M1, SELL, 1_000, 2);
    ok(ctx, ALICE, &as_sub(mkt(M1, BUY, 2)));
    flag(ctx, sub, true);
    rejected(ctx, ALICE, &as_sub(order(M1, BUY, 900, 1, LIMIT, 0)), E_2023);
    rest(ctx, DAVE, M1, BUY, 1_000, 1);
    ok(ctx, ALICE, &as_sub(order(M1, SELL, 0, 1, MKT, RO)));
    assert_eq!(pos(ctx, sub, M1).amount, 1);
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Liquidation of a CROSSED position (P3 — the P2 interim skipped it) and ADL with cross winners
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// The sweep and `liquidate()` judge a CROSSED position on its ACCOUNT (P3 — the P2 interim left
/// it alone): healthy, `liquidate()` refuses it write-clean; below cross maintenance after the tick
/// (E = −400 < ΣMM = 225, BANKRUPT), the tick's sweep runs the episode, whose IOC finds no bid and
/// whose ADL then fills at the MM-share bankruptcy price — above the mark for an insolvent long, all
/// of the account's share on its one market — so the deficit goes to the winner, DAVE, and the
/// wallet ends at zero, not below (no fee either: a bankrupt account pays none). An ISOLATED
/// position in the same tick is liquidated per position as always.
#[test]
fn liq_the_sweep_and_liquidate_judge_a_crossed_position_on_its_account() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    fill(ctx, ALICE, M1, 10, 1_000);
    set_lev(ctx, CAROL, M1, 20);
    fill(ctx, CAROL, M1, 5, 1_000); // isolated: silo 250 − 2.5
    fill(ctx, BOB, M2, 2, 500); // a healthy isolated position
    set_wallet(ctx, ALICE, (600 * USD) as i64);
    assert!(!model(ctx, ALICE).below_maintenance());
    rejected(ctx, ERIN, &liquidateCall { user: ALICE, marketId: M1 }.abi_encode(), E_LIQ_CROSS_HEALTHY);
    let if0 = storage::load_insurance_fund(ctx).unwrap();

    let c = tick(ctx, M1, 900);
    let mut liq: Vec<(Address, u8)> =
        evs::<Liquidation>(&c.logs).iter().map(|l| (l.user, l.marginType)).collect();
    liq.sort();
    assert_eq!(liq, vec![(ALICE, CROSSED), (CAROL, ISOLATED)]);
    assert_eq!(pos(ctx, ALICE, M1).amount, 0);
    assert_eq!(pos(ctx, CAROL, M1).amount, 0);
    let started = evs::<CrossLiquidationStarted>(&c.logs);
    assert_eq!(started.len(), 1);
    assert_eq!((started[0].user, started[0].triggerMarketId), (ALICE, M1));
    assert_eq!(
        (started[0].crossMarginBalance, started[0].totalCrossMaintMargin),
        ((-400 * USD) as i64, (225 * USD) as u64),
        "the detection values"
    );
    assert_eq!(evs::<CrossLiquidationEnded>(&c.logs).len(), 1);
    assert!(!storage::load_account(ctx, ALICE).unwrap().in_liquidation());
    let adl = evs::<Adl>(&c.logs);
    assert!(adl.iter().all(|a| a.adlUser == DAVE), "DAVE is the only opposite holder");
    let alice_adl: Vec<&Adl> = adl.iter().filter(|a| a.liquidatedUser == ALICE).collect();
    assert_eq!(alice_adl.iter().map(|a| a.qty).sum::<u64>(), 10);
    assert!(alice_adl.iter().all(|a| a.price > 900), "an insolvent long's price is above the mark");
    let w = wallet(ctx, ALICE);
    assert!((0..USD).contains(&w), "the deficit went to the winner, W ends at zero: {w}");
    assert_eq!(storage::load_insurance_fund(ctx).unwrap(), if0, "no fee, no bad debt");

    rejected(ctx, ERIN, &liquidateCall { user: ALICE, marketId: M1 }.abi_encode(), E_LIQ_NO_POSITION);
    rejected(ctx, ERIN, &liquidateCall { user: BOB, marketId: M2 }.abi_encode(), E_HEALTHY);
}

/// ADL winners: CROSSED is ranked on its ACCOUNT — `uPnL_k / E` (§3.4 ADL, Binance's
/// `PnL% × EffLev` for a cross position) — and ISOLATED on its position, `uPnL / equity`. CAROL's
/// isolated long of 4 is liquidated at 900 with an empty book; BOB (cross) and ERIN (isolated,
/// silo 1,998) are short 4 each at +400.
#[test]
fn liq_adl_ranks_a_cross_winner_on_its_account_equity() {
    fn run_with(bob_wallet: i128) -> Vec<Address> {
        let mut ctx = crossed_world();
        let ctx = &mut ctx;
        set_lev(ctx, CAROL, M1, 20);
        fill(ctx, CAROL, M1, 4, 1_000);
        fill(ctx, BOB, M1, -4, 1_000);
        set_type(ctx, ERIN, M1, ISOLATED);
        set_lev(ctx, ERIN, M1, 2);
        fill(ctx, ERIN, M1, -4, 1_000);
        set_wallet(ctx, BOB, bob_wallet as i64);
        let c = tick(ctx, M1, 900);
        assert_eq!(evs::<Liquidation>(&c.logs).len(), 1);
        let adl = evs::<Adl>(&c.logs);
        assert_eq!(adl.iter().map(|a| a.qty).sum::<u64>(), 4);
        assert_eq!(pos(ctx, BOB, M1).margin, 0);
        adl.iter().map(|a| a.adlUser).collect()
    }
    // BOB: 400 / (1,000 + 400) = 0.29 beats ERIN's 400 / (1,998 + 400) = 0.17.
    assert_eq!(run_with(1_000 * USD), vec![BOB]);
    // BOB: 400 / (5,000 + 400) = 0.07 — now ERIN goes first.
    assert_eq!(run_with(5_000 * USD), vec![ERIN]);
}

/// A CROSSED winner the fill would take below zero is NOT a clean candidate, however high its rank:
/// the report's clean tier is `E − take·|fill − mark| ≥ 0` (§3.4 ADL), and here `take` is BOB's
/// whole short, so both readings of it agree. CAROL's long of 4 is liquidated at 900 with an empty
/// book, filled at her bankruptcy price 951 — 51 above the mark, against the shorts. BOB (cross,
/// W = −300): `E = −300 + 400 = 100 > 0` and `uPnL / E = 4`, but `E − 4 · 51 = −104 < 0`. ERIN
/// (isolated, ROE 400 / 2,398) is clean: she is ADL'd first and takes all 4; BOB is untouched.
#[test]
fn liq_adl_prefers_a_clean_winner_to_a_cross_winner_the_fill_takes_negative() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    set_lev(ctx, CAROL, M1, 20);
    fill(ctx, CAROL, M1, 4, 1_000);
    fill(ctx, BOB, M1, -4, 1_000);
    set_type(ctx, ERIN, M1, ISOLATED);
    set_lev(ctx, ERIN, M1, 2);
    fill(ctx, ERIN, M1, -4, 1_000);
    set_wallet(ctx, BOB, (-300 * USD) as i64);
    let bob = pos(ctx, BOB, M1);
    let c = tick(ctx, M1, 900);
    assert_eq!(evs::<Liquidation>(&c.logs).len(), 1);
    let md = model(ctx, BOB);
    assert_eq!(md.e, 100 * USD, "BOB's account is solvent at the mark…");
    let adl = evs::<Adl>(&c.logs);
    assert!(adl.iter().all(|a| a.price == 951), "…but not at the bankruptcy-price fill");
    assert_eq!(adl.iter().map(|a| (a.adlUser, a.qty)).collect::<Vec<_>>(), vec![(ERIN, 4)]);
    assert_eq!(pos(ctx, BOB, M1), bob, "the cross winner was held back to the fallback tier");
}

/// A CROSSED winner whose account equity is not positive (`E ≤ 0` — a loss on another cross
/// market) is held to the fallback tier, and ADL through it is total: no error, no division by a
/// zero margin, its fill settled into W, the identity intact. (Cross P3 review: the fund backs a
/// fallback-tier cross winner — its `E` cannot fall below `min(E, 0)` — so here, `E < 0`, the
/// fill's whole loss off the mark is credited back: W moves by the realized PnL plus that cover,
/// which is the leg's uPnL at the mark, and `E` does not move.)
#[test]
fn liq_adl_against_a_cross_winner_at_non_positive_equity_is_total() {
    let mut ctx = crossed_world();
    let ctx = &mut ctx;
    set_lev(ctx, CAROL, M1, 20);
    fill(ctx, CAROL, M1, 4, 1_000);
    fill(ctx, ALICE, M1, -4, 1_000); // the only short on M1
    fill(ctx, ALICE, M2, 20, 500);
    tick(ctx, M2, 400); // −2,000 on the M2 leg
    set_wallet(ctx, ALICE, (1_500 * USD) as i64);
    let (w0, cr0) = (wallet(ctx, ALICE), pos(ctx, ALICE, M1).cumulative_realized_pnl);
    let c = tick(ctx, M1, 900);
    assert!(model(ctx, ALICE).e < 0 || evs::<Adl>(&c.logs).iter().any(|a| a.adlUser == ALICE));
    let adl = evs::<Adl>(&c.logs);
    assert_eq!(adl.iter().map(|a| a.qty).sum::<u64>(), 4);
    assert!(adl.iter().all(|a| a.adlUser == ALICE));
    assert_eq!((pos(ctx, ALICE, M1).amount, pos(ctx, ALICE, M1).margin), (0, 0));
    let realized = pos(ctx, ALICE, M1).cumulative_realized_pnl - cr0;
    let cover: i128 = adl.iter().map(|a| a.qty as i128 * (a.price as i128 - 900) * USD).sum();
    assert!(cover > 0);
    assert_eq!(wallet(ctx, ALICE) - w0, realized as i128 + cover);
    assert_eq!(model(ctx, ALICE).e, -100 * USD, "E = 1,500 + 400 − 2,000, unmoved by the fill");
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Gated: byte identity
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// A multi-market ISOLATED scenario: leverage, funding on two markets, taker and maker fills with
/// fees, marks one market at a time with epochs, a partial close, a flip, margin added and removed,
/// money out (accepted and refused), a refused leverage cut, a mark walk across M1's brackets,
/// resting orders and cancels, a sweep that liquidates two positions with ADL, a refused manual
/// liquidation and an insurance-fund deposit. Returns every recorded outcome and every getter of
/// every account.
fn isolated_scenario(ctx: &mut TestCtx) -> (Vec<Outcome>, Vec<Vec<u8>>) {
    RECORD.with(|r| *r.borrow_mut() = Some(Vec::new()));
    for (u, levs) in
        [(ALICE, [20, 10, 5]), (BOB, [10, 5, 5]), (CAROL, [10, 5, 5]), (ERIN, [10, 5, 5])]
    {
        for (m, l) in MARKETS.into_iter().zip(levs) {
            set_lev(ctx, u, m, l);
        }
    }
    enable_funding(ctx, M1, 300);
    enable_funding(ctx, M2, -200);
    fill(ctx, ALICE, M1, 20, 1_000);
    make(ctx, ALICE, M2, -30, 500);
    fill(ctx, ALICE, M3, 50, 200);
    make(ctx, BOB, M1, 10, 999);
    fill(ctx, CAROL, M1, 8, 1_001);
    fill(ctx, BOB, M3, 20, 201);
    make(ctx, CAROL, M2, -15, 499);
    tick(ctx, M1, 1_010);
    tick(ctx, M2, 505);
    tick(ctx, M1, 1_020);
    tick(ctx, M3, 198);
    tick(ctx, M2, 510);
    tick(ctx, M1, 1_025);
    fill(ctx, ALICE, M1, -8, 1_030);
    fill(ctx, ALICE, M2, 60, 510);
    ok(ctx, ALICE, &addPositionMarginCall { marketId: M1, amount: 300 * USD as u64 }.abi_encode());
    let _ = attempt(
        ctx,
        ALICE,
        &removePositionMarginCall { marketId: M1, amount: 100 * USD as u64 }.abi_encode(),
    );
    let _ = attempt(ctx, ALICE, &setLeverageCall { marketId: M1, leverage: 10 }.abi_encode());
    let _ = attempt(ctx, ALICE, &transferFromPerpCall { amount: u64::MAX / 4 }.abi_encode());
    ok(ctx, ALICE, &transferFromPerpCall { amount: 1_000 * USD as u64 }.abi_encode());
    fill(ctx, ALICE, M1, 25, 1_025);
    walk_mark(ctx, M1, 1_400, 75);
    rest(ctx, BOB, M1, SELL, 1_450, 5);
    rest(ctx, BOB, M1, BUY, 1_200, 3);
    cancel_all(ctx, BOB, M1);
    fill(ctx, CAROL, M3, 10, 198);
    walk_mark(ctx, M3, 150, 16); // ALICE's and CAROL's M3 longs go under; ADL on an empty book
    let _ = attempt(ctx, ERIN, &liquidateCall { user: BOB, marketId: M1 }.abi_encode());
    ok(ctx, ADMIN, &depositInsuranceFundCall { amount: 1_000 * USD as u64 }.abi_encode());
    fill(ctx, BOB, M1, -2, 1_300);
    let rec = RECORD.with(|r| r.borrow_mut().take()).unwrap();
    let mut views = Vec::new();
    for u in users() {
        views.push(view(ctx, &getAccountCall { user: u }.abi_encode()).bytes.to_vec());
        for m in MARKETS {
            let risk = getPositionRiskCall { user: u, marketId: m }.abi_encode();
            views.push(view(ctx, &risk).bytes.to_vec());
            let sym = getSymbolConfigCall { user: u, marketId: m }.abi_encode();
            views.push(view(ctx, &sym).bytes.to_vec());
        }
    }
    (rec, views)
}

/// With the gate forced OFF (the engine before the flip), the scenario at the stored default
/// (CROSSED, not in force) is byte-identical — every outcome, return value, gas figure and log,
/// every getter, the whole stored world bar the type byte — to the same scenario with the gate ON
/// and every account explicitly ISOLATED. Cross margin changes nothing an isolated account can
/// observe.
#[test]
fn gated_an_isolated_scenario_is_byte_identical_with_the_gate_on_and_everyone_isolated() {
    let (rec_a, views_a, world_a) = {
        let _off = crate::margin_type::test_gate::disable();
        let mut a = fresh();
        let (rec, views) = isolated_scenario(&mut a);
        (rec, views, world_of(&mut a, true))
    };
    let liquidations: usize = rec_a.iter().map(|o| evs::<Liquidation>(&o.3).len()).sum();
    let adls: usize = rec_a.iter().map(|o| evs::<Adl>(&o.3).len()).sum();
    let fundings: usize = rec_a.iter().map(|o| evs::<FundingSettled>(&o.3).len()).sum();
    let refusals = rec_a.iter().filter(|o| o.0).count();
    assert!(liquidations >= 2 && adls >= 1 && fundings >= 4 && refusals >= 2, "scenario coverage");

    let mut b = fresh();
    for u in [ALICE, BOB, CAROL, DAVE, ERIN, ADMIN] {
        for m in MARKETS {
            set_type(&mut b, u, m, ISOLATED);
        }
    }
    let (rec_b, views_b) = isolated_scenario(&mut b);
    let world_b = world_of(&mut b, true);
    assert_eq!(rec_a.len(), rec_b.len());
    for (i, (x, y)) in rec_a.iter().zip(&rec_b).enumerate() {
        assert!(x == y, "call {i} differs (reverted / bytes / gas / logs)");
    }
    assert!(views_a == views_b, "a getter differs");
    assert!(world_a == world_b, "the stored world differs");
}
