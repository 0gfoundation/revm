//! **Cross margin P4 — an independent spec suite for the account's cross legs and the health check
//! priced from them.**
//!
//! Written from `SPEC_cross_margin.md` ("P4 — decisions" D1–D4: the per-user list of the state of
//! every position CROSSED in force, maintained by the position write hook, and the account-level
//! health `W`, `E = W + Σ_C (PF_k + uPnL_k)`, `Σ_C MM_k` priced from it under the TOTAL policy J4),
//! NOT from the implementation's tests (`cross_legs_tests`) nor from its internals. Every state
//! change goes through the call entry point (`run_perp_dex_call`) on an [`InMemoryHost`] whose
//! blocks are ended explicitly, so the legs also travel through the committed store. The only
//! fixture writes are the ERC-20 balance a `deposit` pulls from, the block clock, and the last
//! traded price cleared before an oracle tick so that the mark is exactly the published index.
//!
//! # The model — the spec's terms, recomputed from the position records
//!
//! ```text
//! legs(u)    [(k, amount_k, vQuote_k, anchor_k)  for every market k, in market order,
//!             where the stored position of (u, k) is CROSSED (byte 0) and amount_k != 0]
//! N_k        trunc(amount_k · mark_k · 10^6 / 10^(bd+pd))
//! uPnL_k     N_k + vQuote_k                     (0 when mark_k = 0, J4)
//! MM_k       the slice form over the tier table set through setMarginTiers, at |N_k|
//! PF_k       −trunc(amount_k · (I_k − anchor_k) / 10^(bd+pd)), I_k the FundingState index
//! W          the stored perp wallet
//! E          W + Σ_legs (PF_k + uPnL_k)
//! ΣMM        Σ_legs MM_k
//! ```
//!
//! # What every call is held to ([`World::attempt`])
//!
//! * REFUSED: no perp write, no log, and every account's stored legs bit-identical.
//! * ACCEPTED: for EVERY account — the stored legs equal `legs(u)`; the in-block resident list is
//!   never an empty list (an empty list is a delete); `cross_health` reports `W`, `E`, `ΣMM`,
//!   `has_cross = legs(u) ≠ ∅`, no faulted leg, and per leg `(k, uPnL_k, MM_k, PF_k)` exactly as
//!   the model computes them.
//! * At every block end: the committed bytes of every account's legs key decode to `legs(u)`, and
//!   the key is ABSENT for an account with no cross leg.
//!
//! # Map
//!
//! * `route_*` — one scenario per route that moves a position: taker and maker fills (open,
//!   increase, partial close, flip, close), a self-trade, funding (pending, settled, and the
//!   zero-payment re-anchor of a cancel-only match participant), setMarginType both ways on a flat
//!   market, isolated margin add/remove next to cross legs, fired closePosition / reduce-only stops
//!   and trailing stops, sub-accounts (`internalTransfer`, `asSubAccount` with placeOrder / batch /
//!   setMarginType), batches, the cross liquidation episode, ADL with a cross loser and a cross
//!   winner on either side of an isolated one, the negative-wallet clearance, `liquidate()`, the
//!   drain, sixteen markets and mixed accounts.
//! * `refused_*` — refusals on accounts holding legs leave the legs untouched.
//! * `placement_*` — a placement or a cancel writes no legs key (D2's reason for its own blob).
//! * `fuzz_*` — randomised runs (mixed margin types, up to 16 markets, liquidations, ADL,
//!   clearances, funding, sub-accounts, batches, block ends) with the per-call check;
//!   `CROSS_LEGS_FUZZ_SEEDS` / `CROSS_LEGS_FUZZ_STEPS` / `CROSS_LEGS_FUZZ_BASE` scale them.

use alloy_sol_types::{SolCall, SolError, SolEvent};
use primitives::{address, Address, Bytes, FixedBytes, U256};

use crate::call::PerpOutput;
use crate::host::{InMemoryHost, PerpHost};
use crate::interface::IPerpDex::{
    addMarketCall, addPositionMarginCall, asSubAccountCall, batchPlaceOrdersCall, cancelOrderCall,
    createSubAccountCall, depositCall, depositInsuranceFundCall, drainDeferredWorkCall,
    getAccountCall, getAccountReturn, initAdminCall, internalTransferCall, liquidateCall,
    placeAlgoOrderCall, placeOrderCall, removePositionMarginCall, setLeverageCall,
    setMarginTiersCall, setMarginTypeCall, setUserFeeRatesCall, transferFromPerpCall,
    transferToPerpCall, updateIndexPriceCall, AccountBalanceChanged, Adl, AlgoOrderTriggered,
    CrossLiquidationEnded, CrossLiquidationStarted, FundingSettled, Liquidation, OrderCancelled,
    PlaceItem, Trade,
};
use crate::typed_store::Resident;
use crate::types::{CrossLegState, CrossLegs};
use crate::{run_perp_dex_call, storage, USDC_ADDRESS};

// ── Fixture ──────────────────────────────────────────────────────────────────────────────────

type Log = primitives::Log;
type Id = [u8; 32];

/// One dollar in quote units (6 dp).
const USD: i128 = 1_000_000;
const GAS: u64 = 30_000_000;
/// $1e9.
const BIG: i128 = 1_000_000_000 * USD;

const ALICE: Address = address!("a11ce00000000000000000000000000000000001");
const BOB: Address = address!("b0b0000000000000000000000000000000000002");
const CAROL: Address = address!("ca20100000000000000000000000000000000003");
/// Deep liquidity: ISOLATED at leverage 1 everywhere, $1e9, fee-free.
const DAVE: Address = address!("da5e000000000000000000000000000000000004");
/// A keeper (`liquidate()`, `drainDeferredWork`).
const KEEPER: Address = address!("6eee000000000000000000000000000000000005");
/// Admin = oracle.
const ADMIN: Address = address!("ad00000000000000000000000000000000000006");

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MKT: u8 = 1;
const GTC: u8 = 0;
const FOK: u8 = 2;
const RO: u8 = 0b01;
const CLOSE_POSITION: u8 = 0b10;
const CROSSED: u8 = 0;
const ISOLATED: u8 = 1;
/// `AccountBalanceChanged.reason` 7 = INSURANCE_CLEAR.
const INSURANCE_CLEAR: u8 = 7;
/// `internalTransfer` wallets.
const PERP: u8 = 1;

fn d(usd: i128) -> i128 {
    usd * USD
}

/// One tier row as `setMarginTiers` takes it: `lb` in whole dollars, `ppm == 0` = the derived rate
/// `1 / (2 · lev)`.
#[derive(Clone, Copy, Debug)]
struct Band {
    lb: u64,
    lev: u32,
    ppm: u32,
}

/// 5 % maintenance (derived, leverage up to 10).
const T10: [Band; 1] = [Band { lb: 0, lev: 10, ppm: 0 }];
/// Three derived brackets.
const T3: [Band; 3] = [
    Band { lb: 0, lev: 20, ppm: 0 },
    Band { lb: 50_000, lev: 10, ppm: 0 },
    Band { lb: 200_000, lev: 4, ppm: 0 },
];
/// Explicit, Binance-shaped rates.
const TB: [Band; 3] = [
    Band { lb: 0, lev: 50, ppm: 10_000 },
    Band { lb: 10_000, lev: 25, ppm: 20_000 },
    Band { lb: 50_000, lev: 10, ppm: 50_000 },
];

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
    /// `Some(interest ppm)`: a funding epoch every interval.
    funding: Option<i64>,
}

impl Spec {
    /// A whole-dollar market (base and price decimals 0): every value is an exact integer.
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

fn user(i: u32) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0x5e;
    b[16..20].copy_from_slice(&i.to_be_bytes());
    Address::from(b)
}

// ── The model ────────────────────────────────────────────────────────────────────────────────

/// `p · q · 10^6 / 10^(bd + pd)`, truncated toward zero (`q` signed).
fn value(p: u64, q: i128, bd: u32, pd: u32) -> i128 {
    p as i128 * q * USD / 10i128.pow(bd + pd)
}

/// The slice form: every bracket the notional reaches adds `⌊width · rate⌋`.
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

/// One leg's health terms as the model prices them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LegTerms {
    market: u64,
    upnl: i128,
    mm: i128,
    pf: i128,
}

/// The model's account health.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Health {
    w: i128,
    e: i128,
    cmm: i128,
    has_cross: bool,
    legs: Vec<LegTerms>,
}

/// What a run reached: the routes the checked calls exercised.
#[derive(Clone, Debug, Default)]
struct Reached {
    calls: u64,
    refused: u64,
    blocks: u64,
    /// Accepted calls after which some account's legs differ from before the call.
    legs_moved: u64,
    /// …of which some account's legs list went from non-empty to empty (a key delete).
    legs_emptied: u64,
    /// A leg's funding anchor moved with its amount and vQuote unchanged.
    anchor_only: u64,
    max_legs: usize,
    cross_closes: u64,
    iso_closes: u64,
    episodes: u64,
    adl_fills: u64,
    adl_cross_winner: u64,
    clearances: u64,
    funding_settled: u64,
    algo_fired: u64,
    refused_with_legs: u64,
}

