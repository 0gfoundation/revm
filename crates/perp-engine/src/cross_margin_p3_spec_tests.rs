//! **Cross margin P3 — an independent spec suite: cross liquidation, the episode, ADL, the
//! negative-wallet clearance, and a never-`Err` fuzz.**
//!
//! Written from `SPEC_cross_margin.md` (owner decisions Q1–Q12, the P3 decisions R1–R8) and the
//! design report `CROSS_MARGIN_explore.md` (§1.5–§1.7 Binance's liquidation and ADL facts, §3.0
//! grafts J1–J7, §3.2 the formulas, §3.4 detection / the episode / ADL, §4 deviations), NOT from
//! the implementation's own tests (`cross_liquidation_tests`, `cross_golden_tests`) nor from its
//! internals. Every action goes through the full shell (`run_perp_dex_call`); state is only READ
//! directly, to feed the model its inputs, plus a few fixture writes between calls (a wallet, the
//! block clock, the last traded price before an oracle tick so that the mark is exactly the index).
//!
//! # The model, written from the formulas (§3.2, §3.4)
//!
//! ```text
//! N_k        trunc(amount_k · mark_k)                      (quote units, 6 dp)
//! uPnL_k     N_k + vQuote_k
//! PF_k       −trunc(amount_k · (I_k − anchor_k) / 10^(bd+pd))  (the FundingState index)
//! MM_k       the slice form over the tier table set through the ABI (Binance N·MMR − cum)
//! E          W + Σ_C (PF_k + uPnL_k)                       cross margin balance
//! ΣMM        Σ_C MM_k
//! liquidatable (cross)     E < ΣMM, account-level
//! liquidatable (isolated)  margin_k + PF_k + uPnL_k < MM_k
//! share_k    trunc(E · MM_k / ΣMM)                         the MM-proportional share (Q5, D4)
//! BP_k       the price at which closing the whole position realizes uPnL_k − share_k, rounded
//!            in the account's favour; the IOC limit is BP_k clamped into the mark's band (Q4)
//! fee        trunc(notional(mark, closed) · rate / 10^4), cross cap max(0, E_after), isolated
//!            cap the close credit (Q3, J5)
//! LP1        (WB − TMM1 + UPNL1 + cumB − Side·Pos·EP) / (Pos·MMR_B − Side·Pos)   (§1.5)
//! ```
//!
//! # What every call is held to ([`World::attempt`])
//!
//! * a REFUSED call: zero perp writes, no log, a bit-identical world;
//! * an ACCEPTED call: the custody identity `Σ (spot + W + Σ (margin + vQuote + PF)) + IF` moves
//!   by exactly what the insurance fund could not cover (`InsuranceFundDepleted.badDebt`, the one
//!   socialised amount) — so a liquidation, a fee, an ADL fill, a funding epoch and a clearance all
//!   conserve value to the unit; `Σ amount == 0` per market; `open_position_count` counts the open
//!   positions; a position CROSSED in force holds no margin; a FLAT account's wallet is never below
//!   zero (Q8: cleared at the end of the call that closed its last position); a FLAGGED account
//!   holds a cross position (the flag never outlives the last one).
//!
//! # Map (test prefix → spec item)
//!
//! * `lp_*` — Binance's two published cross liquidation-price examples (§1.5, F15): the model
//!   reproduces Binance's numbers, and the episode starts exactly one tick below LP1.
//! * `det_*` — detection is account-level (E < ΣMM), on the tick of any market the account holds.
//! * `ep_*` — the episode: cancel-all scope (Q2, R3), the close order (D2), the MM-share IOC limit
//!   and the band clamp (Q4, Q5), the stale guard (Q10), the partial stop (Q6), the fee caps (Q3,
//!   J5), the ADL remainder at the mark or BP (R5), the flag (R6), continuation across ticks, by
//!   the drain and by `liquidate()`.
//! * `frz_*` — the `-2023` matrix of an account an episode really flagged (Q11). The maker freeze
//!   and its reduce-only exemption (R1) are exercised where an episode leaves orders behind
//!   (`ep_out_of_budget_…`) and in the flag's own test.
//! * `rest_*` — R2, J1 at rest.
//! * `adl_*` — R5: the sign-split cross rank, the per-fill clean tier, D7.
//! * `clr_*` — Q8 / R4: the negative-wallet clearance on every route, never with a position left,
//!   and a short insurance fund.
//! * `fuzz_*` — the never-`Err` randomized multi-market runs — five markets, sixteen markets, and a
//!   crowd that exhausts the sweep's step budget — and their determinism (`P3_FUZZ_SEEDS` /
//!   `P3_FUZZ_STEPS` and the per-test variables for a long run).

use alloy_sol_types::{SolCall, SolError, SolEvent};
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use ed25519_dalek::{Signer, SigningKey};
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::call::PerpOutput;
use crate::host::PerpHost;
use crate::interface::IPerpDex::{
    batchPlaceOrdersCall, cancelAlgoOrderCall, cancelOrderCall, drainDeferredWorkCall,
    getAccountCall, getAccountReturn, getPositionRiskCall, liquidateCall, placeAlgoOrderCall,
    placeOrderCall, placeOrderSignedCall, registerApiKeyCall, setLeverageCall, setMarginTiersCall,
    setMarginTypeCall, transferFromPerpCall, transferToPerpCall, updateIndexPriceCall,
    updateMarketCall, AccountBalanceChanged, AccountPosition, Adl, AlgoOrderCancelled,
    AlgoOrderPlaced, CrossLiquidationEnded, CrossLiquidationStarted, InsuranceFundChanged,
    InsuranceFundDepleted, Liquidation, OrderCancelled, OrderPlaced, PlaceItem, Trade,
};
use crate::types::{MarginTiers, Market};
use crate::{run_perp_dex_call, storage, PERP_DEX_ADDRESS, USDC_ADDRESS};

// ── Fixture ──────────────────────────────────────────────────────────────────────────────────

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;
type Log = primitives::Log;
type Id = [u8; 32];

/// One dollar in quote units (6 dp).
const USD: i128 = 1_000_000;
const GAS: u64 = 30_000_000;
/// $1e9.
const BIG: i128 = 1_000_000_000 * USD;

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
/// Deep liquidity everywhere: ISOLATED at leverage 1 with a $1e9 wallet, fee-free.
const DAVE: Address = address!("4444444444444444444444444444444444444444");
/// A keeper (`liquidate()`, `drainDeferredWork`) and a bystander.
const ERIN: Address = address!("5555555555555555555555555555555555555555");
/// Admin = oracle.
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MKT: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;
const RO: u8 = 0b01;
const CLOSE_POSITION: u8 = 0b10;
const CROSSED: u8 = 0;
const ISOLATED: u8 = 1;
/// `OrderCancelled.reason` / `AlgoOrderCancelled.reason`: 3 = Liquidation, 4 = Adl.
const CANCEL_LIQUIDATION: u8 = 3;
/// `AccountBalanceChanged.reason` 7 = INSURANCE_CLEAR.
const INSURANCE_CLEAR: u8 = 7;
/// Binance `-2023` batch reason.
const BATCH_IN_LIQUIDATION: u8 = 35;

const E_2023: &str = "placeOrder: -2023 User in liquidation mode now. Only reduce-only and \
                      closePosition orders are accepted";
const E_2023_MT: &str = "setMarginType: -2023 User in liquidation mode now.";
/// J1 / R2: the open would leave (or rest under) the cross maintenance margin.
const E_J1: &str = "placeOrder: -2019 Margin is insufficient. The open would leave the cross \
                    margin balance below the cross maintenance margin";
const E_LIQ_HEALTHY: &str = "liquidate: account is above cross maintenance margin";
const E_LIQ_NO_POSITION: &str = "liquidate: no open position";
const E_LIQ_ISO_HEALTHY: &str = "liquidate: position is above maintenance margin";

/// The report's step budgets (§3.4): 50 per sweep (the existing `MAX_SWEEP_ACTIONS_PER_UPDATE`),
/// a fresh 16 for a manual `liquidate()`.
const SWEEP_STEPS: usize = 50;
const MANUAL_STEPS: usize = 16;
/// Owner decision Q10.
const STALE_MULT: u64 = 3;

/// One tier row as `setMarginTiers` takes it: `lb` in whole dollars, `ppm == 0` = the derived rate
/// `1 / (2 · lev)`.
#[derive(Clone, Copy, Debug)]
struct Band {
    lb: u64,
    lev: u32,
    ppm: u32,
}

/// A market as a scenario declares it.
#[derive(Clone, Debug)]
struct Spec {
    id: u64,
    bd: u32,
    pd: u32,
    mark0: u64,
    tiers: Vec<Band>,
    interval: u64,
    band_bps: u32,
    fee_bps: u32,
    /// `Some(interest ppm)`: a funding epoch on every tick.
    funding: Option<i64>,
}

impl Spec {
    /// A whole-dollar market (base and price decimals 0): one contract at price `p` is worth
    /// `p · 10^6` quote units, every value is an exact integer.
    fn whole(id: u64, mark0: u64, tiers: &[Band]) -> Self {
        Spec {
            id,
            bd: 0,
            pd: 0,
            mark0,
            tiers: tiers.to_vec(),
            interval: 15,
            band_bps: 10_000,
            fee_bps: 0,
            funding: None,
        }
    }
}

/// 5 % maintenance (derived, leverage up to 10).
const T10: [Band; 1] = [Band { lb: 0, lev: 10, ppm: 0 }];

fn user(i: u32) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0x77;
    b[16..20].copy_from_slice(&i.to_be_bytes());
    Address::from(b)
}

struct World {
    ctx: TestCtx,
    users: Vec<Address>,
    specs: Vec<Spec>,
    /// The block clock.
    now: u64,
    /// The return bytes of the last call made through [`World::attempt`] (the id decoders read
    /// it, so `attempt` can keep returning just the logs).
    last_out: Vec<u8>,
}

fn take_logs(ctx: &mut TestCtx) -> Vec<Log> {
    JournalTr::take_logs(ctx.journal_mut())
}

fn revert_reason(out: &PerpOutput) -> String {
    alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason).unwrap_or_else(|_| {
        format!("<undecodable revert {:?}>", out.bytes)
    })
}

fn evs<E: SolEvent>(logs: &[Log]) -> Vec<E> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|l| E::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .collect()
}

fn is<E: SolEvent>(l: &Log) -> bool {
    l.data.topics().first() == Some(&E::SIGNATURE_HASH)
}