struct World {
    h: InMemoryHost,
    users: Vec<Address>,
    specs: Vec<Spec>,
    /// The return bytes of the last call made through [`World::attempt`].
    last_out: Vec<u8>,
    reached: Reached,
}

fn revert_reason(out: &PerpOutput) -> String {
    alloy_sol_types::Revert::abi_decode(&out.bytes)
        .map(|r| r.reason)
        .unwrap_or_else(|_| format!("<undecodable revert {:?}>", out.bytes))
}

fn evs<E: SolEvent>(logs: &[Log]) -> Vec<E> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|l| E::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .collect()
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

fn mkt(m: u64, side: u8, qty: u64) -> Vec<u8> {
    order(m, side, 0, qty, MKT, GTC, 0)
}

fn item(m: u64, side: u8, price: u64, qty: u64, kind: u8, flags: u8) -> PlaceItem {
    PlaceItem {
        marketId: m,
        side,
        price,
        quantity: qty,
        orderType: kind,
        tif: GTC,
        clientOrderId: FixedBytes::default(),
        flags,
    }
}

/// A STOP_MARKET conditional (`orderType 1`): `flags` RO or CLOSE_POSITION.
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

fn as_sub(sub: Address, inner: Vec<u8>) -> Vec<u8> {
    asSubAccountCall { subAccount: sub, data: Bytes::from(inner) }.abi_encode()
}

impl World {
    // ── construction (all through the shell) ──

    fn new(specs: Vec<Spec>) -> World {
        let mut w = World {
            h: InMemoryHost::new(1_000_005),
            users: Vec::new(),
            specs: specs.clone(),
            last_out: Vec::new(),
            reached: Reached::default(),
        };
        w.ok(ADMIN, &initAdminCall { admin: ADMIN }.abi_encode());
        for s in &specs {
            let funding_interval = if s.funding.is_some() { s.interval } else { 0 };
            w.ok(
                ADMIN,
                &addMarketCall {
                    marketId: s.id,
                    baseDecimals: s.bd,
                    priceDecimals: s.pd,
                    tickSize: 1,
                    stepSize: 1,
                    minQuantity: 1,
                    // `maxPrice · maxQuantity` stays a valid `i64` value.
                    maxQuantity: 1_000_000 * 10u64.pow(s.bd),
                    maxPrice: 1_000_000 * 10u64.pow(s.pd),
                    priceUpdateInterval: s.interval,
                    fundingInterval: funding_interval,
                    interestRate: s.funding.unwrap_or(0),
                    liquidationFeeRateBps: s.fee_bps,
                    initialMarkPrice: s.mark0,
                    priceBandBps: s.band_bps,
                }
                .abi_encode(),
            );
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
        }
        // The insurance fund, from the admin's own deposit.
        w.enroll(ADMIN, BIG);
        w.ok(ADMIN, &depositInsuranceFundCall { amount: BIG as u64 }.abi_encode());
        w.enroll(DAVE, BIG);
        w.enroll(KEEPER, 0);
        for s in specs {
            w.set_type(DAVE, s.id, ISOLATED);
            w.set_lev(DAVE, s.id, 1);
            w.tick(s.id, s.mark0);
        }
        w.end_block();
        w
    }

    fn spec(&self, m: u64) -> &Spec {
        self.specs.iter().find(|s| s.id == m).unwrap()
    }

    fn markets(&self) -> Vec<u64> {
        self.specs.iter().map(|s| s.id).collect()
    }

    /// `u` joins the checked set with `wallet` in its perp wallet: the ERC-20 balance a `deposit`
    /// pulls from is the fixture; `deposit` and `transferToPerp` go through the shell.
    fn enroll(&mut self, u: Address, wallet: i128) {
        if !self.users.contains(&u) {
            self.users.push(u);
        }
        if wallet > 0 {
            self.fund(u, wallet);
        }
    }

    fn fund(&mut self, u: Address, amount: i128) {
        let held = self.h.external_balance(USDC_ADDRESS, u).unwrap();
        self.h.set_external_balance(USDC_ADDRESS, u, held + U256::from(amount as u128)).unwrap();
        self.ok(u, &depositCall { amount: U256::from(amount as u128) }.abi_encode());
        self.ok(u, &transferToPerpCall { amount: amount as u64 }.abi_encode());
    }

    fn now(&self) -> u64 {
        self.h.now
    }

    fn set_now(&mut self, t: u64) {
        self.h.now = t;
    }

    // ── reads ──

    fn pos(&mut self, u: Address, m: u64) -> crate::types::PerpPosition {
        storage::load_position(&mut self.h, u, m).unwrap()
    }

    fn wallet(&mut self, u: Address) -> i128 {
        storage::load_account(&mut self.h, u).unwrap().perp_wallet_balance as i128
    }

    fn flagged(&mut self, u: Address) -> bool {
        storage::load_account(&mut self.h, u).unwrap().in_liquidation()
    }

    fn mark(&mut self, m: u64) -> u64 {
        storage::load_market_ref(&mut self.h, m).unwrap().unwrap().mark_price
    }

    fn deferred(&mut self, m: u64) -> bool {
        storage::load_market_ref(&mut self.h, m).unwrap().unwrap().deferred_work != 0
    }

    fn stored_legs(&mut self, u: Address) -> Vec<CrossLegState> {
        storage::load_cross_legs_ref(&mut self.h, u).unwrap().0.clone()
    }

    fn account_view(&mut self, u: Address) -> getAccountReturn {
        let out = run_perp_dex_call(
            &getAccountCall { user: u }.abi_encode(),
            GAS,
            KEEPER,
            U256::ZERO,
            true,
            &mut self.h,
        )
        .unwrap();
        assert!(!out.reverted, "getAccount reverted: {}", revert_reason(&out));
        getAccountCall::abi_decode_returns(&out.bytes).unwrap()
    }

    // ── the model ──

    /// `legs(u)` — from the position records, in market id order.
    fn model_legs(&mut self, u: Address) -> Vec<CrossLegState> {
        let mut legs = Vec::new();
        let mut markets = self.markets();
        markets.sort_unstable();
        for m in markets {
            let p = self.pos(u, m);
            if p.margin_type == CROSSED && p.amount != 0 {
                legs.push(CrossLegState {
                    market_id: m,
                    amount: p.amount,
                    v_quote_balance: p.v_quote_balance,
                    last_funding_index: p.last_funding_index,
                });
            }
        }
        legs
    }

    /// The model's health of `u`, priced from `legs(u)`, the marks, the FundingState indices and
    /// the tier tables this world set.
    fn model_health(&mut self, u: Address) -> Health {
        let w = self.wallet(u);
        let mut hl = Health { w, e: w, cmm: 0, has_cross: false, legs: Vec::new() };
        for l in self.model_legs(u) {
            let s = self.spec(l.market_id).clone();
            let mark = self.mark(l.market_id);
            let funding = storage::load_funding_state(&mut self.h, l.market_id).unwrap();
            let index = funding.cumulative_funding_index;
            let scale = 10i128.pow(s.bd + s.pd);
            let n = value(mark, l.amount as i128, s.bd, s.pd);
            let upnl = if mark == 0 { 0 } else { n + l.v_quote_balance as i128 };
            let mm = slice_mm(&s.tiers, n);
            let pf = -(l.amount as i128 * (index - l.last_funding_index) / scale);
            hl.e += upnl + pf;
            hl.cmm += mm;
            hl.has_cross = true;
            hl.legs.push(LegTerms { market: l.market_id, upnl, mm, pf });
        }
        hl
    }

    /// The engine's `cross_health`, in the model's shape.
    fn engine_health(&mut self, u: Address) -> (Health, u8) {
        let mut legs = Vec::new();
        let h = crate::margin_view::cross_health(&mut self.h, u, Some(&mut legs)).unwrap();
        let legs = legs
            .iter()
            .map(|l| LegTerms {
                market: l.market_id,
                upnl: l.upnl,
                mm: l.mm,
                pf: l.pending_funding,
            })
            .collect();
        (Health { w: h.w, e: h.e, cmm: h.cross_mm, has_cross: h.has_cross, legs }, h.faulted_legs)
    }

    /// The per-call check (module docs) for every account.
    fn check(&mut self, at: &str) {
        for u in self.users.clone() {
            let want = self.model_legs(u);
            assert!(want.len() <= crate::types::MAX_USER_MARKETS, "{at}: {u} legs {want:?}");
            let got = self.stored_legs(u);
            assert_eq!(got, want, "{at}: {u}'s stored cross legs");
            if let Resident::Hit(v) = self.h.live.cross_legs(u) {
                assert!(!v.is_empty(), "{at}: {u}'s legs resident as an EMPTY list, not a delete");
            }
            let model = self.model_health(u);
            let (engine, faulted) = self.engine_health(u);
            assert_eq!(faulted, 0, "{at}: {u} faulted legs");
            assert_eq!(engine, model, "{at}: {u}'s cross health");
            self.reached.max_legs = self.reached.max_legs.max(want.len());
        }
    }

    /// The legs every account holds now (the refused-call witness and the moved-legs counter).
    fn all_legs(&mut self) -> Vec<Vec<CrossLegState>> {
        self.users.clone().into_iter().map(|u| self.stored_legs(u)).collect()
    }

    /// Ends the block: the delta reaches the committed store and the live store is reset. Then
    /// every account's committed legs key decodes to `legs(u)`, absent when that is empty.
    fn end_block(&mut self) {
        let _ = self.h.end_block();
        self.reached.blocks += 1;
        for u in self.users.clone() {
            let want = self.model_legs(u);
            let key = perp_core::keys::cross_legs_key(u);
            match self.h.committed.get(&key) {
                None => assert!(want.is_empty(), "block end: {u} has legs {want:?} uncommitted"),
                Some((bytes, _)) => {
                    assert!(!want.is_empty(), "block end: {u} holds no leg, the key is stored");
                    let got: CrossLegs = perp_core::codec::decode(bytes).unwrap();
                    assert_eq!(got.0, want, "block end: {u}'s committed cross legs");
                }
            }
        }
        self.check("after the block end");
    }

    // ── the shell ──

    /// One shell call, checked whichever way it goes — see the module docs.
    fn attempt(&mut self, caller: Address, input: &[u8]) -> Result<Vec<Log>, String> {
        let legs_before = self.all_legs();
        let writes = self.h.perp_write_count();
        let n_logs = self.h.logs.len();
        let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, &mut self.h).unwrap();
        let logs = self.h.logs.split_off(n_logs);
        self.last_out = out.bytes.to_vec();
        self.reached.calls += 1;
        let sel: [u8; 4] = input[..4].try_into().unwrap();
        if out.reverted {
            let reason = revert_reason(&out);
            assert_eq!(self.h.perp_write_count(), writes, "[{reason}] the refused call wrote");
            assert!(logs.is_empty(), "[{reason}] the refused call logged");
            assert_eq!(self.all_legs(), legs_before, "[{reason}] the refused call moved legs");
            self.reached.refused += 1;
            if legs_before.iter().any(|l| !l.is_empty()) {
                self.reached.refused_with_legs += 1;
            }
            return Err(reason);
        }
        self.check(&format!("after {sel:?} by {caller}"));
        let legs_after = self.all_legs();
        if legs_after != legs_before {
            self.reached.legs_moved += 1;
            let emptied =
                legs_before.iter().zip(&legs_after).any(|(b, a)| !b.is_empty() && a.is_empty());
            self.reached.legs_emptied += emptied as u64;
            let anchor_only = legs_before.iter().zip(&legs_after).any(|(b, a)| {
                b.iter().any(|x| {
                    a.iter().any(|y| {
                        x.market_id == y.market_id
                            && (x.amount, x.v_quote_balance) == (y.amount, y.v_quote_balance)
                            && x.last_funding_index != y.last_funding_index
                    })
                })
            });
            self.reached.anchor_only += anchor_only as u64;
        }
        self.count(&logs);
        Ok(logs)
    }

    fn count(&mut self, logs: &[Log]) {
        let r = &mut self.reached;
        for l in evs::<Liquidation>(logs) {
            if l.marginType == CROSSED {
                r.cross_closes += 1;
            } else {
                r.iso_closes += 1;
            }
        }
        r.episodes += evs::<CrossLiquidationStarted>(logs).len() as u64;
        let adl = evs::<Adl>(logs);
        r.adl_fills += adl.len() as u64;
        let settled = evs::<FundingSettled>(logs);
        r.funding_settled += settled.iter().filter(|f| f.amount != 0).count() as u64;
        r.algo_fired += evs::<AlgoOrderTriggered>(logs).len() as u64;
        let clear = evs::<AccountBalanceChanged>(logs);
        r.clearances += clear.iter().filter(|a| a.reason == INSURANCE_CLEAR).count() as u64;
        for a in adl {
            let winner = a.adlUser;
            let p = storage::load_position(&mut self.h, winner, a.marketId).unwrap();
            self.reached.adl_cross_winner += (p.margin_type == CROSSED) as u64;
        }
    }

    fn ok(&mut self, caller: Address, input: &[u8]) -> Vec<Log> {
        self.attempt(caller, input).unwrap_or_else(|e| panic!("unexpected revert: {e}"))
    }

    /// A refusal whose reason contains `expect`.
    fn rejected(&mut self, caller: Address, input: &[u8], expect: &str) {
        match self.attempt(caller, input) {
            Ok(_) => panic!("expected a revert containing `{expect}`, but the call SUCCEEDED"),
            Err(e) => assert!(e.contains(expect), "expected `{expect}`, got `{e}`"),
        }
    }

    // ── shortcuts ──

    fn set_type(&mut self, u: Address, m: u64, t: u8) {
        self.ok(u, &setMarginTypeCall { marketId: m, marginType: t }.abi_encode());
    }

    fn set_lev(&mut self, u: Address, m: u64, l: u64) {
        self.ok(u, &setLeverageCall { marketId: m, leverage: l }.abi_encode());
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
        logs
    }

    /// `maker` rests `qty` (signed) at `price` (`flags` RO or 0) and DAVE takes it.
    fn make(&mut self, maker: Address, m: u64, qty: i64, price: u64, flags: u8) -> Vec<Log> {
        let (maker_side, taker_side) = if qty > 0 { (BUY, SELL) } else { (SELL, BUY) };
        self.ok(maker, &order(m, maker_side, price, qty.unsigned_abs(), LIMIT, GTC, flags));
        let logs = self.ok(DAVE, &mkt(m, taker_side, qty.unsigned_abs()));
        let t = evs::<Trade>(&logs);
        assert_eq!(t.iter().map(|t| t.quantity).sum::<u64>(), qty.unsigned_abs(), "made");
        logs
    }

    /// The oracle publishes `index` for `m` one interval after its last publication (the clock
    /// moving with it). The last traded price is cleared first, so the mark is the index.
    fn tick(&mut self, m: u64, index: u64) -> Vec<Log> {
        storage::save_last_traded_price(&mut self.h, m, 0).unwrap();
        let interval = self.spec(m).interval;
        let last = storage::load_index_price_state(&mut self.h, m).unwrap().timestamp;
        let ts = (last + interval).max(self.now() - self.now() % interval);
        if ts > self.now() {
            self.set_now(ts);
        }
        let logs = self.ok(
            ADMIN,
            &updateIndexPriceCall { marketId: m, indexPrice: index, timestamp: ts }.abi_encode(),
        );
        assert_eq!(self.mark(m), index, "the tick set the mark");
        logs
    }

    fn legs_of(&mut self, u: Address) -> Vec<(u64, i64)> {
        self.stored_legs(u).iter().map(|l| (l.market_id, l.amount)).collect()
    }

    fn delta_has_legs_key(&mut self, u: Address) -> bool {
        // Peek the live store's dirty set through a delta of a CLONE — the real block is not ended.
        use context_interface::journaled_state::PerpStore;
        let mut probe = self.h.live.clone();
        PerpStore::take_delta(&mut probe).contains_key(&perp_core::keys::cross_legs_key(u))
    }
}

/// Five whole-dollar markets at $1,000 (M1–M4 at 5 % maintenance, M5 on three derived brackets).
/// ALICE, BOB and CAROL hold $1e6 at the default margin type (CROSSED), leverage 10.
const M1: u64 = 1;
const M2: u64 = 2;
const M3: u64 = 3;
const M4: u64 = 4;
const M5: u64 = 5;