fn order(m: u64, side: u8, price: u64, qty: u64, kind: u8, tif: u8, flags: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: m,
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

fn gtc(m: u64, side: u8, price: u64, qty: u64) -> Vec<u8> {
    order(m, side, price, qty, LIMIT, GTC, 0)
}

fn gtc_ro(m: u64, side: u8, price: u64, qty: u64) -> Vec<u8> {
    order(m, side, price, qty, LIMIT, GTC, RO)
}

fn mkt(m: u64, side: u8, qty: u64) -> Vec<u8> {
    order(m, side, 0, qty, MKT, GTC, 0)
}

fn mkt_ro(m: u64, side: u8, qty: u64) -> Vec<u8> {
    order(m, side, 0, qty, MKT, GTC, RO)
}

/// A STOP_MARKET (`orderType 1`) conditional: `flags` RO or CLOSE_POSITION.
fn stop_market(m: u64, side: u8, trigger: u64, qty: u64, flags: u8) -> Vec<u8> {
    placeAlgoOrderCall {
        algoType: 0,
        marketId: m,
        side,
        orderType: 1,
        triggerPrice: trigger,
        price: 0,
        quantity: qty,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

/// A TRAILING_STOP_MARKET (`orderType 4`), reduce-only, activating at once, callback `bps`.
fn trailing(m: u64, side: u8, qty: u64, bps: u64) -> Vec<u8> {
    placeAlgoOrderCall {
        algoType: 0,
        marketId: m,
        side,
        orderType: 4,
        triggerPrice: 0,
        price: bps,
        quantity: qty,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags: RO,
    }
    .abi_encode()
}

fn liquidate(u: Address, m: u64) -> Vec<u8> {
    liquidateCall { user: u, marketId: m }.abi_encode()
}

fn drain(m: u64) -> Vec<u8> {
    drainDeferredWorkCall { marketId: m }.abi_encode()
}

// ── The model (§3.2) ─────────────────────────────────────────────────────────────────────────

/// `p · q · 10^6 / 10^(pd + bd)`, truncated toward zero (`q` signed).
fn value(p: u64, q: i128, bd: u32, pd: u32) -> i128 {
    p as i128 * q * USD / 10i128.pow(bd + pd)
}

/// The slice form (D10): every bracket the notional reaches adds `⌊width · rate⌋`.
fn slice_mm(tiers: &[Band], n: i128) -> i128 {
    let n = n.abs();
    let mut mm = 0;
    for (i, b) in tiers.iter().enumerate() {
        let lo = b.lb as i128 * USD;
        if n <= lo {
            break;
        }
        let hi = tiers.get(i + 1).map_or(i128::MAX, |x| x.lb as i128 * USD);
        let w = n.min(hi) - lo;
        mm += if b.ppm == 0 { w / (2 * b.lev as i128) } else { w * b.ppm as i128 / 1_000_000 };
    }
    mm
}

/// The bracket rate (ppm) and Binance's `cum` (quote units) of the bracket holding `n`:
/// `cum_n = cum_{n−1} + floor_n · (MMR_n − MMR_{n−1})` (§1.4), so `MM = N · MMR − cum`.
fn bracket(tiers: &[Band], n: i128) -> (i128, i128) {
    let rate = |b: &Band| if b.ppm == 0 { 1_000_000 / (2 * b.lev as i128) } else { b.ppm as i128 };
    let (mut r, mut cum) = (rate(&tiers[0]), 0);
    for b in &tiers[1..] {
        if n.abs() <= b.lb as i128 * USD {
            break;
        }
        cum += b.lb as i128 * USD * (rate(b) - r) / 1_000_000;
        r = rate(b);
    }
    (r, cum)
}

#[derive(Clone, Copy, Debug, Default)]
struct Leg {
    market: u64,
    cross: bool,
    amount: i64,
    vq: i64,
    margin: i64,
    mark: u64,
    n: i128,
    upnl: i128,
    mm: i128,
    pf: i128,
}

impl Leg {
    /// An ISOLATED position below its own maintenance (`margin + PF + uPnL < MM`).
    fn iso_liquidatable(&self) -> bool {
        !self.cross && self.amount != 0 && (self.margin as i128 + self.pf + self.upnl) < self.mm
    }
}

#[derive(Clone, Debug, Default)]
struct Acct {
    w: i128,
    spot: i128,
    flagged: bool,
    open_count: u8,
    legs: Vec<Leg>,
    e: i128,
    cmm: i128,
    has_cross: bool,
}

impl Acct {
    fn leg(&self, m: u64) -> &Leg {
        self.legs.iter().find(|l| l.market == m).unwrap()
    }

    /// `E < ΣMM` with a cross position open — the account-level condition (strict).
    fn below(&self) -> bool {
        self.has_cross && self.e < self.cmm
    }

    fn flat(&self) -> bool {
        self.legs.iter().all(|l| l.amount == 0)
    }

    /// The MM-proportional share of `E` (Q5): `trunc(E · MM_k / ΣMM)`, all of `E` when no leg is
    /// priced at a maintenance margin.
    fn share(&self, m: u64) -> i128 {
        if self.cmm > 0 {
            self.e * self.leg(m).mm / self.cmm
        } else {
            self.e
        }
    }
}

impl World {
    // ── construction ──

    fn new(specs: Vec<Spec>) -> World {
        let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        for a in [USDC_ADDRESS, PERP_DEX_ADDRESS, ADMIN, Address::ZERO] {
            JournalTr::load_account(ctx.journal_mut(), a).unwrap();
        }
        storage::save_admin(&mut ctx, ADMIN).unwrap();
        for s in &specs {
            storage::save_market(
                &mut ctx,
                &Market {
                    market_id: s.id,
                    base_decimals: s.bd,
                    price_decimals: s.pd,
                    tick_size: 1,
                    step_size: 1,
                    min_quantity: 1,
                    // `maxPrice · maxQuantity` stays a valid `i64` value (`updateMarket` checks).
                    max_quantity: 1_000_000 * 10u64.pow(s.bd),
                    max_price: 1_000_000 * 10u64.pow(s.pd),
                    price_update_interval: s.interval,
                    active: true,
                    funding_interval: 0,
                    interest_rate: 0,
                    liquidation_fee_rate_bps: s.fee_bps,
                    price_band_bps: s.band_bps,
                    mark_price: 0,
                    tiers: MarginTiers::default(),
                    basis_mode: 0,
                    deferred_work: 0,
                    cumulative_funding_index: 0,
                },
            )
            .unwrap();
            storage::save_mark_price(&mut ctx, s.id, s.mark0).unwrap();
        }
        storage::save_insurance_fund(&mut ctx, BIG as u64).unwrap();
        storage::flush_account_snapshots(&mut ctx).unwrap();
        let _ = take_logs(&mut ctx);
        let mut w = World { ctx, users: Vec::new(), specs, now: 1_000_005, last_out: Vec::new() };
        w.ctx.block.timestamp = U256::from(w.now);
        w.enroll(ADMIN, 0);
        w.enroll(DAVE, BIG);
        for s in w.specs.clone() {
            w.ok(
                ADMIN,
                &setMarginTiersCall {
                    marketId: s.id,
                    lowerBounds: s.tiers.iter().map(|b| b.lb * USD as u64).collect(),
                    maxLeverages: s.tiers.iter().map(|b| b.lev).collect(),
                    maintMarginRatesPpm: s.tiers.iter().map(|b| b.ppm).collect(),
                    notionalCap: 0,
                }
                .abi_encode(),
            );
            if let Some(interest) = s.funding {
                w.update_market(s.id, |c| {
                    c.fundingInterval = s.interval;
                    c.interestRate = interest;
                });
            }
            w.set_type(DAVE, s.id, ISOLATED);
            w.tick(s.id, s.mark0);
        }
        w
    }

    fn spec(&self, m: u64) -> &Spec {
        self.specs.iter().find(|s| s.id == m).unwrap()
    }

    fn markets(&self) -> Vec<u64> {
        self.specs.iter().map(|s| s.id).collect()
    }

    /// Fixture: `u` holds `wallet` in its perp wallet and joins the conservation set.
    fn enroll(&mut self, u: Address, wallet: i128) {
        JournalTr::load_account(self.ctx.journal_mut(), u).unwrap();
        if !self.users.contains(&u) {
            self.users.push(u);
        }
        self.set_wallet(u, wallet);
    }

    /// Fixture only: overwrite `u`'s perp wallet between calls.
    fn set_wallet(&mut self, u: Address, w: i128) {
        storage::mutate_account(&mut self.ctx, u, |a| a.perp_wallet_balance = w as i64).unwrap();
        storage::flush_account_snapshots(&mut self.ctx).unwrap();
        let _ = take_logs(&mut self.ctx);
    }

    /// Fixture only: `u`'s spot (`usdcBalance`).
    fn set_spot(&mut self, u: Address, spot: u128) {
        storage::mutate_account(&mut self.ctx, u, |a| a.usdc_balance = U256::from(spot).into())
            .unwrap();
        storage::flush_account_snapshots(&mut self.ctx).unwrap();
        let _ = take_logs(&mut self.ctx);
    }

    /// Fixture only: the insurance fund.
    fn set_fund(&mut self, v: u64) {
        storage::save_insurance_fund(&mut self.ctx, v).unwrap();
    }

    fn set_now(&mut self, t: u64) {
        self.now = t;
        self.ctx.block.timestamp = U256::from(t);
    }

    // ── reads ──

    fn pos(&mut self, u: Address, m: u64) -> crate::types::PerpPosition {
        storage::load_position(&mut self.ctx, u, m).unwrap()
    }

    fn wallet(&mut self, u: Address) -> i128 {
        storage::load_account(&mut self.ctx, u).unwrap().perp_wallet_balance as i128
    }

    fn fund(&mut self) -> i128 {
        storage::load_insurance_fund(&mut self.ctx).unwrap() as i128
    }

    fn flagged(&mut self, u: Address) -> bool {
        storage::load_account(&mut self.ctx, u).unwrap().in_liquidation()
    }

    fn mark(&mut self, m: u64) -> u64 {
        storage::load_mark_price(&mut self.ctx, m).unwrap()
    }

    fn deferred(&mut self, m: u64) -> bool {
        storage::load_market_ref(&mut self.ctx, m).unwrap().unwrap().deferred_work != 0
    }

    fn open_orders(&mut self, u: Address, m: u64) -> usize {
        storage::load_buy_orders(&mut self.ctx, u, m).unwrap().len()
            + storage::load_sell_orders(&mut self.ctx, u, m).unwrap().len()
    }

    fn open_algos(&mut self, u: Address, m: u64) -> usize {
        storage::load_user_algo_orders(&mut self.ctx, u, m).unwrap().len()
    }

    fn leg(&mut self, u: Address, m: u64) -> Leg {
        let s = self.spec(m).clone();
        let p = self.pos(u, m);
        let mark = self.mark(m);
        let index = storage::load_funding_state(&mut self.ctx, m).unwrap().cumulative_funding_index;
        let cross = p.margin_type == CROSSED;
        let n = value(mark, p.amount as i128, s.bd, s.pd);
        let pf = if p.amount == 0 {
            0
        } else {
            -(p.amount as i128 * (index - p.last_funding_index) / 10i128.pow(s.bd + s.pd))
        };
        Leg {
            market: m,
            cross,
            amount: p.amount,
            vq: p.v_quote_balance,
            margin: p.margin,
            mark,
            n,
            upnl: if mark == 0 && cross { 0 } else { n + p.v_quote_balance as i128 },
            mm: slice_mm(&s.tiers, n),
            pf,
        }
    }

    fn acct(&mut self, u: Address) -> Acct {
        let a = storage::load_account(&mut self.ctx, u).unwrap();
        let spot: U256 = a.usdc_balance.clone().into();
        let mut md = Acct {
            w: a.perp_wallet_balance as i128,
            spot: spot.to::<u128>() as i128,
            flagged: a.in_liquidation(),
            open_count: a.open_position_count,
            ..Acct::default()
        };
        for m in self.markets() {
            let l = self.leg(u, m);
            if l.cross {
                md.e += l.pf + l.upnl;
                md.cmm += l.mm;
                md.has_cross |= l.amount != 0;
            }
            md.legs.push(l);
        }
        md.e += md.w;
        md
    }

    /// `Σ (spot + W + Σ (margin + vQuote + PF)) + IF`.
    fn custody(&mut self) -> i128 {
        let mut v = self.fund();
        for u in self.users.clone() {
            let a = self.acct(u);
            v += a.spot
                + a.w
                + a.legs.iter().map(|l| l.margin as i128 + l.vq as i128 + l.pf).sum::<i128>();
        }
        v
    }

    /// Everything a refused call must leave bit-identical.
    fn snapshot(&mut self) -> Vec<String> {
        let mut s = Vec::new();
        for u in self.users.clone() {
            s.push(format!("{:?}", storage::load_account(&mut self.ctx, u).unwrap()));
            for m in self.markets() {
                s.push(format!("{:?}", self.pos(u, m)));
                s.push(format!("{:?}", storage::load_buy_orders(&mut self.ctx, u, m).unwrap()));
                s.push(format!("{:?}", storage::load_sell_orders(&mut self.ctx, u, m).unwrap()));
                let algos = storage::load_user_algo_orders(&mut self.ctx, u, m).unwrap();
                s.push(format!("{algos:?}"));
            }
        }
        for m in self.markets() {
            s.push(format!("{:?}", storage::load_market_ref(&mut self.ctx, m).unwrap()));
            s.push(format!("{:?}", storage::load_index_price_state(&mut self.ctx, m).unwrap()));
            s.push(format!("{:?}", storage::load_funding_state(&mut self.ctx, m).unwrap()));
        }
        s.push(format!("IF {}", self.fund()));
        s
    }

    /// The stored invariants every accepted call must leave.
    fn check_invariants(&mut self, at: &str) {
        for m in self.markets() {
            let mut net = 0i128;
            for u in self.users.clone() {
                net += self.pos(u, m).amount as i128;
            }
            assert_eq!(net, 0, "{at}: Σ amount on market {m}");
        }
        for u in self.users.clone() {
            let a = self.acct(u);
            let open = a.legs.iter().filter(|l| l.amount != 0).count() as u8;
            assert_eq!(a.open_count, open, "{at}: {u} open_position_count");
            for l in &a.legs {
                if l.cross {
                    assert_eq!(l.margin, 0, "{at}: {u} CROSSED on {} holds margin", l.market);
                }
            }
            if a.flat() {
                assert!(a.w >= 0, "{at}: {u} is flat with a negative wallet {} (Q8)", a.w);
            }
            if a.flagged {
                assert!(a.has_cross, "{at}: {u} is flagged without a cross position");
            }
            // Cross P4: the account's cross-leg summary is exactly its non-zero CROSSED positions'
            // state, in market order — derived here from the position blobs, not by the engine.
            let walked: Vec<crate::types::CrossLegState> = self
                .markets()
                .into_iter()
                .map(|m| (m, self.pos(u, m)))
                .filter(|(_, p)| p.margin_type == CROSSED && p.amount != 0)
                .map(|(m, p)| crate::types::CrossLegState {
                    market_id: m,
                    amount: p.amount,
                    v_quote_balance: p.v_quote_balance,
                    last_funding_index: p.last_funding_index,
                })
                .collect();
            let stored = storage::load_cross_legs_ref(&mut self.ctx, u).unwrap();
            assert_eq!(stored.0, walked, "{at}: {u} cross legs");
            // …and the health check priced from it is the model's account (every leg priceable
            // here, so none is faulted).
            let h = crate::margin_view::cross_health(&mut self.ctx, u, None).unwrap();
            assert_eq!(
                (h.e, h.cross_mm, h.has_cross, h.faulted_legs),
                (a.e, a.cmm, a.has_cross, 0),
                "{at}: {u} cross health"
            );
        }
    }

    // ── the shell ──

    fn run(&mut self, caller: Address, input: &[u8]) -> PerpOutput {
        run_perp_dex_call(input, GAS, caller, U256::ZERO, false, &mut self.ctx).unwrap()
    }

    fn view(&mut self, input: &[u8]) -> PerpOutput {
        run_perp_dex_call(input, GAS, ERIN, U256::ZERO, true, &mut self.ctx).unwrap()
    }

    /// One shell call, checked whichever way it goes — see the module docs.
    fn attempt(&mut self, caller: Address, input: &[u8]) -> Result<Vec<Log>, String> {
        let _ = take_logs(&mut self.ctx);
        let before_world = self.snapshot();
        let writes = self.ctx.perp_write_count();
        let before = self.custody();
        let out = self.run(caller, input);
        let logs = take_logs(&mut self.ctx);
        self.last_out = out.bytes.to_vec();
        let sel: [u8; 4] = input[..4].try_into().unwrap();
        if out.reverted {
            let reason = revert_reason(&out);
            assert_eq!(self.ctx.perp_write_count(), writes, "[{reason}] the refused call wrote");
            assert!(logs.is_empty(), "[{reason}] the refused call logged");
            assert!(self.snapshot() == before_world, "[{reason}] the refused call moved state");
            return Err(reason);
        }
        let socialised: i128 =
            evs::<InsuranceFundDepleted>(&logs).iter().map(|d| d.badDebt as i128).sum();
        let after = self.custody();
        assert_eq!(
            after - before,
            socialised,
            "custody moved by other than the socialised shortfall: caller {caller} selector {sel:?}"
        );
        self.check_invariants(&format!("after {sel:?} by {caller}"));
        Ok(logs)
    }

    fn ok(&mut self, caller: Address, input: &[u8]) -> Vec<Log> {
        self.attempt(caller, input).unwrap_or_else(|e| panic!("unexpected revert: {e}"))
    }

    fn rejected(&mut self, caller: Address, input: &[u8], expect: &str) {
        match self.attempt(caller, input) {
            Ok(_) => panic!("expected the revert `{expect}`, but the call SUCCEEDED"),
            Err(e) => assert_eq!(e, expect),
        }
    }

    // ── shortcuts ──

    fn set_type(&mut self, u: Address, m: u64, t: u8) {
        self.ok(u, &setMarginTypeCall { marketId: m, marginType: t }.abi_encode());
    }

    fn set_lev(&mut self, u: Address, m: u64, l: u64) {
        self.ok(u, &setLeverageCall { marketId: m, leverage: l }.abi_encode());
    }

    /// `updateMarket` through the ABI, every field as stored except what `f` changes.
    fn update_market(&mut self, m: u64, f: impl FnOnce(&mut updateMarketCall)) {
        let mk = storage::load_market_ref(&mut self.ctx, m).unwrap().unwrap();
        let mut c = updateMarketCall {
            marketId: m,
            tickSize: mk.tick_size,
            stepSize: mk.step_size,
            minQuantity: mk.min_quantity,
            maxQuantity: mk.max_quantity,
            maxPrice: mk.max_price,
            priceUpdateInterval: mk.price_update_interval,
            active: mk.active,
            fundingInterval: mk.funding_interval,
            interestRate: mk.interest_rate,
            liquidationFeeRateBps: mk.liquidation_fee_rate_bps,
            priceBandBps: mk.price_band_bps,
        };
        f(&mut c);
        self.ok(ADMIN, &c.abi_encode());
        if let Some(s) = self.specs.iter_mut().find(|s| s.id == m) {
            s.fee_bps = c.liquidationFeeRateBps;
            s.band_bps = c.priceBandBps;
        }
    }

    fn set_fee_rate(&mut self, m: u64, bps: u32) {
        self.update_market(m, |c| c.liquidationFeeRateBps = bps);
    }

    fn set_band(&mut self, m: u64, bps: u32) {
        self.update_market(m, |c| c.priceBandBps = bps);
    }

    /// A GTC that must rest untouched; returns its id.
    fn rest(&mut self, u: Address, m: u64, side: u8, price: u64, qty: u64) -> Id {
        let logs = self.ok(u, &gtc(m, side, price, qty));
        assert!(evs::<Trade>(&logs).is_empty(), "{u} {side}@{price} traded while resting");
        placeOrderCall::abi_decode_returns(&self.last_out).unwrap().0
    }

    /// `taker` trades `qty` (signed: + buys) at `price` against liquidity DAVE rests there.
    fn fill(&mut self, taker: Address, m: u64, qty: i64, price: u64) -> Vec<Log> {
        let (maker_side, taker_side) = if qty > 0 { (SELL, BUY) } else { (BUY, SELL) };
        self.rest(DAVE, m, maker_side, price, qty.unsigned_abs());
        let logs = self.ok(taker, &mkt(m, taker_side, qty.unsigned_abs()));
        let t = evs::<Trade>(&logs);
        assert_eq!(t.iter().map(|t| t.quantity).sum::<u64>(), qty.unsigned_abs(), "filled");
        assert!(t.iter().all(|t| t.price == price && t.taker == taker && t.maker == DAVE));
        logs
    }

    /// `maker` rests `qty` (signed) at `price` (`flags` RO or 0) and DAVE takes it.
    fn make(&mut self, maker: Address, m: u64, qty: i64, price: u64, flags: u8) -> Vec<Log> {
        let (maker_side, taker_side) = if qty > 0 { (BUY, SELL) } else { (SELL, BUY) };
        self.ok(maker, &order(m, maker_side, price, qty.unsigned_abs(), LIMIT, GTC, flags));
        let logs = self.ok(DAVE, &mkt(m, taker_side, qty.unsigned_abs()));
        let t = evs::<Trade>(&logs);
        assert_eq!(t.iter().map(|t| t.quantity).sum::<u64>(), qty.unsigned_abs(), "made");
        assert!(t.iter().all(|t| t.price == price && t.maker == maker));
        logs
    }

    /// The oracle publishes `index` for `m` one interval after its last publication (or at the
    /// block clock, whichever is later). The last traded price is cleared first, so the mark is
    /// exactly the index — asserted.
    fn tick(&mut self, m: u64, index: u64) -> Vec<Log> {
        let logs = self.tick_raw(m, index, true);
        assert_eq!(self.mark(m), index, "the tick set the mark");
        logs
    }

    fn tick_raw(&mut self, m: u64, index: u64, reset_last: bool) -> Vec<Log> {
        if reset_last {
            storage::save_last_traded_price(&mut self.ctx, m, 0).unwrap();
        }
        let interval = self.spec(m).interval;
        let last = storage::load_index_price_state(&mut self.ctx, m).unwrap().timestamp;
        let ts = (last + interval).max(self.now - self.now % interval);
        self.ok(
            ADMIN,
            &updateIndexPriceCall { marketId: m, indexPrice: index, timestamp: ts }.abi_encode(),
        )
    }

    /// A tick stamped at exactly `ts` (the oracle's own clock).
    fn tick_at(&mut self, m: u64, index: u64, ts: u64) -> Vec<Log> {
        storage::save_last_traded_price(&mut self.ctx, m, 0).unwrap();
        let logs = self.ok(
            ADMIN,
            &updateIndexPriceCall { marketId: m, indexPrice: index, timestamp: ts }.abi_encode(),
        );
        assert_eq!(self.mark(m), index, "the tick set the mark");
        logs
    }

    /// Every resting order of `u` on `m`, cancelled.
    fn cancel_all(&mut self, u: Address, m: u64) {
        let ids: Vec<Id> = storage::load_buy_orders(&mut self.ctx, u, m)
            .unwrap()
            .iter()
            .chain(storage::load_sell_orders(&mut self.ctx, u, m).unwrap().iter())
            .map(|e| e.order_id)
            .collect();
        for id in ids {
            self.ok(u, &cancelOrderCall { orderId: id.into(), marketId: m }.abi_encode());
        }
    }

    fn account_view(&mut self, u: Address) -> getAccountReturn {
        let out = self.view(&getAccountCall { user: u }.abi_encode());
        assert!(!out.reverted, "getAccount reverted: {}", revert_reason(&out));
        getAccountCall::abi_decode_returns(&out.bytes).unwrap()
    }

    fn risk(&mut self, u: Address, m: u64) -> AccountPosition {
        let out = self.view(&getPositionRiskCall { user: u, marketId: m }.abi_encode());
        assert!(!out.reverted, "getPositionRisk reverted: {}", revert_reason(&out));
        getPositionRiskCall::abi_decode_returns(&out.bytes).unwrap()
    }

    /// The model's `E` and `Σ MM` against `getAccount`'s.
    fn check_view(&mut self, u: Address) {
        let a = self.acct(u);
        let v = self.account_view(u);
        assert_eq!(v.crossMarginBalance as i128, a.e.clamp(i64::MIN as i128, i64::MAX as i128));
        assert_eq!(v.totalCrossMaintMargin as i128, a.cmm, "{u} totalCrossMaintMargin");
    }

    /// The model's bankruptcy price of `u`'s whole position on `m` at the MM share (§3.2), before
    /// the band.
    fn bp(&mut self, u: Address, m: u64) -> u64 {
        let a = self.acct(u);
        let s = self.spec(m).clone();
        let l = *a.leg(m);
        let target = l.n - a.share(m);
        let amt = l.amount as i128;
        let v = |p: u64| value(p, amt, s.bd, s.pd);
        if amt > 0 {
            if target <= 0 {
                return 0;
            }
            // The least price whose close realizes at least the target.
            let mut p = (target * 10i128.pow(s.bd + s.pd) / (amt * USD)) as u64;
            while p > 0 && v(p - 1) >= target {
                p -= 1;
            }
            while v(p) < target {
                p += 1;
            }
            p
        } else {
            if target >= 0 {
                return 0;
            }
            // The greatest price a short can buy back at without realizing more than the target.
            let mut p = (target * 10i128.pow(s.bd + s.pd) / (amt * USD)) as u64;
            while v(p + 1) >= target {
                p += 1;
            }
            while p > 0 && v(p) < target {
                p -= 1;
            }
            p
        }
    }

    /// The fill-time band around the stored mark: `(upper, lower)`.
    fn band(&mut self, m: u64) -> (u64, u64) {
        let mark = self.mark(m) as u128;
        let bps = self.spec(m).band_bps as u128;
        let upper = mark * (10_000 + bps) / 10_000;
        let lower = if bps >= 10_000 { 0 } else { mark * (10_000 - bps) / 10_000 };
        (upper as u64, lower as u64)
    }

    /// The clearance fee due on `qty` closed at the stored mark (Q3), before any cap.
    fn fee_due(&mut self, m: u64, qty: u64) -> i128 {
        let s = self.spec(m).clone();
        let mark = self.mark(m);
        value(mark, qty as i128, s.bd, s.pd) * s.fee_bps as i128 / 10_000
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Binance's published cross liquidation-price examples (§1.5, F15)
// ═════════════════════════════════════════════════════════════════════════════════════════════

const ETH: u64 = 1;
const BTC: u64 = 2;
/// Binance's ETHUSDT brackets of the example (MMR 0.5 / 0.65 / 1 / 2 / 5 / 10 %, cum 0 / 15 / 365 /
/// 5,365 / 35,365 / 135,365), base 3 dp, price 2 dp.
const ETH_TIERS: [Band; 6] = [
    Band { lb: 0, lev: 100, ppm: 5_000 },
    Band { lb: 10_000, lev: 75, ppm: 6_500 },
    Band { lb: 100_000, lev: 50, ppm: 10_000 },
    Band { lb: 500_000, lev: 25, ppm: 20_000 },
    Band { lb: 1_000_000, lev: 10, ppm: 50_000 },
    Band { lb: 2_000_000, lev: 5, ppm: 100_000 },
];
/// Binance's BTCUSDT brackets of the example (MMR 0.4 / 0.5 / 1 / 2.5 %, cum 0 / 50 / 1,300 /
/// 16,300).
const BTC_TIERS: [Band; 4] = [
    Band { lb: 0, lev: 125, ppm: 4_000 },
    Band { lb: 50_000, lev: 100, ppm: 5_000 },
    Band { lb: 250_000, lev: 50, ppm: 10_000 },
    Band { lb: 1_000_000, lev: 20, ppm: 25_000 },
];

/// The example's account (Binance FAQ "How to calculate liquidation price of USDⓈ-M futures",
/// one-way, cross): WB 1,535,443.01; ETHUSDT long 3,683.979 @ 1,456.84 (mark 1,335.18, PNL
/// −448,192.89, MM 356,512.508); BTCUSDT long 109.488 @ 32,481.98 (mark 31,967.27, PNL −56,354.57,
/// MM 71,200.81144). Published: LP(ETH) = 1,153.26, LP(BTC) = 26,316.89.
fn binance_example() -> World {
    let spec = |id, mark0, tiers: &[Band]| Spec {
        id,
        bd: 3,
        pd: 2,
        mark0,
        tiers: tiers.to_vec(),
        interval: 15,
        band_bps: 10_000,
        fee_bps: 0,
        funding: None,
    };
    let mut w = World::new(vec![spec(ETH, 145_684, &ETH_TIERS), spec(BTC, 3_248_198, &BTC_TIERS)]);
    w.enroll(ALICE, 1_535_443_010_000);
    w.set_lev(ALICE, ETH, 5);
    w.set_lev(ALICE, BTC, 20);
    w.fill(ALICE, ETH, 3_683_979, 145_684);
    w.fill(ALICE, BTC, 109_488, 3_248_198);
    assert_eq!(w.wallet(ALICE), 1_535_443_010_000, "WB: no fee, nothing realized");
    w.tick(BTC, 3_196_727);
    w.tick(ETH, 133_518);
    // The model reproduces every number Binance publishes for the account (to its rounding).
    let a = w.acct(ALICE);
    let near = |got: i128, published: i128, tol: i128| (got - published).abs() <= tol;
    assert!(near(a.leg(ETH).upnl, -448_192_890_000, 10_000), "ETH PNL {}", a.leg(ETH).upnl);
    assert!(near(a.leg(BTC).upnl, -56_354_570_000, 10_000), "BTC PNL {}", a.leg(BTC).upnl);
    assert!(near(a.leg(ETH).mm, 356_512_508_000, 1_000), "ETH MM {}", a.leg(ETH).mm);
    assert!(near(a.leg(BTC).mm, 71_200_811_440, 10), "BTC MM {}", a.leg(BTC).mm);
    assert_eq!(bracket(&ETH_TIERS, a.leg(ETH).n), (100_000, 135_365 * USD), "ETH bracket, cumB");
    assert_eq!(bracket(&BTC_TIERS, a.leg(BTC).n), (25_000, 16_300 * USD), "BTC bracket, cumB");
    assert!(!a.below() && !a.flagged);
    w.check_view(ALICE);
    w
}

/// LP1 for `m`, in price units, as an exact fraction `(num, den)` — Binance's one-way cross formula
/// (§1.5) on the model's terms: `WB` the cross wallet, `TMM1` / `UPNL1` the OTHER cross legs'
/// maintenance margin and PnL, `cumB` / `MMR_B` the bracket of `m`'s position at the current mark.
fn lp1(w: &mut World, u: Address, m: u64) -> (i128, i128) {
    let a = w.acct(u);
    let s = w.spec(m).clone();
    let l = *a.leg(m);
    let others = a.legs.iter().filter(|o| o.cross && o.market != m);
    let (tmm1, upnl1): (i128, i128) =
        others.fold((0, 0), |(t, p), o| (t + o.mm, p + o.upnl + o.pf));
    let (mmr, cum) = bracket(&s.tiers, l.n);
    let side = l.amount.signum() as i128;
    // `Side·Pos·EP` is the entry notional, `−vQuote`, signed with the side.
    let q = a.w + l.pf - tmm1 + upnl1 + cum + l.vq as i128;
    // LP = q / (Pos · (MMR − Side)) in dollars per contract; in price units × 10^(bd + pd) / 10^6.
    let num = q * 10i128.pow(s.bd + s.pd);
    let den = l.amount as i128 * (mmr - side * 1_000_000);
    (num, den)
}

/// The engine's liquidation boundary for a LONG on the tick grid: the greatest price at which the
/// account is liquidatable is `⌊LP1⌋`; one tick up it is not.
fn binance_lp_boundary(m: u64, published: u64) {
    let mut w = binance_example();
    let (num, den) = lp1(&mut w, ALICE, m);
    let lp_floor = (num / den) as u64;
    // Binance displays LP1 rounded half-up to the price precision.
    let rounded = ((2 * num + den) / (2 * den)) as u64;
    assert_eq!(rounded, published, "LP1 reproduces Binance's published liquidation price");
    // The row reports the same boundary (§3.5: a cross row's LP is LP1).
    assert_eq!(w.risk(ALICE, m).liquidationPrice, lp_floor, "getPositionRisk liquidationPrice");

    // Walk the mark down to one tick above the boundary: never liquidated on the way.
    let start = w.mark(m);
    for i in 1..=4u64 {
        let p = start - (start - (lp_floor + 1)) * i / 4;
        let logs = w.tick(m, p);
        assert!(evs::<CrossLiquidationStarted>(&logs).is_empty(), "liquidated at {p} > LP1");
        assert!(!w.acct(ALICE).below(), "the model agrees at {p}");
    }
    assert_eq!(w.mark(m), lp_floor + 1);
    // One tick further: the episode starts on this very tick, triggered by `m`, and reports the
    // detection values — the model's `E` and `Σ MM` at the new mark, every other leg held.
    let a = w.acct(ALICE);
    let s = w.spec(m).clone();
    let l = *a.leg(m);
    let n_new = value(lp_floor, l.amount as i128, s.bd, s.pd);
    let e_new = a.e - l.n + n_new;
    let cmm_new = a.cmm - l.mm + slice_mm(&s.tiers, n_new);
    assert!(e_new < cmm_new, "the model calls the account liquidatable at ⌊LP1⌋");
    let logs = w.tick(m, lp_floor);
    let started = evs::<CrossLiquidationStarted>(&logs);
    assert_eq!(started.len(), 1, "the episode starts one tick below LP1");
    assert_eq!((started[0].user, started[0].triggerMarketId), (ALICE, m));
    assert_eq!(
        (started[0].crossMarginBalance as i128, started[0].totalCrossMaintMargin as i128),
        (e_new, cmm_new),
        "the detection values"
    );
    let liq = evs::<Liquidation>(&logs);
    assert_eq!(liq[0].marketId, m, "the trigger market closes first");
    assert_eq!(liq[0].marginType, CROSSED);
}

#[test]
fn lp_binance_eth_example_episode_starts_one_tick_below_lp1() {
    binance_lp_boundary(ETH, 115_326);
}

#[test]
fn lp_binance_btc_example_episode_starts_one_tick_below_lp1() {
    binance_lp_boundary(BTC, 2_631_689);
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// The standard scripted world
// ═════════════════════════════════════════════════════════════════════════════════════════════

const M1: u64 = 1;
const M2: u64 = 2;
const M3: u64 = 3;
const M4: u64 = 4;
/// An explicit-rate table whose second bracket's rate (15 %) is above the first bracket's `1/L`:
/// a leverage-10 position the mark carries past $20k has `MM > PIM` (the J1 shape, §3.0).
const M5: u64 = 5;
const DRIFT: [Band; 2] =
    [Band { lb: 0, lev: 10, ppm: 50_000 }, Band { lb: 20_000, lev: 5, ppm: 150_000 }];

/// Five whole-dollar markets at a mark of $1,000: M1–M4 at 5 % maintenance (leverage up to 10),
/// M5 on [`DRIFT`]; the band off, no funding, no clearance fee. ALICE, BOB, CAROL and ERIN hold
/// $1e9 at the default margin type (CROSSED) and leverage 10 everywhere; DAVE is the deep ISOLATED
/// liquidity.
fn std_world() -> World {
    let mut specs: Vec<Spec> =
        [M1, M2, M3, M4].iter().map(|&m| Spec::whole(m, 1_000, &T10)).collect();
    specs.push(Spec::whole(M5, 1_000, &DRIFT));
    let mut w = World::new(specs);
    for u in [ALICE, BOB, CAROL, ERIN] {
        w.enroll(u, BIG);
        for m in [M1, M2, M3, M4, M5] {
            w.set_lev(u, m, 10);
        }
    }
    w
}

fn d(usd: i128) -> i128 {
    usd * USD
}

/// `u`'s `AccountBalanceChanged` rows in `logs`: `(reason, totalWalletBalance)`.
fn rows(logs: &[Log], u: Address) -> Vec<(u8, i128)> {
    evs::<AccountBalanceChanged>(logs)
        .into_iter()
        .filter(|r| r.user == u)
        .map(|r| (r.reason, r.totalWalletBalance as i128))
        .collect()
}

fn liq_markets(logs: &[Log], u: Address) -> Vec<u64> {
    evs::<Liquidation>(logs).iter().filter(|l| l.user == u).map(|l| l.marketId).collect()
}

/// The liquidation close orders the episode placed for `u`: `(market, price, qty, type, tif)`.
fn closes(logs: &[Log], u: Address) -> Vec<(u64, u64, u64, u8, u8)> {
    evs::<OrderPlaced>(logs)
        .iter()
        .filter(|o| o.user == u)
        .map(|o| (o.marketId, o.price, o.quantity, o.orderType, o.tif))
        .collect()
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Detection (§3.4): account-level, on the tick of any market the account holds
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// A cross leg deep under water on its own is NOT a liquidation while the ACCOUNT is at cross
/// maintenance (`E ≥ ΣMM`): the M1 long is $1,000 down against an MM of $450 — liquidatable as a
/// position on no silo — but the M2 short's $1,200 gain carries the account. `liquidate()` is
/// refused write-clean. When M2's mark comes back, the ACCOUNT is under (`E = 0 < $950`) and it is
/// caught on M2's tick — the market that moved — which closes first.
#[test]
fn det_the_condition_is_the_accounts_not_a_positions() {
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.fill(ALICE, M2, -10, 1_000);
    w.set_wallet(ALICE, d(1_000));
    w.tick(M2, 880);
    let logs = w.tick(M1, 900);
    let a = w.acct(ALICE);
    assert_eq!((a.e, a.cmm), (d(1_200), d(890)));
    assert!(a.leg(M1).upnl < -a.leg(M1).mm, "the M1 leg alone is far below its own MM");
    assert!(evs::<CrossLiquidationStarted>(&logs).is_empty());
    assert!(evs::<Liquidation>(&logs).is_empty());
    w.rejected(ERIN, &liquidate(ALICE, M1), E_LIQ_HEALTHY);
    w.rejected(ERIN, &liquidate(ALICE, M2), E_LIQ_HEALTHY);
    w.check_view(ALICE);

    let logs = w.tick(M2, 1_000);
    let started = evs::<CrossLiquidationStarted>(&logs);
    assert_eq!(started.len(), 1);
    assert_eq!((started[0].user, started[0].triggerMarketId), (ALICE, M2));
    assert_eq!(
        (started[0].crossMarginBalance as i128, started[0].totalCrossMaintMargin as i128),
        (0, d(950))
    );
    assert_eq!(liq_markets(&logs, ALICE)[0], M2, "the market whose tick caught it closes first");
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// The episode
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// **Q2 / R3 — cancel ALL**: the episode cancels every open order of the account in every market of
/// its index — a book order and an untriggered closePosition stop on the trigger market, a trailing
/// stop ALONE on M2 (a market with no book order), book orders on its ISOLATED market and on a
/// market where it holds nothing — each with reason 3, all before the first close, market by market
/// in ascending order. The ISOLATED position is not touched (the episode closes CROSS positions
/// only). Here the trigger market's close restores the account, so M2 stays open (Q6).
#[test]
fn ep_cancel_all_reaches_every_market_and_every_order_kind() {
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.fill(ALICE, M2, 5, 1_000);
    w.set_type(ALICE, M3, ISOLATED);
    w.set_lev(ALICE, M3, 1);
    w.fill(ALICE, M3, 2, 1_000);
    w.rest(ALICE, M1, BUY, 900, 1);
    w.rest(ALICE, M3, BUY, 900, 1);
    w.rest(ALICE, M4, BUY, 900, 1);
    w.ok(ALICE, &stop_market(M1, SELL, 800, 0, CLOSE_POSITION));
    w.ok(ALICE, &trailing(M2, SELL, 5, 1_000));
    w.set_wallet(ALICE, d(1_500));
    let iso = w.pos(ALICE, M3);
    let iso = (iso.amount, iso.v_quote_balance, iso.margin);

    let logs = w.tick(M1, 880);
    let a = w.acct(ALICE);
    assert!(!a.flagged && !a.below(), "the trigger market's close restored the account");
    // Every order is gone, each cancelled for the liquidation.
    for m in [M1, M2, M3, M4] {
        assert_eq!((w.open_orders(ALICE, m), w.open_algos(ALICE, m)), (0, 0), "market {m}");
    }
    let book: Vec<(u64, u8)> = evs::<OrderCancelled>(&logs)
        .iter()
        .filter(|c| c.user == ALICE)
        .map(|c| (c.marketId, c.reason))
        .collect();
    assert_eq!(book, [M1, M3, M4].map(|m| (m, CANCEL_LIQUIDATION)).to_vec());
    let algo: Vec<(u64, u8)> = evs::<AlgoOrderCancelled>(&logs)
        .iter()
        .filter(|c| c.user == ALICE)
        .map(|c| (c.marketId, c.reason))
        .collect();
    assert_eq!(algo, vec![(M1, CANCEL_LIQUIDATION), (M2, CANCEL_LIQUIDATION)]);
    // …all of them before the first close.
    let first_close = logs.iter().position(is::<OrderPlaced>).unwrap();
    let last_cancel = logs
        .iter()
        .rposition(|l| is::<OrderCancelled>(l) || is::<AlgoOrderCancelled>(l))
        .unwrap();
    assert!(last_cancel < first_close, "cancel-all runs before the closes");
    assert_eq!(liq_markets(&logs, ALICE), vec![M1]);
    let after = w.pos(ALICE, M3);
    assert_eq!((after.amount, after.v_quote_balance, after.margin), iso, "ISOLATED: untouched");
    assert_eq!(w.pos(ALICE, M2).amount, 5, "Q6: the account was restored before M2");
}

/// **D2 — the close order**: the trigger market first, then the other cross markets by `MM_k`
/// DESCENDING, ties by market id ASCENDING. A bankrupt account (`E < 0`) is closed everywhere; each
/// market's remainder goes to ADL at its MM-share bankruptcy price (R5), recomputed on the state
/// the previous closes left — so the deficit is split across the markets by maintenance margin
/// (Q5, D4), and the wallet ends at zero, never below (rounding in the account's favour).
#[test]
fn ep_close_order_is_trigger_first_then_mm_descending_and_the_deficit_splits_by_mm() {
    let mut w = std_world();
    w.fill(ALICE, M1, 4, 1_000);
    w.fill(ALICE, M2, 10, 1_000);
    w.fill(ALICE, M3, 16, 1_000);
    w.fill(ALICE, M4, 10, 1_000);
    w.set_wallet(ALICE, d(1_500));

    // The model's episode, simulated from the detection state: E = 1,500 − 2,000 = −500 at a
    // trigger mark of 500; MM 100 / 500 / 800 / 500.
    let fees_free = w.spec(M1).fee_bps == 0;
    assert!(fees_free);
    let mut sim = w.acct(ALICE);
    {
        let l = sim.legs.iter_mut().find(|l| l.market == M1).unwrap();
        let n = value(500, l.amount as i128, 0, 0);
        sim.e += n - l.n;
        sim.cmm += slice_mm(&T10, n) - l.mm;
        l.mm = slice_mm(&T10, n);
        l.upnl = n + l.vq as i128;
        l.n = n;
        l.mark = 500;
    }
    assert_eq!((sim.e, sim.cmm), (d(-500), d(1_900)));
    let order = [M1, M3, M2, M4];
    let mut expected = Vec::new();
    for m in order {
        let l = *sim.leg(m);
        let target = l.n - sim.share(m);
        // A long's bankruptcy price: the least whole price realizing at least the target.
        let amt = l.amount as i128;
        let p = ((target + amt * USD - 1) / (amt * USD)) as u64;
        expected.push((m, p));
        let realized = value(p, amt, 0, 0) + l.vq as i128;
        sim.w += realized;
        sim.e += realized - l.upnl;
        sim.cmm -= l.mm;
        sim.legs.retain(|x| x.market != m);
        sim.has_cross = !sim.legs.iter().all(|x| x.amount == 0 || !x.cross);
    }

    let logs = w.tick(M1, 500);
    assert_eq!(liq_markets(&logs, ALICE), order.to_vec(), "trigger, then MM desc, ties by id");
    let adl: Vec<(u64, u64)> = evs::<Adl>(&logs)
        .iter()
        .filter(|a| a.liquidatedUser == ALICE)
        .map(|a| (a.marketId, a.price))
        .collect();
    assert_eq!(adl, expected, "each remainder at its MM-share bankruptcy price");
    let winners = evs::<Adl>(&logs).into_iter().filter(|a| a.liquidatedUser == ALICE);
    assert!(winners.map(|a| a.adlUser).all(|u| u == DAVE));
    let a = w.acct(ALICE);
    assert!(a.flat() && !a.flagged);
    // The deficit went to the counterparties; what is left is the rounding of each price in the
    // account's favour (at most a dollar per contract on this grid), exactly as modelled.
    assert_eq!(a.w, sim.w);
    assert!((0..d(40)).contains(&a.w), "W = {}", a.w);
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
}

/// **Q4 / Q5 — the IOC at the MM-share bankruptcy price**, a PARTIAL stop (Q6) and the remainder at
/// the MARK (R5, `E ≥ 0`). ALICE is long 10 M1 and 10 M2; at an M1 mark of 900, `E = $600 < ΣMM =
/// $950`. M1's share of `E` is `600 · 450 / 950`, so the limit is ⌈(9,000 − 284.21) / 10⌉ = 872 —
/// not the whole-account 840 — and the 860 bid is out of reach. 4 fill at 880; still under; the
/// other 6 go to ADL at the mark; back at maintenance (`E = $520 ≥ $500`): STOP — M2 is the
/// user's again.
#[test]
fn ep_the_ioc_limit_is_the_mm_share_bankruptcy_price_then_adl_at_mark_and_a_partial_stop() {
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.fill(ALICE, M2, 10, 1_000);
    w.set_wallet(ALICE, d(1_600));
    w.rest(DAVE, M1, BUY, 880, 4);
    w.rest(DAVE, M1, BUY, 860, 10);
    // The model at the new mark, before the tick (only M1's leg moves).
    let a = w.acct(ALICE);
    let n = value(900, 10, 0, 0);
    let (e, cmm) = (a.e + n - a.leg(M1).n, a.cmm + slice_mm(&T10, n) - a.leg(M1).mm);
    assert_eq!((e, cmm), (d(600), d(950)));
    let share = e * slice_mm(&T10, n) / cmm;
    let bp = ((n - share + 10 * USD - 1) / (10 * USD)) as u64;
    assert_eq!(bp, 872);

    let logs = w.tick(M1, 900);
    assert_eq!(closes(&logs, ALICE), vec![(M1, 872, 10, LIMIT, IOC)], "a LIMIT IOC at BP");
    let t: Vec<(u64, u64, Address)> =
        evs::<Trade>(&logs).iter().map(|t| (t.price, t.quantity, t.maker)).collect();
    assert_eq!(t, vec![(880, 4, DAVE)], "nothing below the limit");
    let adl: Vec<(Address, u64, u64)> =
        evs::<Adl>(&logs).iter().map(|a| (a.adlUser, a.qty, a.price)).collect();
    assert_eq!(adl, vec![(DAVE, 6, 900)], "the remainder at the mark: E ≥ 0");
    let a = w.acct(ALICE);
    assert_eq!((a.e, a.cmm), (d(520), d(500)));
    assert!(!a.flagged && a.leg(M2).amount == 10, "Q6: a partial liquidation");
    let ended = evs::<CrossLiquidationEnded>(&logs);
    assert_eq!(ended.len(), 1);
    assert_eq!(
        (ended[0].crossMarginBalance as i128, ended[0].totalCrossMaintMargin as i128),
        (d(520), d(500))
    );
    let liq = evs::<Liquidation>(&logs);
    assert_eq!((liq.len(), liq[0].amount, liq[0].reward, liq[0].markPrice), (1, 10, 0, 900));
    assert_eq!(w.open_orders(DAVE, M1), 0, "the ADL'd winner lost its orders in this market");
}

/// **Q4 — the band clamp, both ends.** Lower: with a 2 % band at a mark of 900 (`[882, 918]`), the
/// MM-share bankruptcy price 872 of the scenario above is clamped UP to 882. Upper: a bankrupt long
/// (`E = −$300`) has its bankruptcy price ABOVE the mark (930), clamped DOWN to 918 — where the
/// book takes the whole position. The close realizes past the account's equity, so the wallet ends
/// −$120 and, the account being flat, the insurance fund clears it (Q8); a bankrupt close pays no
/// clearance fee.
#[test]
fn ep_the_ioc_limit_is_clamped_into_the_band() {
    // Lower bound.
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.fill(ALICE, M2, 10, 1_000);
    w.set_wallet(ALICE, d(1_600));
    w.set_band(M1, 200);
    w.rest(DAVE, M1, BUY, 885, 3);
    w.rest(DAVE, M1, BUY, 880, 4);
    let logs = w.tick(M1, 900);
    assert_eq!(w.band(M1), (918, 882));
    assert_eq!(closes(&logs, ALICE), vec![(M1, 882, 10, LIMIT, IOC)]);
    let t: Vec<(u64, u64)> = evs::<Trade>(&logs).iter().map(|t| (t.price, t.quantity)).collect();
    assert_eq!(t, vec![(885, 3)]);

    // Upper bound.
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.set_wallet(ALICE, d(700));
    w.set_band(M1, 200);
    w.set_fee_rate(M1, 125);
    w.rest(DAVE, M1, BUY, 918, 10);
    let if0 = w.fund();
    let logs = w.tick(M1, 900);
    let a = w.acct(ALICE);
    assert!(a.flat());
    assert_eq!(closes(&logs, ALICE), vec![(M1, 918, 10, LIMIT, IOC)], "930 clamped to 918");
    let t: Vec<(u64, u64)> = evs::<Trade>(&logs).iter().map(|t| (t.price, t.quantity)).collect();
    assert_eq!(t, vec![(918, 10)]);
    assert!(evs::<Adl>(&logs).is_empty());
    assert_eq!(evs::<Liquidation>(&logs)[0].reward, 0, "bankrupt: no clearance fee");
    assert_eq!(a.w, 0);
    assert_eq!(w.fund() - if0, -d(120), "the fund cleared the −$120 (Q8)");
    let r = rows(&logs, ALICE);
    assert_eq!(&r[r.len() - 2..], &[(r[r.len() - 2].0, -d(120)), (INSURANCE_CLEAR, 0)]);
}

/// The bankruptcy price of the upper-bound scenario, from the model (930 > 918).
#[test]
fn ep_model_bankruptcy_price_of_the_clamped_scenario() {
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.set_wallet(ALICE, d(700));
    // At a mark of 900 E = −300: BP = ⌈(9,000 + 300) / 10⌉.
    storage::save_mark_price(&mut w.ctx, M1, 900).unwrap();
    assert_eq!(w.bp(ALICE, M1), 930);
}

/// ALICE long 2 on M1 and 10 on M2 at $1,000, wallet $800: an M1 mark of 500 makes her bankrupt
/// (`E = −$200`) with M2 still to close. M2 was last published at `t2`.
fn stale_world() -> (World, u64) {
    stale_world_with(false)
}

/// [`stale_world`], with an ISOLATED long of 1 on M3 beside it when `iso` (its silo is its own).
fn stale_world_with(iso: bool) -> (World, u64) {
    let mut w = std_world();
    if iso {
        w.set_type(ALICE, M3, ISOLATED);
        w.fill(ALICE, M3, 1, 1_000);
    }
    w.fill(ALICE, M1, 2, 1_000);
    w.fill(ALICE, M2, 10, 1_000);
    w.set_wallet(ALICE, d(800));
    let t2 = storage::load_index_price_state(&mut w.ctx, M2).unwrap().timestamp;
    (w, t2)
}

/// **Q10 — the stale guard, to the second.** An episode skips closing market k when its mark is
/// OLDER than `STALE_MULT × interval` (3 × 15 s): at exactly 45 s M2 is closed, at 46 s it is
/// skipped — the account stays flagged (still under maintenance) with no `CrossLiquidationEnded`,
/// and M2's own next tick resumes the SAME episode (no second `Started`) and ends it. The market
/// whose oracle tick runs the sweep is exempt whatever its stamp (its mark is this transaction's):
/// a tick stamped long before the block time still closes.
#[test]
fn ep_the_stale_guard_skips_a_market_older_than_three_intervals_to_the_second() {
    // Exactly 3 intervals old: not stale.
    let (mut w, t2) = stale_world();
    w.set_now(t2 + STALE_MULT * 15);
    let logs = w.tick(M1, 500);
    assert_eq!(liq_markets(&logs, ALICE), vec![M1, M2]);
    let a = w.acct(ALICE);
    assert!(a.flat() && !a.flagged);
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);

    // One second older: skipped, the flag stays.
    let (mut w, t2) = stale_world();
    w.set_now(t2 + STALE_MULT * 15 + 1);
    let logs = w.tick(M1, 500);
    assert_eq!(liq_markets(&logs, ALICE), vec![M1]);
    let a = w.acct(ALICE);
    assert!(a.flagged && a.below() && a.leg(M2).amount == 10);
    assert!(evs::<CrossLiquidationEnded>(&logs).is_empty());
    assert!(!w.deferred(M2), "a stale mark is not same-block work: nothing recorded");
    // M2's own tick resumes it.
    let logs = w.tick(M2, 1_000);
    assert!(evs::<CrossLiquidationStarted>(&logs).is_empty(), "the same episode, resumed");
    assert_eq!(liq_markets(&logs, ALICE), vec![M2]);
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    let a = w.acct(ALICE);
    assert!(a.flat() && !a.flagged && a.w >= 0);

    // The ticking market is exempt: its oracle stamps a time far behind the block.
    let (mut w, t2) = stale_world();
    w.set_now(t2 + 1_000);
    let logs = w.tick_at(M1, 500, t2 + 15);
    assert_eq!(liq_markets(&logs, ALICE), vec![M1], "M1 closes, its stamp old; M2 is stale");
    assert!(w.flagged(ALICE));
}

/// **Q6 inside one market, the fee before the stop check.** Long 10 at a mark of 920 with `E = $400
/// < $460`: the IOC (limit 880) takes the 3 bid at 915; the clearance fee on those 3 (1 % of the
/// notional at the mark, $27.60, under the cap `E_after = $385`) is charged, and the account is
/// back at maintenance (`$357.40 ≥ $322`) — the liquidation STOPS with 7 still open: no ADL.
#[test]
fn ep_a_partial_fill_that_restores_the_account_stops_the_episode_after_its_fee() {
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.set_wallet(ALICE, d(1_200));
    w.set_fee_rate(M1, 100);
    w.rest(DAVE, M1, BUY, 915, 3);
    let if0 = w.fund();
    let logs = w.tick(M1, 920);
    assert_eq!(closes(&logs, ALICE), vec![(M1, 880, 10, LIMIT, IOC)]);
    assert!(evs::<Adl>(&logs).is_empty(), "restored by the IOC: nothing goes to ADL");
    let fee = w.fee_due(M1, 3);
    assert_eq!(fee, 27_600_000);
    let liq = evs::<Liquidation>(&logs);
    assert_eq!((liq[0].amount, liq[0].reward as i128), (10, fee));
    assert_eq!(w.fund() - if0, fee, "the fee went to the insurance fund");
    let a = w.acct(ALICE);
    assert_eq!(a.leg(M1).amount, 7);
    assert_eq!((a.e, a.cmm), (d(385) - fee, d(322)));
    assert!(!a.flagged);
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
}

/// **Q3 / J5 — the clearance fee is the closed notional × the rate, capped.**
/// * cross: at `max(0, E_after)` — 5 % of $9,200 is $460, but only $100 of equity is left;
/// * cross, both legs: the IOC's 4 and then the ADL's 6 are each charged, each at the equity then
///   left; the account still under, M2 closes too (at the mark, `E ≥ 0`) and pays its own;
/// * cross, at the EQUITY, not the wallet: with 6 of M1 still open at a loss the wallet holds
///   $1,120 but `E` only $520 — a 20 % fee ($720 due) takes $520, and the account goes on at
///   `E = 0`;
/// * isolated: at the CLOSE CREDIT — the silo returned $50 to the wallet, so $50 of the $452.50,
///   not a cent of the $5,000 cross wallet (J5).
#[test]
fn ep_the_clearance_fee_is_capped_at_the_equity_left_cross_and_the_close_credit_isolated() {
    // Cross, capped at E_after.
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.set_wallet(ALICE, d(1_100));
    w.set_fee_rate(M1, 500);
    w.rest(DAVE, M1, BUY, 900, 10);
    let if0 = w.fund();
    let logs = w.tick(M1, 920);
    assert_eq!(w.fee_due(M1, 10), d(460));
    assert_eq!(evs::<Liquidation>(&logs)[0].reward as i128, d(100));
    assert_eq!(w.fund() - if0, d(100));
    let a = w.acct(ALICE);
    assert!(a.flat() && a.w == 0);
    assert!(evs::<InsuranceFundDepleted>(&logs).is_empty());
    assert!(
        evs::<InsuranceFundChanged>(&logs).iter().all(|c| c.delta > 0),
        "the fund only received: nothing to clear"
    );

    // Cross, the IOC leg and the ADL leg each charged.
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.fill(ALICE, M2, 10, 1_000);
    w.set_wallet(ALICE, d(1_600));
    w.set_fee_rate(M1, 100);
    w.set_fee_rate(M2, 100);
    w.rest(DAVE, M1, BUY, 880, 4);
    let if0 = w.fund();
    let logs = w.tick(M1, 900);
    let liq: Vec<(u64, i128)> =
        evs::<Liquidation>(&logs).iter().map(|l| (l.marketId, l.reward as i128)).collect();
    // M1: $36 on the 4 the book took (E then $520), $54 on the 6 ADL took at 900 (E then $484);
    // E = $430 < $500: M2 closes at the mark by ADL and pays 1 % of $10,000.
    assert_eq!(liq, vec![(M1, d(36) + d(54)), (M2, d(100))]);
    assert_eq!(w.fund() - if0, d(190));
    let a = w.acct(ALICE);
    assert!(a.flat() && !a.flagged);
    assert_eq!(a.w, d(330));

    // Cross, capped at the equity while a losing position is still open.
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.fill(ALICE, M2, 10, 1_000);
    w.set_wallet(ALICE, d(1_600));
    w.set_fee_rate(M1, 2_000);
    w.rest(DAVE, M1, BUY, 880, 4);
    let if0 = w.fund();
    let logs = w.tick(M1, 900);
    assert_eq!(w.fee_due(M1, 4), d(720));
    let liq: Vec<(u64, i128)> =
        evs::<Liquidation>(&logs).iter().map(|l| (l.marketId, l.reward as i128)).collect();
    assert_eq!(liq, vec![(M1, d(520)), (M2, 0)], "E_after = $520 while W = $1,120");
    let adl: Vec<(u64, u64)> = evs::<Adl>(&logs).iter().map(|a| (a.marketId, a.price)).collect();
    assert_eq!(adl, vec![(M1, 900), (M2, 1_000)], "E = 0 after the fee: still at the mark");
    assert_eq!(w.fund() - if0, d(520));
    let a = w.acct(ALICE);
    assert!(a.flat() && !a.flagged && a.w == 0);

    // Isolated, capped at the close credit.
    let mut w = std_world();
    w.set_type(CAROL, M1, ISOLATED);
    w.fill(CAROL, M1, 10, 1_000);
    w.set_wallet(CAROL, d(5_000));
    w.set_fee_rate(M1, 500);
    w.rest(DAVE, M1, BUY, 905, 10);
    let if0 = w.fund();
    let logs = w.tick(M1, 905);
    let liq = evs::<Liquidation>(&logs);
    assert_eq!((liq.len(), liq[0].user, liq[0].marginType), (1, CAROL, ISOLATED));
    assert_eq!(w.fee_due(M1, 10), 452_500_000);
    assert_eq!(liq[0].reward as i128, d(50), "the close credit, not the wallet");
    assert_eq!(w.fund() - if0, d(50));
    assert_eq!(w.wallet(CAROL), d(5_000));
}

// ── Budget and continuation ──────────────────────────────────────────────────────────────────

/// `n` accounts, each CROSS long 1 on `m` at $1,000 with $60 left in the wallet: a mark of 900
/// makes each bankrupt (`E = −$40 < $45`), one step to liquidate (no orders, one market).
fn victims(w: &mut World, m: u64, n: u32) -> Vec<Address> {
    let mut v = Vec::new();
    for i in 0..n {
        let u = user(i);
        w.enroll(u, d(1_000));
        w.set_lev(u, m, 10);
        w.fill(u, m, 1, 1_000);
        w.set_wallet(u, d(60));
        v.push(u);
    }
    v
}

/// **The sweep's step budget (50), the continuation, the freeze while it waits (R1), and the drain
/// that resumes it.** 49 one-step victims take 49 of the 50 steps of M1's tick. ALICE comes last in
/// M1's registry, liquidatable, with orders on M2 and M3: her episode flags her, spends the last
/// step cancelling M2, and stops with M3's orders still resting and every position open — the
/// continuation recorded on M1 (the market of her next close), which the sweep reports as
/// `deferred_work`. In between ALICE may only reduce: her plain M3 bid, when the book reaches it,
/// is cancelled for the liquidation instead of filled (the freeze), while her reduce-only M3 offer
/// fills (R1). `drainDeferredWork(M1)` resumes the SAME episode and finishes it.
#[test]
fn ep_out_of_budget_the_episode_records_a_continuation_the_drain_resumes() {
    let mut w = std_world();
    let v = victims(&mut w, M1, (SWEEP_STEPS - 1) as u32);
    w.fill(ALICE, M3, 5, 1_000);
    w.fill(ALICE, M1, 1, 1_000);
    w.rest(ALICE, M2, BUY, 900, 1);
    w.rest(ALICE, M3, BUY, 950, 1);
    w.ok(ALICE, &gtc_ro(M3, SELL, 1_010, 1));
    w.set_wallet(ALICE, d(300));

    let logs = w.tick(M1, 900);
    for u in &v {
        let a = w.acct(*u);
        assert!(a.flat() && a.w >= 0 && !a.flagged, "victim {u}");
    }
    let started = evs::<CrossLiquidationStarted>(&logs);
    assert_eq!(started.iter().filter(|s| s.user == ALICE).count(), 1);
    assert!(liq_markets(&logs, ALICE).is_empty(), "no step left to close");
    let cancelled: Vec<u64> = evs::<OrderCancelled>(&logs)
        .iter()
        .filter(|c| c.user == ALICE)
        .map(|c| c.marketId)
        .collect();
    assert_eq!(cancelled, vec![M2], "the last step cancelled M2");
    assert_eq!(w.open_orders(ALICE, M3), 2, "M3's orders wait, frozen");
    assert!(w.flagged(ALICE));
    assert!(w.deferred(M1), "the continuation is recorded on M1");
    assert_eq!((w.pos(ALICE, M1).amount, w.pos(ALICE, M3).amount), (1, 5));

    // The freeze: the plain bid is cancelled when hit, not filled …
    let logs = w.ok(DAVE, &mkt(M3, SELL, 1));
    let c = evs::<OrderCancelled>(&logs);
    assert_eq!((c.len(), c[0].user, c[0].reason), (1, ALICE, CANCEL_LIQUIDATION));
    assert!(evs::<Trade>(&logs).is_empty());
    // … the reduce-only offer fills (R1).
    let logs = w.ok(DAVE, &mkt(M3, BUY, 1));
    let t = evs::<Trade>(&logs);
    assert_eq!((t.len(), t[0].maker, t[0].price), (1, ALICE, 1_010));
    assert_eq!(w.pos(ALICE, M3).amount, 4);

    // The drain resumes the same episode: M1 closes (at the mark, E ≥ 0) and the account is back.
    let logs = w.ok(ERIN, &drain(M1));
    assert!(!drainDeferredWorkCall::abi_decode_returns(&w.last_out).unwrap());
    assert!(evs::<CrossLiquidationStarted>(&logs).is_empty());
    assert_eq!(liq_markets(&logs, ALICE), vec![M1]);
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1);
    let a = w.acct(ALICE);
    assert!(!a.flagged && !a.below() && a.leg(M3).amount == 4);
    assert!(!w.deferred(M1));
}

/// **The continuation on ANOTHER market.** 48 victims; ALICE's cancel (M2) and her M1 close spend
/// the last two steps, she is still under (bankrupt), and her M3 close is left: the continuation is
/// written on M3's `deferred_work` directly, and M3's drain finishes the episode.
#[test]
fn ep_a_continuation_on_another_market_is_resumed_by_that_markets_drain() {
    let mut w = std_world();
    victims(&mut w, M1, (SWEEP_STEPS - 2) as u32);
    w.fill(ALICE, M3, 5, 1_000);
    w.fill(ALICE, M1, 1, 1_000);
    w.rest(ALICE, M2, BUY, 900, 1);
    w.set_wallet(ALICE, d(50));
    let logs = w.tick(M1, 900);
    assert_eq!(liq_markets(&logs, ALICE), vec![M1]);
    assert!(w.flagged(ALICE) && w.acct(ALICE).below());
    assert!(w.deferred(M3) && !w.deferred(M1), "recorded on M3, the next close");
    let logs = w.ok(ERIN, &drain(M3));
    assert_eq!(liq_markets(&logs, ALICE), vec![M3]);
    assert!(evs::<CrossLiquidationStarted>(&logs).is_empty());
    let a = w.acct(ALICE);
    assert!(a.flat() && !a.flagged && a.w >= 0);
    assert!(!w.deferred(M3));
}

/// `n` whole-dollar markets at $1,000 and 5 % maintenance; ALICE, BOB, ERIN at leverage 10.
fn world_n(n: u64) -> World {
    let mut w = World::new((1..=n).map(|m| Spec::whole(m, 1_000, &T10)).collect());
    for u in [ALICE, BOB, ERIN] {
        w.enroll(u, BIG);
        for m in 1..=n {
            w.set_lev(u, m, 10);
        }
    }
    w
}

/// **`liquidate()` spends a fresh budget of 16 steps** (§3.4: "a new step budget of 16"), one per
/// market cancelled and one per market closed. ALICE, bankrupt (`W = −$500`), holds cross longs on
/// M1 and M2 and resting orders in `k` markets. With orders in 14 markets the episode fits (14 + 2)
/// and ends; with 15 it cancels all 15 and closes M1, and M2's close is recorded on M2's
/// `deferred_work`, which M2's drain then resumes.
///
/// Both markets run a 2 % band with a bid at its top (1,020): each bankruptcy price is far above
/// it, so each close fills there, and the account ends flat with the deficit left in its wallet —
/// the insurance fund clears it (Q8) in the call that closed the last position: `liquidate()`
/// itself, or the drain.
#[test]
fn ep_manual_liquidate_spends_a_fresh_budget_of_sixteen_steps() {
    for k in [MANUAL_STEPS as u64 - 2, MANUAL_STEPS as u64 - 1] {
        let mut w = world_n(16);
        w.fill(ALICE, M1, 1, 1_000);
        w.fill(ALICE, M2, 1, 1_000);
        for m in 1..=k {
            w.rest(ALICE, m, BUY, 900, 1);
        }
        for m in [M1, M2] {
            w.set_band(m, 200);
            w.rest(DAVE, m, BUY, 1_020, 1);
        }
        w.set_wallet(ALICE, -d(500));
        assert!(w.acct(ALICE).below(), "under maintenance with no tick: only a keeper finds it");
        let f = w.fund();
        let logs = w.ok(ERIN, &liquidate(ALICE, M1));
        let cancelled = evs::<OrderCancelled>(&logs).iter().filter(|c| c.user == ALICE).count();
        assert_eq!(cancelled as u64, k);
        let liq = evs::<Liquidation>(&logs);
        assert!(liq.iter().all(|l| l.liquidator == ERIN), "the keeper is recorded");
        assert_eq!(closes(&logs, ALICE)[0], (M1, 1_020, 1, LIMIT, IOC), "BP clamped to the band");
        if k == MANUAL_STEPS as u64 - 2 {
            assert_eq!(liq_markets(&logs, ALICE), vec![M1, M2]);
            let a = w.acct(ALICE);
            assert!(a.flat() && !a.flagged);
            assert_cleared(&mut w, &logs, ALICE, d(460), f);
        } else {
            assert_eq!(liq_markets(&logs, ALICE), vec![M1], "out of steps before M2");
            assert!(w.flagged(ALICE) && w.deferred(M2) && !w.deferred(M1));
            assert_eq!(w.wallet(ALICE), -d(480), "a receivable while M2 is open");
            let logs = w.ok(ERIN, &drain(M2));
            assert_eq!(liq_markets(&logs, ALICE), vec![M2]);
            let a = w.acct(ALICE);
            assert!(a.flat() && !a.flagged);
            assert_cleared(&mut w, &logs, ALICE, d(460), f);
        }
    }
}

// ── The flag (R6) ────────────────────────────────────────────────────────────────────────────

/// ALICE flagged by an episode that could not finish: M2's mark is stale, she is still under. M1
/// closed at its MM-share bankruptcy price, which leaves the wallet NEGATIVE with M2 still open —
/// a receivable, not a clearance (Q8: only once no position is left).
fn flagged_world() -> World {
    flagged_world_with(false)
}

fn flagged_world_with(iso: bool) -> World {
    let (mut w, t2) = stale_world_with(iso);
    w.set_now(t2 + STALE_MULT * 15 + 1);
    let f = w.fund();
    w.tick(M1, 500);
    assert!(w.flagged(ALICE) && w.acct(ALICE).below() && w.pos(ALICE, M2).amount == 10);
    assert!(w.wallet(ALICE) < 0, "M1's share of the deficit is in the wallet");
    assert_eq!(w.fund(), f, "not cleared while M2 is open");
    w
}

/// **The flag ends exactly when it should.** It does not end while the account is under and its
/// remaining market cannot be closed (a keeper's `liquidate()` on the stale market changes
/// nothing), it ends with ONE write and `CrossLiquidationEnded` — no close — once the account is
/// back at maintenance and the episode is visited again (a tick of its market, or `liquidate()`),
/// and it ends at the end of the call in which the account's own reduce-only close takes its last
/// cross position (that fill is the R1 exemption at work) — whether or not an ISOLATED position
/// stays open beside it.
#[test]
fn ep_the_flag_ends_on_recovery_or_with_the_last_cross_position_and_not_before() {
    for route in 0..4 {
        let mut w = flagged_world_with(route == 3);
        let before = w.snapshot();
        let logs = w.ok(ERIN, &liquidate(ALICE, M2));
        assert!(logs.is_empty() && w.snapshot() == before, "a keeper cannot close a stale mark");
        assert!(w.flagged(ALICE));
        match route {
            0 | 1 => {
                // A deposit takes the account back over maintenance; the next visit ends it.
                w.set_spot(ALICE, d(10_000) as u128);
                w.ok(ALICE, &transferToPerpCall { amount: d(1_000) as u64 }.abi_encode());
                assert!(!w.acct(ALICE).below());
                let logs =
                    if route == 0 { w.tick(M2, 1_000) } else { w.ok(ERIN, &liquidate(ALICE, M2)) };
                assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1, "route {route}");
                assert!(evs::<Liquidation>(&logs).is_empty() && evs::<Trade>(&logs).is_empty());
                assert!(!w.flagged(ALICE));
                assert_eq!(w.pos(ALICE, M2).amount, 10, "recovered: nothing closed");
            }
            _ => {
                // Her own reduce-only close of the last cross position, filled as a maker.
                w.ok(ALICE, &gtc_ro(M2, SELL, 990, 10));
                let logs = w.ok(DAVE, &mkt(M2, BUY, 10));
                assert_eq!(evs::<Trade>(&logs)[0].maker, ALICE, "R1: the reduce-only maker fills");
                let ended = evs::<CrossLiquidationEnded>(&logs);
                assert_eq!((ended.len(), ended[0].user), (1, ALICE));
                assert!(!w.flagged(ALICE));
                assert_eq!(w.pos(ALICE, M3).amount, (route == 3) as i64, "the isolated leg stays");
            }
        }
    }
}

// ── While flagged (Q11, R1) ──────────────────────────────────────────────────────────────────

const KEY_ID: u8 = 1;
const RECV: u64 = 60;

fn signed_order(
    w: &World,
    sk: &SigningKey,
    account: Address,
    m: u64,
    side: u8,
    price: u64,
    flags: u8,
) -> Vec<u8> {
    let client = [9u8; 16];
    let mut msg = b"perpdex_v1_order".to_vec();
    msg.extend_from_slice(&w.ctx.chain_id().to_be_bytes());
    msg.extend_from_slice(account.as_slice());
    msg.extend_from_slice(&m.to_be_bytes());
    msg.push(side);
    msg.extend_from_slice(&price.to_be_bytes());
    msg.extend_from_slice(&1u64.to_be_bytes());
    msg.extend_from_slice(&[LIMIT, GTC]);
    msg.extend_from_slice(&client);
    msg.extend_from_slice(&w.now.to_be_bytes());
    msg.extend_from_slice(&RECV.to_be_bytes());
    msg.extend_from_slice(&[KEY_ID, flags]);
    placeOrderSignedCall {
        account,
        marketId: m,
        side,
        price,
        quantity: 1,
        orderType: LIMIT,
        tif: GTC,
        clientOrderId: client.into(),
        timestamp: w.now,
        recvWindow: RECV,
        keyId: KEY_ID,
        flags,
        signature: sk.sign(&msg).to_bytes().to_vec().into(),
    }
    .abi_encode()
}

fn item(m: u64, side: u8, price: u64, flags: u8) -> PlaceItem {
    PlaceItem {
        marketId: m,
        side,
        price,
        quantity: 1,
        orderType: LIMIT,
        tif: GTC,
        clientOrderId: FixedBytes::default(),
        flags,
    }
}

/// **Q11 — the `-2023` matrix of a REALLY flagged account** (flagged by an episode, not a fixture):
/// every entrypoint refuses an order that is not reduce-only — whichever way it would trade, even
/// one that would only reduce — and the mode switch; reduce-only orders, closePosition and
/// reduce-only conditionals, cancels and deposits go through.
#[test]
fn frz_the_minus_2023_matrix_of_a_flagged_account() {
    let mut w = flagged_world();
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let pubkey = sk.verifying_key().to_bytes().into();
    w.ok(ALICE, &registerApiKeyCall { keyId: KEY_ID, pubkey, expiry: 0 }.abi_encode());
    w.rejected(ALICE, &gtc(M3, BUY, 900, 1), E_2023);
    w.rejected(ALICE, &gtc(M2, SELL, 1_100, 1), E_2023);
    w.rejected(ALICE, &mkt(M2, BUY, 1), E_2023);
    w.rejected(ALICE, &mkt(M2, SELL, 1), E_2023);
    // Signed: refused after the signature burn, with the same reason and no log.
    let out = w.run(ERIN, &signed_order(&w, &sk, ALICE, M3, BUY, 900, 0));
    assert!(out.reverted);
    assert_eq!(revert_reason(&out), E_2023);
    assert!(take_logs(&mut w.ctx).is_empty());
    // Reduce-only: accepted, on every entrypoint.
    let ro = w.ok(ALICE, &gtc_ro(M2, SELL, 1_100, 1));
    let ro_id = evs::<OrderPlaced>(&ro)[0].orderId;
    let logs = w.ok(
        ALICE,
        &batchPlaceOrdersCall { orders: vec![item(M3, BUY, 900, 0), item(M2, SELL, 1_101, RO)] }
            .abi_encode(),
    );
    let st = batchPlaceOrdersCall::abi_decode_returns(&w.last_out).unwrap();
    let st: Vec<(u8, u8)> = st.chunks(34).map(|r| (r[0], r[33])).collect();
    assert_eq!(st, vec![(0, BATCH_IN_LIQUIDATION), (1, 0)]);
    assert_eq!(evs::<OrderPlaced>(&logs).len(), 1);
    w.ok(ERIN, &signed_order(&w, &sk, ALICE, M2, SELL, 1_102, RO));
    let stop = w.ok(ALICE, &stop_market(M2, SELL, 900, 0, CLOSE_POSITION));
    let stop_id = evs::<AlgoOrderPlaced>(&stop)[0].algoId;
    w.ok(ALICE, &trailing(M2, SELL, 10, 500));
    // The mode switch is refused; cancels and deposits are not.
    let switch = setMarginTypeCall { marketId: M4, marginType: ISOLATED }.abi_encode();
    w.rejected(ALICE, &switch, E_2023_MT);
    w.ok(ALICE, &cancelOrderCall { orderId: ro_id, marketId: M2 }.abi_encode());
    w.ok(ALICE, &cancelAlgoOrderCall { algoId: stop_id, marketId: M2 }.abi_encode());
    w.set_spot(ALICE, d(10) as u128);
    w.ok(ALICE, &transferToPerpCall { amount: d(10) as u64 }.abi_encode());
    assert!(w.flagged(ALICE));
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// R2 — J1 at rest
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// **R2: an order that can OPEN rests only while `E − OpenLoss ≥ ΣMM`.** ALICE's M5 long of 19
/// (leverage 10, $19,000 at entry: inside the first bracket) is carried by the mark into the 15 %
/// bracket: `MM = $5,125 > PIM = $4,750`. A maker loss on M1 (never margin-checked, Q9) leaves
/// `E = $4,925` — under maintenance, yet `A = $175 ≥ 0`, so the available-balance gate alone would
/// admit an opening bid costing $150 of IM. R2 refuses it, and an isolated opening order of the
/// same account; a reduce-only order rests. At `E = ΣMM` exactly (a $200 deposit) the bid rests; a
/// bid ABOVE the mark carries its Open Loss and is refused again.
#[test]
fn rest_an_opening_order_rests_only_at_cross_maintenance() {
    let mut w = std_world();
    // The M5 counterparty is BOB (cross, $1e9), whom a 2.5× move does not liquidate.
    w.rest(BOB, M5, SELL, 1_000, 19);
    w.ok(ALICE, &mkt(M5, BUY, 19));
    w.tick(M5, 2_500);
    w.fill(ALICE, M1, 30, 1_000);
    w.set_wallet(ALICE, d(3_425));
    w.make(ALICE, M1, -30, 100, RO);
    let a = w.acct(ALICE);
    assert_eq!((a.e, a.cmm), (d(4_925), d(5_125)));
    let v = w.account_view(ALICE);
    assert_eq!(v.availableBalance as i128, d(175), "A ≥ 0 under maintenance");

    w.rejected(ALICE, &gtc(M5, BUY, 1_500, 1), E_J1);
    w.ok(ALICE, &gtc_ro(M5, SELL, 2_600, 1));
    w.set_type(ALICE, M2, ISOLATED);
    w.rejected(ALICE, &gtc(M2, BUY, 900, 1), E_J1);

    w.set_spot(ALICE, d(200) as u128);
    w.ok(ALICE, &transferToPerpCall { amount: d(200) as u64 }.abi_encode());
    let a = w.acct(ALICE);
    assert_eq!(a.e, a.cmm, "E = ΣMM: healthy (the `≥` rule)");
    let bid = w.rest(ALICE, M5, BUY, 1_500, 1);
    w.ok(ALICE, &cancelOrderCall { orderId: bid.into(), marketId: M5 }.abi_encode());
    // $10 of Open Loss above the mark.
    w.rejected(ALICE, &gtc(M5, BUY, 2_510, 1), E_J1);
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// ADL (R5, D7)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// **R5 — a cross winner at a LOSS ranks `uPnL · E / N²`** (Binance's `PnL% / EffLev`), not
/// `uPnL / E`. ALICE's long of 5 is liquidated at the mark with an empty book (`E ≥ 0`: ADL at the
/// mark). The only shorts are BOB and CAROL, each short 5 at 980 — $100 down — BOB with `E =
/// $1,000`, CAROL with `E = $10,000`: BOB ranks −0.004, CAROL −0.04, so BOB is taken (on `uPnL / E`
/// CAROL's −0.01 would beat BOB's −0.1). D7: BOB loses his orders in M1 only.
#[test]
fn adl_a_losing_cross_winner_ranks_upnl_times_e_over_n_squared() {
    let mut w = std_world();
    w.fill(ALICE, M1, 5, 1_100);
    w.fill(BOB, M1, -5, 980);
    w.fill(CAROL, M1, -5, 980);
    w.set_wallet(ALICE, d(600));
    w.set_wallet(BOB, d(1_100));
    w.set_wallet(CAROL, d(10_100));
    w.rest(BOB, M1, SELL, 1_200, 1);
    w.rest(BOB, M2, BUY, 800, 1);
    let (b, c) = (w.acct(BOB), w.acct(CAROL));
    assert_eq!((b.e, c.e, b.leg(M1).upnl, c.leg(M1).upnl), (d(1_000), d(10_000), -d(100), -d(100)));
    let logs = w.tick(M1, 1_000);
    let adl: Vec<(Address, Address, u64, u64)> =
        evs::<Adl>(&logs).iter().map(|a| (a.liquidatedUser, a.adlUser, a.qty, a.price)).collect();
    assert_eq!(adl, vec![(ALICE, BOB, 5, 1_000)]);
    assert_eq!(w.pos(CAROL, M1).amount, -5);
    assert_eq!((w.open_orders(BOB, M1), w.open_orders(BOB, M2)), (0, 1), "D7");
}

/// **R5 — the clean tier is judged PER FILL: `E − take · |fill − mark| ≥ 0`.** CAROL's isolated
/// long of 4 (silo $400) is liquidated at 850 with an empty book; insolvent, it goes to ADL at its
/// bankruptcy price 900 (50 off the mark). BOB, cross short, ranks first on `uPnL / E`; both his
/// accounts are above maintenance at 850.
/// * BOB short 4 with `E = $190`: the fill would cost him 4 · 50 = $200 > $190 — not clean, he goes
///   behind the clean tier and ERIN (isolated, clean) takes it.
/// * BOB short 10 with `E = $480`: THIS fill takes 4 (4 · 50 = $200 ≤ $480), although his whole
///   position (10 · 50 = $500) would not fit — clean: BOB takes it.
#[test]
fn adl_the_clean_tier_is_judged_on_the_fill() {
    for (bob_short, bob_e, winner) in [(4i64, 190i128, ERIN), (10, 480, BOB)] {
        let mut w = std_world();
        w.set_type(CAROL, M1, ISOLATED);
        w.fill(CAROL, M1, 4, 1_000);
        w.set_type(ERIN, M1, ISOLATED);
        w.set_lev(ERIN, M1, 2);
        w.fill(ERIN, M1, -4, 1_000);
        w.fill(BOB, M1, -bob_short, 1_000);
        // E at the 850 mark = W + 150 · short.
        w.set_wallet(BOB, d(bob_e) - d(150) * bob_short as i128);
        let logs = w.tick(M1, 850);
        let adl: Vec<(Address, u64, u64)> =
            evs::<Adl>(&logs).iter().map(|a| (a.adlUser, a.qty, a.price)).collect();
        assert_eq!(adl, vec![(winner, 4, 900)], "BOB short {bob_short}, E {bob_e}");
        assert!(evs::<Liquidation>(&logs).iter().all(|l| l.user == CAROL), "BOB stays healthy");
    }
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Q8 / R4 — the negative-wallet clearance
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// Asserts the clearance of `deficit` for `u` in `logs`: the fund paid `deficit` (or what it had),
/// the wallet is 0, and `u`'s last two rows are the deficit and then the `INSURANCE_CLEAR` at 0.
fn assert_cleared(w: &mut World, logs: &[Log], u: Address, deficit: i128, fund_before: i128) {
    let covered = deficit.min(fund_before);
    assert_eq!(w.wallet(u), 0, "the wallet is cleared to zero");
    assert_eq!(w.fund(), fund_before - covered, "the fund paid the deficit (what it had)");
    let paid: Vec<i128> =
        evs::<InsuranceFundChanged>(logs).iter().map(|c| c.delta as i128).collect();
    if covered > 0 {
        assert_eq!(paid.last(), Some(&-covered));
    }
    let short: i128 = evs::<InsuranceFundDepleted>(logs).iter().map(|x| x.badDebt as i128).sum();
    assert_eq!(short, deficit - covered, "the uncovered rest is socialised");
    let r = rows(logs, u);
    assert!(r.len() >= 2, "{r:?}");
    assert_eq!(r[r.len() - 2].1, -deficit, "the deficit is published first");
    assert_eq!(r[r.len() - 1], (INSURANCE_CLEAR, 0), "then the clearance");
}

/// ALICE CROSS long 10 on M1 at $1,000 with $100 in the wallet: closed at 850 she realizes −$1,500.
fn underwater() -> World {
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.set_wallet(ALICE, d(100));
    w
}

/// **Q8 on every route that closes the last position**: a reduce-only taker close, a reduce-only
/// maker close, a fired closePosition stop, a cross episode (the band clamp of
/// `ep_the_ioc_limit_is_clamped_into_the_band`), an isolated liquidation of the last position, and
/// an ADL fill that takes a negative-wallet winner's last position. Each clears exactly the deficit
/// in the same call, the deficit row first.
#[test]
fn clr_the_last_close_clears_a_negative_wallet_on_every_route() {
    // A reduce-only taker close.
    let mut w = underwater();
    w.rest(DAVE, M1, BUY, 850, 10);
    let f = w.fund();
    let logs = w.ok(ALICE, &mkt_ro(M1, SELL, 10));
    assert_cleared(&mut w, &logs, ALICE, d(1_400), f);

    // A reduce-only maker close.
    let mut w = underwater();
    let f = w.fund();
    let logs = w.make(ALICE, M1, -10, 850, RO);
    assert_cleared(&mut w, &logs, ALICE, d(1_400), f);

    // A fired closePosition stop (the triggers run before the liquidation sweep).
    let mut w = underwater();
    w.ok(ALICE, &stop_market(M1, SELL, 950, 0, CLOSE_POSITION));
    w.rest(DAVE, M1, BUY, 850, 10);
    let f = w.fund();
    let logs = w.tick(M1, 950);
    assert!(evs::<Liquidation>(&logs).is_empty(), "the stop closed it first");
    assert_cleared(&mut w, &logs, ALICE, d(1_400), f);

    // An isolated liquidation of the last position, the wallet a receivable until then.
    let mut w = std_world();
    w.set_type(ALICE, M3, ISOLATED);
    w.fill(ALICE, M3, 10, 1_000);
    w.fill(ALICE, M1, 10, 1_000);
    w.set_wallet(ALICE, d(1_000));
    w.make(ALICE, M1, -10, 850, RO);
    assert_eq!(w.wallet(ALICE), -d(500), "NOT cleared: the isolated position is still open");
    w.rest(DAVE, M3, BUY, 905, 10);
    let f = w.fund();
    let logs = w.tick(M3, 905);
    assert_eq!(evs::<Liquidation>(&logs)[0].marginType, ISOLATED);
    // The silo's close credit ($50) netted the deficit first.
    assert_cleared(&mut w, &logs, ALICE, d(450), f);

    // An ADL fill closing the winner's last position: BOB, cross short 4 with W = −$620, is under
    // water at the 850 mark (E = −$20), so fallback tier for CAROL's 900 fill, and the only short,
    // so it is his. The fund backs a fallback cross winner down to `min(E, 0)` (P3 review): of
    // the fill's $200 off the mark it pays all of it, +$400 realized and the $200 cover leave
    // W = −$20 — his `E`, unmoved — and the end of the call clears that.
    let mut w = std_world();
    w.set_type(CAROL, M1, ISOLATED);
    w.fill(CAROL, M1, 4, 1_000);
    w.fill(BOB, M1, -4, 1_000);
    w.set_wallet(BOB, -d(620));
    let f = w.fund();
    let logs = w.tick(M1, 850);
    let adl: Vec<(Address, u64)> = evs::<Adl>(&logs).iter().map(|a| (a.adlUser, a.price)).collect();
    assert_eq!(adl, vec![(BOB, 900)]);
    assert_cleared(&mut w, &logs, BOB, d(20), f - d(200));
}

/// **R4 — a short fund pays what it has**: the wallet still ends at 0, the fund at 0, and the rest
/// is socialised (`InsuranceFundDepleted`) — the one place custody grows, by exactly that.
#[test]
fn clr_a_short_insurance_fund_pays_what_it_has_and_the_rest_is_socialised() {
    for fund in [d(500), 0] {
        let mut w = underwater();
        w.set_fund(fund as u64);
        w.rest(DAVE, M1, BUY, 850, 10);
        let logs = w.ok(ALICE, &mkt_ro(M1, SELL, 10));
        assert_cleared(&mut w, &logs, ALICE, d(1_400), fund);
        assert_eq!(w.fund(), 0);
        let dep = evs::<InsuranceFundDepleted>(&logs);
        assert_eq!((dep.len(), dep[0].marketId), (1, M1));
        if fund == 0 {
            assert!(evs::<InsuranceFundChanged>(&logs).is_empty(), "an empty fund logs no change");
        }
    }
}

/// **Q8 — never while a position is open**: a negative wallet next to an open ISOLATED position is
/// a receivable, not a clearance; nor does a cross close at a loss clear it while another cross
/// position remains.
#[test]
fn clr_never_while_any_position_is_open() {
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.fill(ALICE, M2, 1, 1_000);
    w.set_wallet(ALICE, d(100));
    let f = w.fund();
    let logs = w.make(ALICE, M1, -10, 850, RO);
    assert_eq!(w.wallet(ALICE), -d(1_400));
    assert_eq!(w.fund(), f);
    assert!(rows(&logs, ALICE).iter().all(|r| r.0 != INSURANCE_CLEAR));
    // The last one closes (at its entry): now it is cleared.
    let logs = w.make(ALICE, M2, -1, 1_000, RO);
    assert_cleared(&mut w, &logs, ALICE, d(1_400), f);
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// The never-`Err` fuzz (J4)
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// splitmix64.
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
        self.next() % n.max(1)
    }

    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize]
    }

    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }
}

/// What a fuzz run reached — the coverage the long run reports and the default run floors.
#[derive(Default, Debug, Clone)]
struct Stats {
    steps: u64,
    accepted: u64,
    refused: u64,
    ticks: u64,
    drains: u64,
    keeper_liquidations: u64,
    episodes: u64,
    episodes_ended: u64,
    cross_closes: u64,
    iso_closes: u64,
    adl_fills: u64,
    clearances: u64,
    socialised: u64,
    deferrals: u64,
    flagged_after_tick: u64,
    refused_2023: u64,
    refused_r2: u64,
    stale_skips: u64,
    exempt_after_tick: u64,
    min_wallet: i128,
    min_e: i128,
    settle_rounds: u64,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.steps += o.steps;
        self.accepted += o.accepted;
        self.refused += o.refused;
        self.ticks += o.ticks;
        self.drains += o.drains;
        self.keeper_liquidations += o.keeper_liquidations;
        self.episodes += o.episodes;
        self.episodes_ended += o.episodes_ended;
        self.cross_closes += o.cross_closes;
        self.iso_closes += o.iso_closes;
        self.adl_fills += o.adl_fills;
        self.clearances += o.clearances;
        self.socialised += o.socialised;
        self.deferrals += o.deferrals;
        self.flagged_after_tick += o.flagged_after_tick;
        self.refused_2023 += o.refused_2023;
        self.refused_r2 += o.refused_r2;
        self.stale_skips += o.stale_skips;
        self.exempt_after_tick += o.exempt_after_tick;
        self.min_wallet = self.min_wallet.min(o.min_wallet);
        self.min_e = self.min_e.min(o.min_e);
        self.settle_rounds = self.settle_rounds.max(o.settle_rounds);
    }
}

/// The fuzz's market table: whole-dollar markets (exact integers, so the identity closes to the
/// unit through funding too), different tier shapes (derived, explicit with `MM > PIM` drift,
/// Binance-like), bands on and off, clearance fees, funding of either sign.
fn fuzz_specs(n: u64) -> Vec<Spec> {
    let tiers: [&[Band]; 5] = [
        &[
            Band { lb: 0, lev: 20, ppm: 0 },
            Band { lb: 50_000, lev: 10, ppm: 0 },
            Band { lb: 200_000, lev: 4, ppm: 0 },
        ],
        &DRIFT,
        &[Band { lb: 0, lev: 5, ppm: 0 }],
        &[
            Band { lb: 0, lev: 50, ppm: 10_000 },
            Band { lb: 10_000, lev: 25, ppm: 20_000 },
            Band { lb: 50_000, lev: 10, ppm: 50_000 },
        ],
        &T10,
    ];
    let marks = [1_000, 500, 200, 100, 50];
    let bands = [10_000, 500, 1_000, 10_000, 300];
    let fees = [125, 0, 500, 0, 125];
    let funding = [Some(300), Some(-200), None, Some(100), None];
    (0..n)
        .map(|i| {
            let k = (i % 5) as usize;
            Spec {
                id: i + 1,
                bd: 0,
                pd: 0,
                mark0: marks[k],
                tiers: tiers[k].to_vec(),
                interval: 15,
                band_bps: bands[k],
                fee_bps: fees[k],
                funding: funding[k],
            }
        })
        .collect()
}

const KEEPER: Address = address!("6666666666666666666666666666666666666666");

struct Fuzz {
    w: World,
    rng: Rng,
    traders: Vec<Address>,
    /// Deep liquidity: DAVE (ISOLATED, leverage 1) and ERIN (CROSSED, $1e9).
    lps: [Address; 2],
    index: Vec<u64>,
    stalled_until: Vec<u64>,
    stats: Stats,
    /// One digest per call — two runs of a seed must agree call for call (determinism).
    digest: Vec<[u8; 32]>,
}

const LIQ_REFUSALS: [&str; 3] = [E_LIQ_HEALTHY, E_LIQ_NO_POSITION, E_LIQ_ISO_HEALTHY];

impl Fuzz {
    fn new(seed: u64, n_markets: u64, n_traders: u32, warmup: bool) -> Fuzz {
        let mut rng = Rng(seed);
        let specs = fuzz_specs(n_markets);
        let mut w = World::new(specs.clone());
        w.enroll(ERIN, BIG);
        w.enroll(KEEPER, 0);
        // A third of the seeds run on a small or an EMPTY insurance fund.
        match rng.below(3) {
            0 => w.set_fund(0),
            1 => w.set_fund(d(2_000) as u64),
            _ => {}
        }
        let mut traders = Vec::new();
        for i in 0..n_traders {
            let u = user(1_000 + i);
            w.enroll(u, d(200 + rng.below(8_000) as i128));
            w.set_spot(u, d(50_000) as u128);
            if i % 3 == 0 {
                w.ok(
                    ADMIN,
                    &crate::interface::IPerpDex::setUserFeeRatesCall {
                        user: u,
                        makerFeePpm: 200,
                        takerFeePpm: 500,
                    }
                    .abi_encode(),
                );
            }
            for s in &specs {
                if rng.chance(1, 3) {
                    w.set_type(u, s.id, ISOLATED);
                }
                // Half the time at the table's cap.
                let cap = s.tiers[0].lev as u64;
                let lev = if rng.chance(1, 2) { cap } else { 1 + rng.below(cap) };
                w.set_lev(u, s.id, lev);
            }
            traders.push(u);
        }
        if warmup {
            // A crowd: every trader opens a LONG on the first market at leverage 10 with five times
            // its wallet (inside the first bracket — the same liquidation threshold for every
            // account of a margin type) and rests a far bid on the second, so that one crash of
            // the first market takes ~60 accounts under at once, each costing a cancel and a
            // close: far more than the sweep's 50 steps.
            let (m1, m2) = (specs[0].id, specs[1].id);
            for &u in &traders {
                w.set_lev(u, m1, 10);
                let wallet = storage::load_account(&mut w.ctx, u).unwrap().perp_wallet_balance;
                let qty = ((wallet as u64 / USD as u64) * 5).min(45_000) / specs[0].mark0;
                let lp = if rng.chance(1, 2) { DAVE } else { ERIN };
                if w.attempt(lp, &gtc(m1, SELL, specs[0].mark0, qty.max(1))).is_ok() {
                    let _ = w.attempt(u, &mkt(m1, BUY, qty.max(1)));
                    w.cancel_all(lp, m1);
                }
                let _ = w.attempt(u, &gtc(m2, BUY, specs[1].mark0 / 2, 1));
            }
        }
        let index = specs.iter().map(|s| s.mark0).collect();
        let stalled_until = vec![0; specs.len()];
        Fuzz {
            w,
            rng,
            traders,
            lps: [DAVE, ERIN],
            index,
            stalled_until,
            stats: Stats::default(),
            digest: Vec::new(),
        }
    }

    fn markets(&self) -> Vec<u64> {
        self.w.markets()
    }

    /// One checked call; feeds the digest and the counters. Never-revert selectors (oracle,
    /// drain) and `liquidate()`'s refusal set are enforced here.
    fn call(&mut self, caller: Address, input: &[u8]) -> Result<Vec<Log>, String> {
        let r = self.w.attempt(caller, input);
        let sel: [u8; 4] = input[..4].try_into().unwrap();
        let mut h = Vec::new();
        h.extend_from_slice(&sel);
        match &r {
            Ok(logs) => {
                self.stats.accepted += 1;
                h.extend_from_slice(&self.w.last_out);
                for l in logs {
                    for t in l.data.topics() {
                        h.extend_from_slice(t.as_slice());
                    }
                    h.extend_from_slice(&l.data.data);
                }
                self.count(logs);
            }
            Err(e) => {
                self.stats.refused += 1;
                h.extend_from_slice(e.as_bytes());
                if e.contains("-2023") {
                    self.stats.refused_2023 += 1;
                }
                if e == E_J1 {
                    self.stats.refused_r2 += 1;
                }
                assert!(
                    sel != updateIndexPriceCall::SELECTOR && sel != drainDeferredWorkCall::SELECTOR,
                    "the oracle / drain path reverted: {e}"
                );
                if sel == liquidateCall::SELECTOR {
                    assert!(LIQ_REFUSALS.contains(&e.as_str()), "liquidate() refused with `{e}`");
                }
            }
        }
        self.digest.push(*primitives::keccak256(&h));
        r
    }

    fn count(&mut self, logs: &[Log]) {
        let s = &mut self.stats;
        s.episodes += evs::<CrossLiquidationStarted>(logs).len() as u64;
        s.episodes_ended += evs::<CrossLiquidationEnded>(logs).len() as u64;
        for l in evs::<Liquidation>(logs) {
            if l.marginType == CROSSED {
                s.cross_closes += 1;
            } else {
                s.iso_closes += 1;
            }
        }
        s.adl_fills += evs::<Adl>(logs).len() as u64;
        let paid = evs::<InsuranceFundChanged>(logs).into_iter().filter(|c| c.delta < 0);
        s.clearances += paid.count() as u64;
        s.socialised += evs::<InsuranceFundDepleted>(logs).len() as u64;
    }

    fn mark(&mut self, m: u64) -> u64 {
        self.w.mark(m)
    }

    /// The oracle publishes a new index for `m` one interval after its last publication — the
    /// clock catching up when that is ahead of it — then the detection property is checked.
    fn tick(&mut self, m: u64, index: u64) {
        let interval = self.w.spec(m).interval;
        let last = storage::load_index_price_state(&mut self.w.ctx, m).unwrap().timestamp;
        let ts = (last + interval).max(self.w.now - self.w.now % interval);
        if ts > self.w.now {
            let t = ts;
            self.w.set_now(t);
        }
        let input =
            updateIndexPriceCall { marketId: m, indexPrice: index, timestamp: ts }.abi_encode();
        let logs = self.call(ADMIN, &input).unwrap();
        self.stats.ticks += 1;
        self.index[(m - 1) as usize] = index;
        self.after_tick(m, &logs);
    }

    /// **Detection is complete** (§3.4): after `m`'s tick, no account holding a cross position in
    /// `m` is under cross maintenance and unflagged, and no isolated position in `m` is under its
    /// own — unless the sweep left work for the same block (`deferred_work`), or another
    /// liquidation in this tick reached the account after its turn (an ADL fill or a maker fill:
    /// D13, the next tick sees it).
    fn after_tick(&mut self, m: u64, logs: &[Log]) {
        let mut touched: Vec<Address> = evs::<Adl>(logs).iter().map(|a| a.adlUser).collect();
        touched.extend(evs::<Trade>(logs).iter().map(|t| t.maker));
        let deferred = self.w.deferred(m);
        self.stats.deferrals += deferred as u64;
        for u in self.traders.clone().into_iter().chain(self.lps) {
            let a = self.w.acct(u);
            let l = *a.leg(m);
            if l.amount == 0 {
                continue;
            }
            self.stats.flagged_after_tick += a.flagged as u64;
            let missed = (l.cross && a.below() && !a.flagged) || l.iso_liquidatable();
            if missed {
                assert!(
                    deferred || touched.contains(&u),
                    "detection missed {u} on {m}'s tick: {a:#?}"
                );
                self.stats.exempt_after_tick += 1;
            }
            if a.flagged && a.below() {
                // An episode left this account under: its remaining markets were stale (or out of
                // budget, recorded).
                self.stats.stale_skips += !deferred as u64;
            }
        }
    }

    fn step(&mut self, step: u64) {
        self.stats.steps += 1;
        let dt = 1 + self.rng.below(3);
        let now = self.w.now + dt;
        self.w.set_now(now);
        let markets = self.markets();
        if self.rng.chance(1, 60) {
            let m = self.rng.pick(&markets);
            self.stalled_until[(m - 1) as usize] = step + 20 + self.rng.below(60);
        }
        let u = self.rng.pick(&self.traders.clone());
        let m = self.rng.pick(&markets);
        let mk = self.mark(m);
        let lp = self.rng.pick(&self.lps);
        match self.rng.below(32) {
            0..=6 => {
                // A market order against liquidity a provider rests within 1 % of the mark.
                let cap = self.rng.pick(&[2_000u64, 10_000, 40_000, 100_000]);
                let qty = 1 + self.rng.below(cap / mk);
                let side = self.rng.pick(&[BUY, SELL]);
                let off = self.rng.below(mk / 100 + 1);
                let price = if side == BUY { mk + off } else { mk.saturating_sub(off).max(1) };
                let lq = self.call(lp, &gtc(m, 1 - side, price, qty)).is_ok();
                let _ = self.call(u, &mkt(m, side, qty));
                if lq {
                    self.cancel_all(lp, m);
                }
            }
            7..=8 => {
                // A limit away from the mark (maker liquidity of the traders).
                let side = self.rng.pick(&[BUY, SELL]);
                let off = 1 + self.rng.below(mk / 10 + 1);
                let price = if side == BUY { mk.saturating_sub(off).max(1) } else { mk + off };
                let qty = 1 + self.rng.below(10_000 / mk + 1);
                let flags = if self.rng.chance(1, 4) { RO } else { 0 };
                let _ = self.call(u, &order(m, side, price, qty, LIMIT, GTC, flags));
            }
            9 => {
                // A provider takes whatever rests (the traders' maker fills: D13, R1, the freeze).
                let qty = 1 + self.rng.below(20_000 / mk + 1);
                let side = self.rng.pick(&[BUY, SELL]);
                let _ = self.call(lp, &mkt(m, side, qty));
            }
            10 => {
                // A reduce-only close of the whole position: taker, or maker the provider takes.
                let a = self.w.pos(u, m).amount;
                if a != 0 {
                    let side = if a > 0 { SELL } else { BUY };
                    let off = self.rng.below(mk / 50 + 1);
                    let price = if side == BUY { mk + off } else { mk.saturating_sub(off).max(1) };
                    let q = a.unsigned_abs();
                    if self.rng.chance(1, 2) {
                        let lq = self.call(lp, &gtc(m, 1 - side, price, q)).is_ok();
                        let _ = self.call(u, &mkt_ro(m, side, q));
                        if lq {
                            self.cancel_all(lp, m);
                        }
                    } else if self.call(u, &gtc_ro(m, side, price, q)).is_ok() {
                        let _ = self.call(lp, &mkt(m, 1 - side, q));
                    }
                }
            }
            11 => {
                let ids: Vec<Id> = storage::load_buy_orders(&mut self.w.ctx, u, m)
                    .unwrap()
                    .iter()
                    .chain(storage::load_sell_orders(&mut self.w.ctx, u, m).unwrap().iter())
                    .map(|e| e.order_id)
                    .collect();
                if !ids.is_empty() {
                    let id = ids[self.rng.below(ids.len() as u64) as usize];
                    let cancel = cancelOrderCall { orderId: id.into(), marketId: m }.abi_encode();
                    let _ = self.call(u, &cancel);
                }
            }
            12 => {
                let cap = self.w.spec(m).tiers[0].lev as u64;
                let lev = 1 + self.rng.below(cap);
                let _ = self.call(u, &setLeverageCall { marketId: m, leverage: lev }.abi_encode());
            }
            13 => {
                // Money out, decided by X (maxWithdrawAmount).
                let x = self.w.account_view(u).maxWithdrawAmount as i128;
                let amount = match self.rng.below(3) {
                    0 => x,
                    1 => x + 1,
                    _ => d(1 + self.rng.below(3_000) as i128),
                }
                .max(1);
                let r = self.call(u, &transferFromPerpCall { amount: amount as u64 }.abi_encode());
                assert_eq!(r.is_ok(), amount <= x, "money out against X = {x}: {amount} {r:?}");
            }
            14 => {
                let amount = d(1 + self.rng.below(5_000) as i128) as u64;
                let _ = self.call(u, &transferToPerpCall { amount }.abi_encode());
            }
            15 => {
                let t = if self.w.pos(u, m).margin_type == CROSSED { ISOLATED } else { CROSSED };
                let switch = setMarginTypeCall { marketId: m, marginType: t }.abi_encode();
                let _ = self.call(u, &switch);
            }
            16 => {
                // A protective conditional: a closePosition stop or a reduce-only trailing stop.
                let a = self.w.pos(u, m).amount;
                if a != 0 {
                    let side = if a > 0 { SELL } else { BUY };
                    let r = 2 + self.rng.below(18);
                    let input = if self.rng.chance(1, 2) {
                        let trig = if a > 0 { mk - mk * r / 100 } else { mk + mk * r / 100 };
                        stop_market(m, side, trig.max(1), 0, CLOSE_POSITION)
                    } else {
                        trailing(m, side, a.unsigned_abs(), 10 * (1 + self.rng.below(100)))
                    };
                    let _ = self.call(u, &input);
                }
            }
            17 => {
                self.stats.keeper_liquidations += 1;
                let v = self.rng.pick(&self.traders.clone());
                let _ = self.call(KEEPER, &liquidate(v, m));
            }
            18 => {
                self.stats.drains += 1;
                let _ = self.call(KEEPER, &drain(m));
            }
            19 => {
                // Stress: a cross leg closed as a maker 15–80 % through the mark — the loss lands
                // in W whatever its size, which is how a wallet goes deeply negative (no per-fill
                // bad debt for a cross position). Then nothing: the next ticks find the account.
                let a = self.w.acct(u);
                if let Some(l) = a.legs.iter().find(|l| l.cross && l.amount != 0).copied() {
                    let lm = self.w.mark(l.market);
                    let r = 15 + self.rng.below(66);
                    let side = if l.amount > 0 { SELL } else { BUY };
                    let price =
                        if l.amount > 0 { (lm - lm * r / 100).max(1) } else { lm + lm * r / 100 };
                    self.cancel_all(u, l.market);
                    let q = l.amount.unsigned_abs();
                    if self.call(u, &gtc_ro(l.market, side, price, q)).is_ok() {
                        let _ = self.call(lp, &mkt(l.market, 1 - side, q));
                    }
                }
            }
            20 if self.rng.chance(1, 3) => {
                // A market-wide move: every live oracle publishes a 15–50 % crash (or a spike), one
                // market per tick — accounts go under together, the sweep budget is shared out,
                // and the episodes meet each other's stale marks.
                let down = self.rng.chance(3, 4);
                for m in markets {
                    let k = (m - 1) as usize;
                    if self.stalled_until[k] > step {
                        continue;
                    }
                    let cur = self.index[k];
                    let delta = cur * (15 + self.rng.below(36)) / 100;
                    let next = if down { cur.saturating_sub(delta).max(1) } else { cur + delta };
                    self.tick(m, next.min(1_000_000));
                }
            }
            _ => {
                // An oracle tick: small moves mostly, gaps, crashes and spikes.
                let k = (m - 1) as usize;
                if self.stalled_until[k] > step {
                    return;
                }
                let cur = self.index[k];
                let max_price = 1_000_000;
                let roll = self.rng.below(100);
                let bp: u64 = match roll {
                    0..=59 => 10 + self.rng.below(300),
                    60..=79 => 300 + self.rng.below(700),
                    80..=89 => 1_500 + self.rng.below(3_500),
                    90..=94 => 1_500 + self.rng.below(4_500),
                    _ => 0,
                };
                let up = match roll {
                    80..=89 => false,
                    90..=94 => true,
                    _ => self.rng.chance(1, 2),
                };
                let delta = (cur as u128 * bp as u128 / 10_000) as u64;
                let next = if up {
                    (cur + delta).min(max_price)
                } else {
                    cur.saturating_sub(delta).max(1)
                };
                self.tick(m, next);
            }
        }
        let mut min_w = self.stats.min_wallet;
        let mut min_e = self.stats.min_e;
        for t in self.traders.clone() {
            let a = self.w.acct(t);
            min_w = min_w.min(a.w);
            if a.has_cross {
                min_e = min_e.min(a.e);
            }
        }
        self.stats.min_wallet = min_w;
        self.stats.min_e = min_e;
    }

    fn cancel_all(&mut self, u: Address, m: u64) {
        let ids: Vec<Id> = storage::load_buy_orders(&mut self.w.ctx, u, m)
            .unwrap()
            .iter()
            .chain(storage::load_sell_orders(&mut self.w.ctx, u, m).unwrap().iter())
            .map(|e| e.order_id)
            .collect();
        for id in ids {
            let _ = self.call(u, &cancelOrderCall { orderId: id.into(), marketId: m }.abi_encode());
        }
    }

    /// The end of a run: every oracle publishes again on a moving clock (no market stale) and every
    /// market is drained, round after round, until no account is flagged or under maintenance.
    /// Then EVERY account is either healthy — cross at maintenance, each isolated position above
    /// its own — or flat with a wallet at or above zero, and the views agree with the model.
    fn settle(&mut self) {
        let markets = self.markets();
        for k in 0..self.stalled_until.len() {
            self.stalled_until[k] = 0;
        }
        let bad = |f: &mut Fuzz| -> Vec<Address> {
            let mut v = Vec::new();
            for u in f.traders.clone().into_iter().chain(f.lps) {
                let a = f.w.acct(u);
                if a.flagged || a.below() || a.legs.iter().any(|l| l.iso_liquidatable()) {
                    v.push(u);
                }
            }
            v
        };
        let mut rounds = 0;
        while !bad(self).is_empty() {
            rounds += 1;
            assert!(rounds <= 60, "the settle did not converge: {:?}", bad(self));
            for &m in &markets {
                let idx = self.index[(m - 1) as usize];
                self.tick(m, idx);
            }
            for &m in &markets {
                for _ in 0..20 {
                    self.call(KEEPER, &drain(m)).unwrap();
                    if !drainDeferredWorkCall::abi_decode_returns(&self.w.last_out).unwrap() {
                        break;
                    }
                }
            }
        }
        self.stats.settle_rounds = rounds;
        for u in self.traders.clone().into_iter().chain(self.lps) {
            let a = self.w.acct(u);
            assert!(!a.flagged, "{u} is still flagged");
            if a.flat() {
                assert!(a.w >= 0, "{u} flat with W = {}", a.w);
            } else {
                assert!(!a.below(), "{u} under cross maintenance: {a:#?}");
                assert!(!a.legs.iter().any(|l| l.iso_liquidatable()), "{u} isolated under: {a:#?}");
            }
            self.w.check_view(u);
        }
    }
}

/// Runs one seed; returns its stats and per-call digest.
fn fuzz_run(
    seed: u64,
    steps: u64,
    n_markets: u64,
    n_traders: u32,
    warmup: bool,
) -> (Stats, Vec<[u8; 32]>) {
    let mut f = Fuzz::new(seed, n_markets, n_traders, warmup);
    f.stats.min_wallet = i128::MAX;
    f.stats.min_e = i128::MAX;
    if warmup {
        // The crowd's crash: the first market loses a quarter in one tick.
        let crash = f.index[0] * 3 / 4;
        f.tick(1, crash);
    }
    for step in 0..steps {
        f.step(step);
    }
    f.settle();
    (f.stats.clone(), f.digest)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Runs `seeds` seeds from `base`, each wrapped so a failure names its seed; aggregates the stats.
fn fuzz_many(
    base: u64,
    seeds: u64,
    steps: u64,
    n_markets: u64,
    n_traders: u32,
    warmup: bool,
) -> Stats {
    let mut total = Stats { min_wallet: i128::MAX, min_e: i128::MAX, ..Stats::default() };
    let mut failed = Vec::new();
    for i in 0..seeds {
        let seed = base.wrapping_add(i.wrapping_mul(0x5851_f42d_4c95_7f2d));
        let r = std::panic::catch_unwind(|| fuzz_run(seed, steps, n_markets, n_traders, warmup));
        match r {
            Ok((s, _)) => total.add(&s),
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                failed.push((seed, msg));
            }
        }
    }
    eprintln!(
        "fuzz {n_markets} markets × {n_traders} traders, {seeds} seeds × {steps} steps: {total:#?}"
    );
    let shown = &failed[..failed.len().min(5)];
    assert!(failed.is_empty(), "{} of {seeds} seeds failed: {shown:#?}", failed.len());
    total
}

/// **The never-`Err` fuzz.** Five markets (every tier shape, bands on and off, fees, funding of
/// both signs), 10 traders with random margin types per market and leverage up to each table's
/// cap, two liquidity providers (one isolated, one cross), an empty or small insurance fund on a
/// third of the seeds each. Steps: market and limit orders, reduce-only closes as taker and maker,
/// the providers taking resting orders, cancels, leverage, money out (asserted against X) and in,
/// mode switches, closePosition and trailing stops, keeper `liquidate()` (only the three healthy /
/// flat refusals allowed) and drains, deep losses realised as a maker, and oracle ticks — small
/// moves, gaps, crashes and spikes, market-wide crashes one market per tick, oracles stalling
/// while the clock runs (stale marks).
///
/// Every call: the oracle and the drain never revert or panic, refusals are write-clean, the
/// custody identity holds (only the socialised shortfall moves it), `Σ amount == 0`, the stored
/// invariants. After every tick: detection is complete. At the end: every account healthy or flat
/// at `W ≥ 0`, nothing flagged, the views equal to the model. (`fuzz_is_deterministic`: a seed
/// replayed gives the same digest for every call.)
///
/// `P3_FUZZ_SEEDS` / `P3_FUZZ_STEPS` / `P3_FUZZ_BASE` scale it for a long run.
#[test]
fn fuzz_never_err_conserves_and_ends_healthy_five_markets() {
    let seeds = env_u64("P3_FUZZ_SEEDS", 4);
    let steps = env_u64("P3_FUZZ_STEPS", 1_500);
    let base = env_u64("P3_FUZZ_BASE", 0x0003_c0ff_ee00_2026);
    let s = fuzz_many(base, seeds, steps, 5, 10, false);
    if seeds >= 4 && steps >= 1_000 {
        assert!(s.episodes > 0 && s.episodes_ended > 0 && s.cross_closes > 0, "cross episodes");
        assert!(s.iso_closes > 0 && s.adl_fills > 0, "isolated closes and ADL");
        assert!(s.clearances + s.socialised > 0, "a negative wallet cleared");
        assert!(s.min_wallet < 0 && s.min_e < 0, "a negative wallet and E < 0 reached");
        assert!(s.refused_2023 > 0, "the freeze was met");
        assert!(s.flagged_after_tick > 0, "an episode outlived its tick");
    }
}

/// The fuzz at the per-user market cap: 16 markets, 8 traders.
#[test]
fn fuzz_never_err_sixteen_markets() {
    let seeds = env_u64("P3_FUZZ16_SEEDS", 1);
    let steps = env_u64("P3_FUZZ16_STEPS", 1_200);
    let base = env_u64("P3_FUZZ_BASE", 0x0016_c0ff_ee00_2026);
    fuzz_many(base, seeds, steps, 16, 8, false);
}

/// The fuzz with a CROWD: 60 traders on three markets, each opened near its leverage at the start,
/// so one market-wide move puts more accounts under in one tick than the sweep's 50 steps reach.
/// The sweep defers (`deferred_work`), the drains and later ticks resume, and every property of the
/// fuzz holds through it.
#[test]
fn fuzz_never_err_crowd_exhausts_the_sweep_budget() {
    let seeds = env_u64("P3_FUZZ_CROWD_SEEDS", 1);
    let steps = env_u64("P3_FUZZ_CROWD_STEPS", 300);
    let base = env_u64("P3_FUZZ_BASE", 0x00c0_0d00_2026);
    let s = fuzz_many(base, seeds, steps, 3, 60, true);
    assert!(s.deferrals > 0, "the sweep budget was never exhausted");
}

/// A replayed seed is call-for-call identical.
#[test]
fn fuzz_is_deterministic() {
    let (_, a) = fuzz_run(0x00de_7e2a_1234, 400, 5, 6, false);
    let (_, b) = fuzz_run(0x00de_7e2a_1234, 400, 5, 6, false);
    assert_eq!(a.len(), b.len());
    assert!(a == b, "two runs of one seed diverged");
}