fn std_world() -> World {
    let mut specs: Vec<Spec> =
        [M1, M2, M3, M4].iter().map(|&m| Spec::whole(m, 1_000, &T10)).collect();
    specs.push(Spec::whole(M5, 1_000, &T3));
    let mut w = World::new(specs);
    for u in [ALICE, BOB, CAROL] {
        w.enroll(u, d(1_000_000));
        for m in w.markets() {
            w.set_lev(u, m, 10);
        }
    }
    w
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Routes
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// **Fills on either side**: a taker opens, increases, and partially closes; a maker fill reduces;
/// a taker flips the position through zero; a cross taker closes it against a cross maker who
/// opens; a FOK self-trade. Every step, both accounts' legs are the model's, and across two block
/// ends the committed legs too — including the delete when the last leg goes.
#[test]
fn route_fills_open_increase_reduce_flip_close() {
    let mut w = std_world();
    w.fill(ALICE, M1, 4, 1_000);
    assert_eq!(w.legs_of(ALICE), vec![(M1, 4)]);
    w.fill(ALICE, M1, 3, 1_010);
    w.fill(ALICE, M2, -2, 990);
    assert_eq!(w.legs_of(ALICE), vec![(M1, 7), (M2, -2)]);
    w.end_block();
    // A maker fill that reduces, then a partial taker close.
    w.make(ALICE, M1, -2, 1_005, 0);
    w.fill(ALICE, M1, -1, 995);
    assert_eq!(w.legs_of(ALICE), vec![(M1, 4), (M2, -2)]);
    // A flip through zero in one taker order.
    w.fill(ALICE, M1, -9, 1_000);
    assert_eq!(w.legs_of(ALICE), vec![(M1, -5), (M2, -2)]);
    // A cross taker closes against a cross maker who opens.
    w.rest(BOB, M1, SELL, 1_002, 5);
    w.ok(ALICE, &mkt(M1, BUY, 5));
    assert_eq!(w.legs_of(ALICE), vec![(M2, -2)]);
    assert_eq!(w.legs_of(BOB), vec![(M1, -5)]);
    // A FOK self-trade: both legs are ALICE's, the position nets to where it was.
    w.rest(ALICE, M2, SELL, 1_000, 1);
    let logs = w.ok(ALICE, &order(M2, BUY, 1_000, 1, LIMIT, FOK, 0));
    assert_eq!(evs::<Trade>(&logs).len(), 1, "the FOK traded with its owner");
    assert_eq!(w.legs_of(ALICE), vec![(M2, -2)]);
    w.end_block();
    w.fill(ALICE, M2, 2, 1_000);
    assert!(w.stored_legs(ALICE).is_empty());
    w.end_block();
    assert!(w.reached.legs_emptied > 0);
}

/// **Funding**: a pending payment is priced by the health check while the anchor stays; a
/// settlement on a touch moves the anchor; and a cancel-only participant of a match — its own
/// resting order expired by self-trade prevention, no fill — whose payment truncates to ZERO is
/// re-anchored on the silent route, which must carry the new anchor into its leg.
#[test]
fn route_funding_pending_settled_and_zero_payment_reanchor() {
    // A 3 / 2-decimal market (0.001 base, $0.01 price) at $100.00 with funding every interval.
    const F: u64 = 7;
    let mut fs = Spec::whole(F, 10_000, &T10);
    fs.bd = 3;
    fs.pd = 2;
    // 1 ppm a period: an index step of `mark · 1`, so 0.001 base pays `trunc(step / 10^5)` = 0.
    fs.funding = Some(1);
    let mut w = World::new(vec![fs, Spec::whole(M1, 1_000, &T10)]);
    for u in [ALICE, BOB] {
        w.enroll(u, d(1_000_000));
        w.set_lev(u, F, 10);
        w.set_lev(u, M1, 10);
    }
    // ALICE long 0.001 (a payment of `index step / 10^5` truncates to zero), BOB short 5.000.
    w.fill(ALICE, F, 1, 10_000);
    w.fill(BOB, F, -5_000, 10_000);
    w.fill(BOB, M1, 1, 1_000);
    let anchor0 = w.pos(ALICE, F).last_funding_index;
    // Funding epochs pass: the index moves, the legs do not (nothing touched them).
    for _ in 0..3 {
        w.tick(F, 10_100);
    }
    let index = storage::load_funding_state(&mut w.h, F).unwrap().cumulative_funding_index;
    assert_ne!(index, anchor0, "a funding epoch moved the index");
    assert_eq!(w.pos(BOB, F).last_funding_index, anchor0, "pending, not settled");
    let bob = w.model_health(BOB);
    assert!(bob.legs.iter().any(|l| l.market == F && l.pf != 0), "BOB's pending funding is priced");
    w.end_block();
    // BOB touches F: the settlement moves his anchor (and his wallet).
    let logs = w.fill(BOB, F, -1, 10_100);
    assert!(evs::<FundingSettled>(&logs).iter().any(|f| f.user == BOB && f.amount != 0));
    let leg = |w: &mut World, u| *w.stored_legs(u).iter().find(|l| l.market_id == F).unwrap();
    assert_eq!(leg(&mut w, BOB).last_funding_index, index);
    // ALICE: a resting sell, then her own market buy meets only it — EXPIRE_MAKER cancels it, the
    // remainder expires, nothing fills. Her zero payment re-anchors her on the silent route.
    let wallet = w.wallet(ALICE);
    w.rest(ALICE, F, SELL, 10_200, 1);
    let logs = w.ok(ALICE, &mkt(F, BUY, 1));
    assert!(evs::<Trade>(&logs).is_empty());
    assert_eq!(evs::<OrderCancelled>(&logs).len(), 1, "STP expired the maker");
    assert!(evs::<AccountBalanceChanged>(&logs).iter().all(|a| a.user != ALICE), "silent");
    assert_eq!(w.wallet(ALICE), wallet, "a zero payment");
    assert_eq!(w.pos(ALICE, F).last_funding_index, index, "re-anchored");
    assert_eq!(leg(&mut w, ALICE).last_funding_index, index, "…and so is the leg");
    w.end_block();
    assert!(w.reached.anchor_only > 0);
}

/// **setMarginType both ways on a flat market**: an ISOLATED position has no leg; switched to
/// CROSSED while flat, the next one has one; the switch is refused while a position or an order
/// is open (legs untouched); switched back, the next position again has none.
#[test]
fn route_set_margin_type_both_ways_on_a_flat_market() {
    let mut w = std_world();
    let switch = |t| setMarginTypeCall { marketId: M2, marginType: t }.abi_encode();
    w.fill(ALICE, M1, 3, 1_000);
    w.set_type(ALICE, M2, ISOLATED);
    w.fill(ALICE, M2, 2, 1_000);
    assert_eq!(w.legs_of(ALICE), vec![(M1, 3)], "the isolated position has no leg");
    w.rejected(ALICE, &switch(CROSSED), "-4048");
    w.fill(ALICE, M2, -2, 1_000);
    w.set_type(ALICE, M2, CROSSED);
    w.end_block();
    w.fill(ALICE, M2, -4, 1_000);
    assert_eq!(w.legs_of(ALICE), vec![(M1, 3), (M2, -4)]);
    w.rejected(ALICE, &switch(ISOLATED), "-4048");
    w.fill(ALICE, M2, 4, 1_000);
    let bid = w.rest(ALICE, M2, BUY, 900, 1);
    w.rejected(ALICE, &switch(ISOLATED), "-4047");
    w.ok(ALICE, &cancelOrderCall { orderId: bid.into(), marketId: M2 }.abi_encode());
    w.set_type(ALICE, M2, ISOLATED);
    w.fill(ALICE, M2, 5, 1_000);
    assert_eq!(w.legs_of(ALICE), vec![(M1, 3)]);
    w.end_block();
}

/// **Isolated margin next to cross legs**: add / remove isolated margin on a mixed account moves
/// no leg; the same on a CROSSED position is refused (-4049), and an isolated remove past what
/// the silo can spare (-4051): legs untouched.
#[test]
fn route_isolated_margin_add_remove_on_a_mixed_account() {
    let mut w = std_world();
    w.set_type(ALICE, M3, ISOLATED);
    w.fill(ALICE, M1, 5, 1_000);
    w.fill(ALICE, M2, -3, 1_000);
    w.fill(ALICE, M3, 4, 1_000);
    let add = |m, a| addPositionMarginCall { marketId: m, amount: a }.abi_encode();
    let rem = |m, a| removePositionMarginCall { marketId: m, amount: a }.abi_encode();
    w.ok(ALICE, &add(M3, d(100) as u64));
    w.ok(ALICE, &rem(M3, d(50) as u64));
    w.rejected(ALICE, &add(M1, d(100) as u64), "-4049");
    w.rejected(ALICE, &rem(M1, d(1) as u64), "-4049");
    w.rejected(ALICE, &rem(M3, d(10_000) as u64), "-4051");
    assert_eq!(w.legs_of(ALICE), vec![(M1, 5), (M2, -3)]);
    w.end_block();
}

/// **Fired conditionals**: a closePosition stop, a reduce-only stop for part of a position and a
/// trailing stop, each fired by an oracle tick into resting liquidity — the legs follow.
#[test]
fn route_fired_close_position_reduce_only_and_trailing_stops() {
    let mut w = std_world();
    w.fill(ALICE, M1, 5, 1_000);
    w.fill(ALICE, M2, 6, 1_000);
    w.fill(ALICE, M3, -4, 1_000);
    w.ok(ALICE, &stop_market(M1, SELL, 950, 0, CLOSE_POSITION));
    w.ok(ALICE, &stop_market(M2, SELL, 960, 2, RO));
    w.ok(ALICE, &trailing(M3, BUY, 4, 200));
    w.end_block();
    for (m, side, price, qty) in [(M1, BUY, 940, 5), (M2, BUY, 955, 2), (M3, SELL, 1_100, 4)] {
        w.rest(DAVE, m, side, price, qty);
    }
    let logs = w.tick(M1, 945);
    assert_eq!(evs::<AlgoOrderTriggered>(&logs).len(), 1);
    assert_eq!(w.legs_of(ALICE), vec![(M2, 6), (M3, -4)]);
    w.tick(M2, 958);
    assert_eq!(w.legs_of(ALICE), vec![(M2, 4), (M3, -4)]);
    // The trailing stop on the short: the low is 900, a 2 % bounce off it fires.
    w.tick(M3, 900);
    assert_eq!(w.legs_of(ALICE), vec![(M2, 4), (M3, -4)], "not yet");
    let logs = w.tick(M3, 920);
    assert_eq!(evs::<AlgoOrderTriggered>(&logs).len(), 1, "the trailing stop fired");
    w.end_block();
    assert_eq!(w.legs_of(ALICE), vec![(M2, 4)]);
}

/// **Sub-accounts**: a sub funded by `internalTransfer` trades through `asSubAccount` — a market
/// order, a batch, a mode switch and an isolated open — against its master and DAVE; both
/// families' legs follow.
#[test]
fn route_sub_accounts_through_as_sub_account() {
    let mut w = std_world();
    w.ok(ALICE, &createSubAccountCall { name: "s1".into() }.abi_encode());
    let sub = createSubAccountCall::abi_decode_returns(&w.last_out).unwrap();
    w.enroll(sub, 0);
    let amount = d(50_000) as u64;
    let fund_sub = internalTransferCall { to: sub, fromWallet: PERP, toWallet: PERP, amount };
    w.ok(ALICE, &fund_sub.abi_encode());
    w.ok(ALICE, &as_sub(sub, setLeverageCall { marketId: M1, leverage: 10 }.abi_encode()));
    w.ok(ALICE, &as_sub(sub, setLeverageCall { marketId: M2, leverage: 10 }.abi_encode()));
    // The master rests, the sub takes: a sub is a different address, so they trade.
    w.rest(ALICE, M1, SELL, 1_000, 3);
    w.ok(ALICE, &as_sub(sub, mkt(M1, BUY, 3)));
    assert_eq!(w.legs_of(sub), vec![(M1, 3)]);
    assert_eq!(w.legs_of(ALICE), vec![(M1, -3)]);
    w.end_block();
    // A batch through the sub: an open on M2, a resting bid, a reduce on M1.
    w.rest(DAVE, M2, SELL, 1_000, 2);
    w.rest(DAVE, M1, BUY, 1_000, 1);
    let batch = batchPlaceOrdersCall {
        orders: vec![
            item(M2, BUY, 0, 2, MKT, 0),
            item(M2, BUY, 900, 1, LIMIT, 0),
            item(M1, SELL, 0, 1, MKT, RO),
        ],
    };
    w.ok(ALICE, &as_sub(sub, batch.abi_encode()));
    assert_eq!(w.legs_of(sub), vec![(M1, 2), (M2, 2)]);
    // An isolated market for the sub.
    let switch = setMarginTypeCall { marketId: M3, marginType: ISOLATED }.abi_encode();
    w.ok(ALICE, &as_sub(sub, switch));
    w.rest(DAVE, M3, SELL, 1_000, 1);
    w.ok(ALICE, &as_sub(sub, mkt(M3, BUY, 1)));
    assert_eq!(w.legs_of(sub), vec![(M1, 2), (M2, 2)]);
    w.end_block();
}

/// **Batches**: one batch opens on three markets, rests, reduces and has an item refused; the legs
/// are the model's after the whole batch.
#[test]
fn route_batch_place_orders() {
    let mut w = std_world();
    w.fill(ALICE, M4, 2, 1_000);
    w.rest(DAVE, M1, SELL, 1_000, 3);
    w.rest(DAVE, M2, BUY, 1_000, 2);
    w.rest(DAVE, M4, BUY, 1_000, 1);
    let batch = batchPlaceOrdersCall {
        orders: vec![
            item(M1, BUY, 0, 3, MKT, 0),
            item(M2, SELL, 0, 2, MKT, 0),
            item(M3, BUY, 900, 1, LIMIT, 0),
            // Reduce-only on a flat market: refused per item.
            item(M5, SELL, 0, 1, MKT, RO),
            item(M4, SELL, 0, 1, MKT, RO),
        ],
    };
    w.ok(ALICE, &batch.abi_encode());
    assert_eq!(w.legs_of(ALICE), vec![(M1, 3), (M2, -2), (M4, 1)]);
    w.end_block();
}

/// **The cross liquidation episode and ADL**: ALICE, cross on two markets, crashes on M1. The
/// episode closes M1 into BOB's bids and ADLs the rest against CAROL (cross winner); M2 follows
/// if needed. Then an ISOLATED loser's remainder is ADL'd against a cross winner, and a cross
/// loser's against an ISOLATED winner. Every account's legs follow, every call.
#[test]
fn route_cross_episode_closes_and_adl_both_sides() {
    let mut w = std_world();
    w.enroll(user(1), 0);
    w.fund(user(1), d(1_000_000));
    let erin = user(1);
    w.set_lev(erin, M1, 10);
    w.set_lev(erin, M2, 10);
    // ALICE: $2,000 of equity carrying 20 long on M1 and 10 short on M2.
    w.fill(ALICE, M1, 20, 1_000);
    w.fill(ALICE, M2, -10, 1_000);
    w.ok(ALICE, &transferFromPerpCall { amount: (d(1_000_000) - d(3_200)) as u64 }.abi_encode());
    // CAROL is the cross winner on M1 (short), ERIN an isolated short.
    w.fill(CAROL, M1, -12, 1_000);
    w.set_type(erin, M1, ISOLATED);
    w.fill(erin, M1, -8, 1_000);
    w.rest(BOB, M1, BUY, 880, 6);
    w.end_block();
    let logs = w.tick(M1, 900);
    assert!(!evs::<CrossLiquidationStarted>(&logs).is_empty(), "ALICE's episode");
    assert!(!evs::<Adl>(&logs).is_empty(), "ADL took the remainder");
    w.end_block();
    // An ISOLATED loser (CAROL's M3 long, isolated) → ADL against BOB's cross short.
    w.set_type(CAROL, M3, ISOLATED);
    w.fill(CAROL, M3, 10, 1_000);
    w.fill(BOB, M3, -10, 1_000);
    let logs = w.tick(M3, 880);
    let adl = evs::<Adl>(&logs);
    assert!(adl.iter().any(|a| a.liquidatedUser == CAROL && a.adlUser == BOB));
    // A cross loser (BOB's M4 long) → ADL against ERIN's isolated short.
    w.set_type(erin, M4, ISOLATED);
    w.fill(erin, M4, -10, 1_000);
    w.fill(BOB, M4, 10, 1_000);
    let x = w.account_view(BOB).maxWithdrawAmount;
    w.ok(BOB, &transferFromPerpCall { amount: x }.abi_encode());
    let logs = w.tick(M4, 700);
    assert!(evs::<Adl>(&logs).iter().any(|a| a.liquidatedUser == BOB && a.adlUser == erin));
    w.end_block();
    assert!(w.reached.cross_closes > 0 && w.reached.iso_closes > 0);
    assert!(w.reached.adl_cross_winner > 0);
}

/// **The negative-wallet clearance (Q8)**: ALICE's only position, CROSSED, is closed as a maker
/// far through the mark; the loss takes her wallet below zero with no position left, and the
/// insurance fund clears it at the end of the call. Her legs key is gone (committed absent).
#[test]
fn route_negative_wallet_clearance_deletes_the_legs() {
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    let x = w.account_view(ALICE).maxWithdrawAmount;
    w.ok(ALICE, &transferFromPerpCall { amount: x }.abi_encode());
    assert!(w.wallet(ALICE) < d(2_000), "the maker loss below takes more than the wallet");
    w.end_block();
    assert_eq!(w.legs_of(ALICE), vec![(M1, 10)]);
    let logs = w.make(ALICE, M1, -10, 800, RO);
    let clear = evs::<AccountBalanceChanged>(&logs);
    assert!(clear.iter().any(|a| a.user == ALICE && a.reason == INSURANCE_CLEAR), "cleared");
    assert!(w.stored_legs(ALICE).is_empty());
    w.end_block();
    assert!(w.reached.clearances > 0);
}

/// **`liquidate()` and the drain**: an account put under cross maintenance by a maker loss (no
/// tick) is liquidated by a keeper; a crowd that exceeds one sweep's step budget is finished by
/// `drainDeferredWork`. Every account's legs follow through both.
#[test]
fn route_keeper_liquidate_and_drain() {
    let mut w = std_world();
    // ALICE: long 10 on M1 and M2, then an M2 maker loss with no tick.
    w.fill(ALICE, M1, 10, 1_000);
    w.fill(ALICE, M2, 10, 1_000);
    let x = w.account_view(ALICE).maxWithdrawAmount;
    w.ok(ALICE, &transferFromPerpCall { amount: x }.abi_encode());
    assert_eq!(w.wallet(ALICE), d(2_000), "PIM = $2,000 at leverage 10");
    w.make(ALICE, M2, -10, 840, RO);
    w.rest(BOB, M1, BUY, 1_000, 10);
    let logs = w.ok(KEEPER, &liquidateCall { user: ALICE, marketId: M1 }.abi_encode());
    assert!(!evs::<CrossLiquidationStarted>(&logs).is_empty());
    w.end_block();
    // A crowd of 60 cross longs on M3, each near its leverage.
    let crowd: Vec<Address> = (100..160).map(user).collect();
    for &u in &crowd {
        w.enroll(u, d(1_000));
        w.set_lev(u, M3, 10);
        w.fill(u, M3, 9, 1_000);
    }
    w.end_block();
    w.rest(BOB, M3, BUY, 850, 600);
    w.tick(M3, 900);
    let mut drains = 0;
    while w.deferred(M3) {
        drains += 1;
        assert!(drains < 20);
        w.ok(KEEPER, &drainDeferredWorkCall { marketId: M3 }.abi_encode());
    }
    assert!(drains > 0, "the sweep left work for the drain");
    w.end_block();
}

/// **Sixteen markets, a mixed account**: ALICE holds twelve CROSSED and four ISOLATED positions,
/// opened in a shuffled order (the list stays sorted); closes and re-opens move single legs; a
/// crash on one market runs an episode over all twelve legs.
#[test]
fn route_sixteen_markets_mixed_account() {
    let specs: Vec<Spec> = (1..=16).map(|m| Spec::whole(m, 1_000, &T10)).collect();
    let mut w = World::new(specs);
    w.enroll(ALICE, d(40_000));
    w.enroll(BOB, d(10_000_000));
    for m in 1..=16 {
        w.set_lev(ALICE, m, 10);
        w.set_lev(BOB, m, 10);
        if m % 4 == 0 {
            w.set_type(ALICE, m, ISOLATED);
        }
    }
    let order = [9u64, 3, 16, 1, 12, 7, 14, 2, 5, 11, 8, 15, 4, 10, 6, 13];
    for (i, &m) in order.iter().enumerate() {
        let q = if i % 2 == 0 { 3 } else { -3 };
        w.fill(ALICE, m, q, 1_000);
        w.fill(BOB, m, -q, 1_000);
    }
    assert_eq!(w.stored_legs(ALICE).len(), 12);
    w.end_block();
    w.fill(ALICE, 7, 3, 1_000);
    w.fill(ALICE, 1, 1, 1_000);
    w.fill(ALICE, 7, -2, 1_000);
    assert_eq!(w.stored_legs(ALICE).len(), 12);
    w.end_block();
    // Thin her out, then crash market 9 (a long of 3).
    let x = w.account_view(ALICE).maxWithdrawAmount;
    w.ok(ALICE, &transferFromPerpCall { amount: x }.abi_encode());
    for m in 1..=16 {
        w.rest(BOB, m, BUY, 600, 10);
        w.rest(BOB, m, SELL, 1_400, 10);
    }
    let logs = w.tick(9, 400);
    assert!(!evs::<CrossLiquidationStarted>(&logs).is_empty());
    w.end_block();
    assert_eq!(w.reached.max_legs, 16, "BOB: sixteen cross legs");
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Refusals and placements
// ═════════════════════════════════════════════════════════════════════════════════════════════

/// **A refused call leaves every leg untouched** (the harness checks the legs, the write count and
/// the logs of every refusal): the taker margin refusal, the available-balance rest refusal, a
/// reduce-only that would increase, a closePosition stop without a position, money out past X,
/// `liquidate()` of a healthy account, an invalid STP mode — and, on an account an episode left
/// FLAGGED under maintenance (its other leg's mark stale, Q10), the -2023 freeze and the mode
/// switch.
#[test]
fn refused_calls_leave_the_legs_untouched() {
    let mut w = std_world();
    w.fill(ALICE, M1, 10, 1_000);
    w.fill(ALICE, M2, 10, 1_000);
    let x = w.account_view(ALICE).maxWithdrawAmount;
    w.ok(ALICE, &transferFromPerpCall { amount: x }.abi_encode());
    assert_eq!(w.wallet(ALICE), d(2_000), "PIM = $2,000 at leverage 10");
    w.rest(DAVE, M3, SELL, 1_000, 1_000);
    w.rejected(ALICE, &mkt(M3, BUY, 1_000), "-2019");
    w.rejected(ALICE, &gtc(M4, BUY, 1_000, 500), "");
    w.rejected(ALICE, &order(M1, BUY, 1_000, 1, LIMIT, GTC, RO), "");
    w.rejected(ALICE, &stop_market(M4, SELL, 900, 0, CLOSE_POSITION), "closePosition");
    w.rejected(ALICE, &transferFromPerpCall { amount: d(1) as u64 }.abi_encode(), "");
    w.rejected(KEEPER, &liquidateCall { user: ALICE, marketId: M1 }.abi_encode(), "liquidate");
    w.rejected(ALICE, &order(M1, SELL, 1_100, 1, LIMIT, GTC, 0b1100), "self-trade prevention");
    w.end_block();
    // M2's oracle goes quiet past 3 intervals; M1 crashes 16 %: E = $400 < ΣMM = $920. The episode
    // closes M1 (ADL at the mark: E ≥ 0) and is left at E = $400 < MM_M2 = $500 with M2 stale.
    let later = w.now() + 100;
    w.set_now(later);
    let logs = w.tick(M1, 840);
    assert!(!evs::<CrossLiquidationStarted>(&logs).is_empty());
    assert!(evs::<CrossLiquidationEnded>(&logs).is_empty(), "the episode is left open");
    assert!(w.flagged(ALICE));
    assert_eq!(w.legs_of(ALICE), vec![(M2, 10)]);
    w.rejected(ALICE, &gtc(M3, BUY, 900, 1), "-2023");
    let switch = setMarginTypeCall { marketId: M4, marginType: ISOLATED }.abi_encode();
    w.rejected(ALICE, &switch, "-2023");
    w.end_block();
    assert!(w.reached.refused_with_legs >= 9);
}

/// **A placement and a cancel write no legs key** (D2's reason for its own blob): a block holding
/// only ALICE's order placements and cancels on markets where she holds cross legs commits no
/// change to her legs key.
#[test]
fn placement_and_cancel_write_no_legs_key() {
    let mut w = std_world();
    w.fill(ALICE, M1, 5, 1_000);
    w.fill(ALICE, M2, -5, 1_000);
    w.end_block();
    let a = w.rest(ALICE, M1, BUY, 900, 2);
    let b = w.rest(ALICE, M2, SELL, 1_100, 2);
    w.rest(ALICE, M3, BUY, 900, 2);
    w.ok(ALICE, &cancelOrderCall { orderId: a.into(), marketId: M1 }.abi_encode());
    w.ok(ALICE, &cancelOrderCall { orderId: b.into(), marketId: M2 }.abi_encode());
    w.ok(ALICE, &stop_market(M1, SELL, 900, 0, CLOSE_POSITION));
    assert!(!w.delta_has_legs_key(ALICE), "a placement or a cancel dirtied the legs key");
    // A fill does.
    w.fill(ALICE, M1, 1, 1_000);
    assert!(w.delta_has_legs_key(ALICE));
    w.end_block();
}

// ═════════════════════════════════════════════════════════════════════════════════════════════
// Randomised runs
// ═════════════════════════════════════════════════════════════════════════════════════════════

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

/// The fuzz's market table: whole-dollar markets of every tier shape, bands on and off, clearance
/// fees, funding of either sign — and every fifth market a 3 / 2-decimal one, so that `N`, `PF`
/// and the slice form truncate.
fn fuzz_specs(n: u64) -> Vec<Spec> {
    let tiers: [&[Band]; 4] = [&T3, &T10, &TB, &[Band { lb: 0, lev: 5, ppm: 0 }]];
    let marks = [1_000, 500, 200, 100];
    let bands = [10_000, 500, 1_000, 300];
    let fees = [125, 0, 500, 0];
    let funding = [Some(300), Some(-200), None, Some(100)];
    (0..n)
        .map(|i| {
            let k = (i % 4) as usize;
            let mut s = Spec {
                id: i + 1,
                bd: 0,
                pd: 0,
                mark0: marks[k],
                tiers: tiers[k].to_vec(),
                interval: 15,
                band_bps: bands[k],
                fee_bps: fees[k],
                funding: funding[k],
            };
            if i % 5 == 4 {
                s.bd = 3;
                s.pd = 2;
                s.mark0 *= 100;
            }
            s
        })
        .collect()
}

struct Fuzz {
    w: World,
    rng: Rng,
    traders: Vec<Address>,
    /// Deep liquidity: DAVE (ISOLATED, leverage 1) and LP2 (CROSSED, $1e9).
    lps: [Address; 2],
    index: Vec<u64>,
    /// `(master, sub)` pairs.
    subs: Vec<(Address, Address)>,
}

impl Fuzz {
    fn new(seed: u64, n_markets: u64, n_traders: u32) -> Fuzz {
        let mut rng = Rng(seed);
        let specs = fuzz_specs(n_markets);
        let mut w = World::new(specs.clone());
        let lp2 = user(9_999);
        w.enroll(lp2, BIG);
        for s in &specs {
            w.set_lev(lp2, s.id, 1);
        }
        let mut traders = Vec::new();
        let mut subs = Vec::new();
        for i in 0..n_traders {
            let u = user(1_000 + i);
            w.enroll(u, d(500 + rng.below(20_000) as i128));
            if i % 3 == 0 {
                let fees = setUserFeeRatesCall { user: u, makerFeePpm: 200, takerFeePpm: 500 };
                w.ok(ADMIN, &fees.abi_encode());
            }
            for s in &specs {
                if rng.chance(1, 3) {
                    w.set_type(u, s.id, ISOLATED);
                }
                let cap = s.tiers[0].lev as u64;
                let lev = if rng.chance(1, 2) { cap } else { 1 + rng.below(cap) };
                w.set_lev(u, s.id, lev);
            }
            if i % 4 == 0 {
                w.ok(u, &createSubAccountCall { name: "f".into() }.abi_encode());
                let sub = createSubAccountCall::abi_decode_returns(&w.last_out).unwrap();
                w.enroll(sub, 0);
                let amount = d(1_000 + rng.below(5_000) as i128) as u64;
                w.fund(u, amount as i128);
                let to_sub =
                    internalTransferCall { to: sub, fromWallet: PERP, toWallet: PERP, amount };
                w.ok(u, &to_sub.abi_encode());
                for s in &specs {
                    if rng.chance(1, 3) {
                        let iso = setMarginTypeCall { marketId: s.id, marginType: ISOLATED };
                        w.ok(u, &as_sub(sub, iso.abi_encode()));
                    }
                    let lev = 1 + rng.below(s.tiers[0].lev as u64);
                    let set = setLeverageCall { marketId: s.id, leverage: lev };
                    w.ok(u, &as_sub(sub, set.abi_encode()));
                }
                subs.push((u, sub));
                traders.push(sub);
            }
            traders.push(u);
        }
        let index = specs.iter().map(|s| s.mark0).collect();
        Fuzz { w, rng, traders, lps: [DAVE, lp2], index, subs }
    }

    /// `u`'s call, through `asSubAccount` when `u` is a sub.
    fn call(&mut self, u: Address, input: Vec<u8>) -> Result<Vec<Log>, String> {
        match self.subs.iter().find(|(_, s)| *s == u) {
            Some(&(master, sub)) => self.w.attempt(master, &as_sub(sub, input)),
            None => self.w.attempt(u, &input),
        }
    }

    fn tick(&mut self, m: u64, index: u64) {
        let k = (m - 1) as usize;
        let s = self.w.spec(m).clone();
        let max = 1_000_000 * 10u64.pow(s.pd);
        let index = index.clamp(1, max);
        storage::save_last_traded_price(&mut self.w.h, m, 0).unwrap();
        let last = storage::load_index_price_state(&mut self.w.h, m).unwrap().timestamp;
        let ts = (last + s.interval).max(self.w.now() - self.w.now() % s.interval);
        if ts > self.w.now() {
            self.w.set_now(ts);
        }
        let input =
            updateIndexPriceCall { marketId: m, indexPrice: index, timestamp: ts }.abi_encode();
        self.w.attempt(ADMIN, &input).unwrap_or_else(|e| panic!("the oracle reverted: {e}"));
        self.index[k] = index;
    }

    fn cancel_all(&mut self, u: Address, m: u64) {
        let ids: Vec<Id> = storage::load_buy_orders(&mut self.w.h, u, m)
            .unwrap()
            .iter()
            .chain(storage::load_sell_orders(&mut self.w.h, u, m).unwrap().iter())
            .map(|e| e.order_id)
            .collect();
        for id in ids {
            let _ = self.call(u, cancelOrderCall { orderId: id.into(), marketId: m }.abi_encode());
        }
    }

    /// A unit of size worth about `usd` dollars at the mark.
    fn qty_for(&mut self, m: u64, usd: u64) -> u64 {
        let s = self.w.spec(m).clone();
        let mark = self.w.mark(m).max(1) as u128;
        let q = usd as u128 * 10u128.pow(s.bd + s.pd) / mark;
        (q as u64).max(1)
    }

    /// A random size from 1 up to about `usd` dollars at the mark.
    fn rand_qty(&mut self, m: u64, usd: u64) -> u64 {
        let cap = self.qty_for(m, usd);
        1 + self.rng.below(cap)
    }

    fn step(&mut self) {
        let dt = 1 + self.rng.below(3);
        let now = self.w.now() + dt;
        self.w.set_now(now);
        let markets = self.w.markets();
        let u = self.rng.pick(&self.traders.clone());
        let m = self.rng.pick(&markets);
        let mk = self.w.mark(m);
        let lp = self.rng.pick(&self.lps);
        match self.rng.below(40) {
            0..=7 => {
                // A market order against liquidity a provider rests within 1 % of the mark.
                let usd = self.rng.pick(&[2_000u64, 10_000, 40_000, 100_000]);
                let qty = self.rand_qty(m, usd);
                let side = self.rng.pick(&[BUY, SELL]);
                let off = self.rng.below(mk / 100 + 1);
                let price = if side == BUY { mk + off } else { mk.saturating_sub(off).max(1) };
                let lq = self.w.attempt(lp, &gtc(m, 1 - side, price, qty)).is_ok();
                let _ = self.call(u, mkt(m, side, qty));
                if lq {
                    self.cancel_all(lp, m);
                }
            }
            8..=9 => {
                // A limit away from the mark.
                let side = self.rng.pick(&[BUY, SELL]);
                let off = 1 + self.rng.below(mk / 10 + 1);
                let price = if side == BUY { mk.saturating_sub(off).max(1) } else { mk + off };
                let qty = self.rand_qty(m, 10_000);
                let flags = if self.rng.chance(1, 4) { RO } else { 0 };
                let _ = self.call(u, order(m, side, price, qty, LIMIT, GTC, flags));
            }
            10 => {
                // A provider takes whatever rests (the traders' maker fills).
                let qty = self.rand_qty(m, 20_000);
                let side = self.rng.pick(&[BUY, SELL]);
                let _ = self.w.attempt(lp, &mkt(m, side, qty));
            }
            11 => {
                // A reduce-only close of the whole position, as taker or as maker.
                let a = self.w.pos(u, m).amount;
                if a != 0 {
                    let side = if a > 0 { SELL } else { BUY };
                    let q = a.unsigned_abs();
                    if self.rng.chance(1, 2) {
                        let lq = self.w.attempt(lp, &gtc(m, 1 - side, mk, q)).is_ok();
                        let _ = self.call(u, order(m, side, 0, q, MKT, GTC, RO));
                        if lq {
                            self.cancel_all(lp, m);
                        }
                    } else if self.call(u, order(m, side, mk, q, LIMIT, GTC, RO)).is_ok() {
                        let _ = self.w.attempt(lp, &mkt(m, 1 - side, q));
                    }
                }
            }
            12 => self.cancel_all(u, m),
            13 => {
                let cap = self.w.spec(m).tiers[0].lev as u64;
                let lev = 1 + self.rng.below(cap);
                let _ = self.call(u, setLeverageCall { marketId: m, leverage: lev }.abi_encode());
            }
            14 => {
                // Money out at X or a little past it; money in.
                let x = self.w.account_view(u).maxWithdrawAmount;
                let amount = if self.rng.chance(1, 2) { x.max(1) } else { x + 1 };
                let _ = self.call(u, transferFromPerpCall { amount }.abi_encode());
                if self.rng.chance(1, 2) && !self.subs.iter().any(|(_, s)| *s == u) {
                    let amount = d(1 + self.rng.below(5_000) as i128);
                    self.w.fund(u, amount);
                }
            }
            15 => {
                let t = if self.w.pos(u, m).margin_type == CROSSED { ISOLATED } else { CROSSED };
                let _ = self.call(u, setMarginTypeCall { marketId: m, marginType: t }.abi_encode());
            }
            16 => {
                // Isolated margin in and out.
                let amount = d(1 + self.rng.below(500) as i128) as u64;
                let input = if self.rng.chance(1, 2) {
                    addPositionMarginCall { marketId: m, amount }.abi_encode()
                } else {
                    removePositionMarginCall { marketId: m, amount }.abi_encode()
                };
                let _ = self.call(u, input);
            }
            17 => {
                // A protective conditional: a closePosition stop, a reduce-only stop or a trailing.
                let a = self.w.pos(u, m).amount;
                if a != 0 {
                    let side = if a > 0 { SELL } else { BUY };
                    let r = 1 + self.rng.below(10);
                    let trig = if a > 0 { mk - mk * r / 100 } else { mk + mk * r / 100 };
                    let input = match self.rng.below(3) {
                        0 => stop_market(m, side, trig.max(1), 0, CLOSE_POSITION),
                        1 => {
                            let q = 1 + self.rng.below(a.unsigned_abs());
                            stop_market(m, side, trig.max(1), q, RO)
                        }
                        _ => trailing(m, side, a.unsigned_abs(), 10 * (1 + self.rng.below(100))),
                    };
                    let _ = self.call(u, input);
                    // Liquidity for it to fire into, a little through the trigger.
                    let price = if side == SELL { trig * 98 / 100 } else { trig * 102 / 100 };
                    let _ = self.w.attempt(DAVE, &gtc(m, 1 - side, price.max(1), a.unsigned_abs()));
                }
            }
            18 => {
                // A batch on random markets.
                let mut items = Vec::new();
                for _ in 0..1 + self.rng.below(4) {
                    let bm = self.rng.pick(&markets);
                    let bmk = self.w.mark(bm);
                    let side = self.rng.pick(&[BUY, SELL]);
                    let q = self.rand_qty(bm, 5_000);
                    if self.rng.chance(1, 2) {
                        let lq = gtc(bm, 1 - side, bmk, q);
                        let _ = self.w.attempt(lp, &lq);
                        items.push(item(bm, side, 0, q, MKT, 0));
                    } else {
                        let off = 1 + self.rng.below(bmk / 20 + 1);
                        let p =
                            if side == BUY { bmk.saturating_sub(off).max(1) } else { bmk + off };
                        items.push(item(bm, side, p, q, LIMIT, 0));
                    }
                }
                let _ = self.call(u, batchPlaceOrdersCall { orders: items }.abi_encode());
                for bm in markets {
                    if !storage::load_buy_orders(&mut self.w.h, lp, bm).unwrap().is_empty()
                        || !storage::load_sell_orders(&mut self.w.h, lp, bm).unwrap().is_empty()
                    {
                        self.cancel_all(lp, bm);
                    }
                }
            }
            19 => {
                let v = self.rng.pick(&self.traders.clone());
                let input = liquidateCall { user: v, marketId: m }.abi_encode();
                let _ = self.w.attempt(KEEPER, &input);
            }
            20 => {
                let _ = self.w.attempt(KEEPER, &drainDeferredWorkCall { marketId: m }.abi_encode());
            }
            21 => {
                // A cross leg closed as a maker 15–80 % through the mark (a wallet goes negative).
                let legs = self.w.stored_legs(u);
                if let Some(l) = legs.first().copied() {
                    let lm = self.w.mark(l.market_id);
                    let r = 15 + self.rng.below(66);
                    let side = if l.amount > 0 { SELL } else { BUY };
                    let price =
                        if l.amount > 0 { (lm - lm * r / 100).max(1) } else { lm + lm * r / 100 };
                    self.cancel_all(u, l.market_id);
                    let q = l.amount.unsigned_abs();
                    if self.call(u, order(l.market_id, side, price, q, LIMIT, GTC, RO)).is_ok() {
                        let _ = self.w.attempt(lp, &mkt(l.market_id, 1 - side, q));
                    }
                }
            }
            22 => {
                // A sub and its master trade with each other (internal transfer first).
                let i = self.rng.below(self.subs.len() as u64) as usize;
                if let Some(&(master, sub)) = self.subs.get(i) {
                    let amount = d(1 + self.rng.below(2_000) as i128) as u64;
                    let t =
                        internalTransferCall { to: sub, fromWallet: PERP, toWallet: PERP, amount };
                    let _ = self.w.attempt(master, &t.abi_encode());
                    let q = self.rand_qty(m, 3_000);
                    let side = self.rng.pick(&[BUY, SELL]);
                    if self.w.attempt(master, &gtc(m, side, mk, q)).is_ok() {
                        let _ = self.w.attempt(master, &as_sub(sub, mkt(m, 1 - side, q)));
                        self.cancel_all(master, m);
                    }
                }
            }
            23 => {
                // End the block.
                self.w.end_block();
            }
            24 if self.rng.chance(1, 3) => {
                // A market-wide move, one market per tick.
                let down = self.rng.chance(3, 4);
                for m in markets {
                    let cur = self.index[(m - 1) as usize];
                    let delta = cur * (10 + self.rng.below(31)) / 100;
                    let next = if down { cur.saturating_sub(delta) } else { cur + delta };
                    self.tick(m, next);
                }
            }
            _ => {
                // An oracle tick: small moves mostly, gaps, crashes and spikes.
                let cur = self.index[(m - 1) as usize];
                let roll = self.rng.below(100);
                let bp: u64 = match roll {
                    0..=59 => 10 + self.rng.below(300),
                    60..=84 => 300 + self.rng.below(900),
                    _ => 1_500 + self.rng.below(3_500),
                };
                let up = if roll >= 85 { self.rng.chance(1, 4) } else { self.rng.chance(1, 2) };
                let delta = (cur as u128 * bp as u128 / 10_000) as u64;
                let next = if up { cur + delta } else { cur.saturating_sub(delta) };
                self.tick(m, next);
            }
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Runs `seeds` seeds from `base`, each wrapped so a failure names its seed; returns the summed
/// coverage.
fn fuzz_many(base: u64, seeds: u64, steps: u64, n_markets: u64, n_traders: u32) -> Reached {
    let mut total = Reached::default();
    let mut failed = Vec::new();
    for i in 0..seeds {
        let seed = base.wrapping_add(i.wrapping_mul(0x5851_f42d_4c95_7f2d));
        let r = std::panic::catch_unwind(|| {
            let mut f = Fuzz::new(seed, n_markets, n_traders);
            for _ in 0..steps {
                f.step();
            }
            f.w.end_block();
            f.w.reached
        });
        match r {
            Ok(r) => {
                total.calls += r.calls;
                total.refused += r.refused;
                total.blocks += r.blocks;
                total.legs_moved += r.legs_moved;
                total.legs_emptied += r.legs_emptied;
                total.anchor_only += r.anchor_only;
                total.max_legs = total.max_legs.max(r.max_legs);
                total.cross_closes += r.cross_closes;
                total.iso_closes += r.iso_closes;
                total.episodes += r.episodes;
                total.adl_fills += r.adl_fills;
                total.adl_cross_winner += r.adl_cross_winner;
                total.clearances += r.clearances;
                total.funding_settled += r.funding_settled;
                total.algo_fired += r.algo_fired;
                total.refused_with_legs += r.refused_with_legs;
            }
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
    eprintln!("cross legs fuzz {n_markets} markets, {seeds} seeds × {steps} steps: {total:#?}");
    let shown = &failed[..failed.len().min(3)];
    assert!(failed.is_empty(), "{} of {seeds} seeds failed: {shown:#?}", failed.len());
    total
}

/// **The legs and the health, every call, at random**: five markets (every tier shape, bands,
/// fees, funding of both signs, a 3 / 2-decimal market), 9 traders of random margin types per
/// market (three of them sub-accounts trading through `asSubAccount`), two liquidity providers,
/// and every route: market, limit, reduce-only and batch orders, maker fills, cancels, leverage,
/// money out and in, mode switches, isolated margin, conditionals and trailing stops fired by
/// ticks, keeper `liquidate()`, drains, deep maker losses, sub-account trades, oracle moves and
/// market-wide crashes, and block ends. Every call is held to the per-call check.
#[test]
fn fuzz_cross_legs_five_markets() {
    let seeds = env_u64("CROSS_LEGS_FUZZ_SEEDS", 4);
    let steps = env_u64("CROSS_LEGS_FUZZ_STEPS", 2_500);
    let base = env_u64("CROSS_LEGS_FUZZ_BASE", 0x0004_1e95_2026);
    let r = fuzz_many(base, seeds, steps, 5, 7);
    if seeds >= 4 && steps >= 2_500 {
        assert!(r.cross_closes > 0 && r.iso_closes > 0 && r.episodes > 0, "liquidations");
        assert!(r.adl_fills > 0 && r.adl_cross_winner > 0, "ADL with a cross winner");
        assert!(r.clearances > 0, "a negative wallet cleared");
        assert!(r.funding_settled > 0 && r.anchor_only > 0, "funding moved anchors");
        assert!(r.algo_fired > 0, "conditionals fired");
        assert!(r.legs_emptied > 0 && r.refused_with_legs > 0);
    }
}

/// The same at the per-user market cap: sixteen markets.
#[test]
fn fuzz_cross_legs_sixteen_markets() {
    let seeds = env_u64("CROSS_LEGS_FUZZ16_SEEDS", 2);
    let steps = env_u64("CROSS_LEGS_FUZZ16_STEPS", 2_000);
    let base = env_u64("CROSS_LEGS_FUZZ_BASE", 0x0016_1e95_2026);
    let r = fuzz_many(base, seeds, steps, 16, 6);
    if steps >= 2_000 {
        assert!(r.max_legs >= 6, "accounts held many cross legs: {}", r.max_legs);
        assert!(r.cross_closes > 0 && r.adl_fills > 0, "liquidations and ADL");
    }
}
