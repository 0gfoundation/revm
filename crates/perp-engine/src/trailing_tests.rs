//! Tests for the trailing-stop structure.
//!
//! * the predicate and its derived values ([`fires`] / [`thr`] / [`display_stop`]);
//! * [`check_structure`] — the full O(N) invariant checker (I1–I5, garbage-page accounting, no
//!   orphan key), run after every step of the differential tests;
//! * [`Model`] — the simple O(N) per-order reference: every activated order carries its own peak,
//!   updated on every tick, and every sweep scans every order. It is the "simple-capped" design,
//!   kept here as the oracle;
//! * two differential property tests against it — one driving the sweep legs directly with RANDOM
//!   budgets and caps (overflow, partial drains, skipped drains, late activations), one through
//!   the real `updateIndexPrice` / `drainDeferredWork` path — asserting the per-tick fire set
//!   (in order, with the reported stop), the activation set, the deferral bits and every live
//!   order's peak and stop agree exactly;
//! * the decline-then-spike worst case, with storage-operation counts;
//! * scenarios through the real oracle path.

use super::*;
use crate::host::InMemoryHost;
use crate::interface::IPerpDex::{
    cancelAlgoOrderCall, drainDeferredWorkCall, getAlgoOrderCall, getOpenAlgoOrdersCall,
    placeAlgoOrderCall, placeAlgoOrderSignedCall, updateIndexPriceCall, AlgoOrderActivated,
    AlgoOrderCancelled, AlgoOrderPlaced, AlgoOrderTriggered,
};
use crate::types::{
    AccountUpdateReason, AlgoOrderType, ApiKey, IndexPriceState, MarginTiers, PerpPosition,
    UserAccount,
};
use alloy_sol_types::{SolCall, SolEvent};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const MKT: u64 = 3;
const TICK: u64 = 5;
const ADMIN: Address = Address::repeat_byte(0xAD);
const RO: u8 = 1;
const CP: u8 = 2;
const TRAIL: u8 = AlgoOrderType::TrailingStopMarket as u8;
/// Every seeded position's size: far above any order quantity, so a fire is never sized down.
const POS: i64 = 1_000;

type Id = [u8; 32];

// ── Fixture ───────────────────────────────────────────────────────────────────────────────────

fn user(i: usize) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0x7a;
    b[12..].copy_from_slice(&(i as u64).to_be_bytes());
    Address::from(b)
}

fn base_market(mark: u64) -> Market {
    Market {
        market_id: MKT,
        base_decimals: 0,
        price_decimals: 2,
        tick_size: TICK,
        step_size: 1,
        min_quantity: 1,
        max_quantity: 1_000_000,
        max_price: 10_000_000_000,
        price_update_interval: 15,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 0,
        // Disabled band: no level is ever stranded, so the band GC never runs in these tests.
        price_band_bps: 1_000_000,
        mark_price: mark,
        tiers: MarginTiers::default(),
        basis_mode: 0,
        deferred_work: 0,
    }
}

/// A market at `mark`, seq 0 (no oracle tick yet).
fn new_host(mark: u64) -> InMemoryHost {
    let mut h = InMemoryHost::new(1);
    storage::save_admin(&mut h, ADMIN).unwrap();
    storage::save_market(&mut h, &base_market(mark)).unwrap();
    storage::save_index_price_state(&mut h, MKT, &IndexPriceState { index_price: mark, timestamp: 0 })
        .unwrap();
    h
}

/// A heavily over-collateralised position of `amount` at `mark`: never liquidated, never sized down.
fn seed_position(h: &mut InMemoryHost, u: Address, amount: i64, mark: u64) {
    storage::save_account(
        h,
        u,
        UserAccount { perp_wallet_balance: 1_000_000_000_000_000, ..UserAccount::default() },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let value = mark as i64 * 10_000 * amount.abs();
    storage::save_position(
        h,
        u,
        MKT,
        &PerpPosition {
            amount,
            v_quote_balance: if amount > 0 { -value } else { value },
            margin: 100 * value,
            leverage: 5,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

/// Close and reopen the same way: one more `generation`, same exposure.
fn bump_generation(h: &mut InMemoryHost, u: Address) {
    let p = storage::load_position(h, u, MKT).unwrap();
    let flat = PerpPosition { amount: 0, v_quote_balance: 0, margin: 0, ..p.clone() };
    storage::save_position(h, u, MKT, &flat, AccountUpdateReason::Adjustment).unwrap();
    storage::save_position(h, u, MKT, &p, AccountUpdateReason::Adjustment).unwrap();
}

fn trailing_call(side: Side, activation: u64, cb: u64, qty: u64, flags: u8) -> Vec<u8> {
    placeAlgoOrderCall {
        algoType: 0,
        marketId: MKT,
        side: side as u8,
        orderType: TRAIL,
        triggerPrice: activation,
        price: cb,
        quantity: qty,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

fn try_place(
    h: &mut InMemoryHost,
    owner: Address,
    side: Side,
    activation: u64,
    cb: u64,
    qty: u64,
) -> Result<Id, PerpError> {
    crate::algo::run_place_algo_order(&trailing_call(side, activation, cb, qty, RO), owner, h)
        .map(|b| b[..32].try_into().unwrap())
}

fn place(h: &mut InMemoryHost, owner: Address, side: Side, activation: u64, cb: u16) -> Id {
    try_place(h, owner, side, activation, cb as u64, 1).unwrap()
}

fn cancel(h: &mut InMemoryHost, owner: Address, id: &Id) {
    crate::algo::run_cancel_algo_order(
        &cancelAlgoOrderCall { algoId: (*id).into(), marketId: MKT }.abi_encode(),
        owner,
        h,
    )
    .unwrap();
}

fn view(h: &mut InMemoryHost, id: &Id) -> crate::interface::IPerpDex::getAlgoOrderReturn {
    let ret = crate::algo::run_get_algo_order(
        &getAlgoOrderCall { algoId: (*id).into() }.abi_encode(),
        h,
    )
    .unwrap();
    getAlgoOrderCall::abi_decode_returns(&ret).unwrap()
}

fn open_algo_orders(h: &mut InMemoryHost, u: Address) -> BTreeSet<Id> {
    let ret = crate::algo::run_get_open_algo_orders(
        &getOpenAlgoOrdersCall { user: u, marketId: MKT }.abi_encode(),
        h,
    )
    .unwrap();
    getOpenAlgoOrdersCall::abi_decode_returns(&ret).unwrap().into_iter().map(|b| b.0).collect()
}

/// Store a mark at `seq` exactly as the oracle path does (index state, then the mark).
fn set_mark(h: &mut InMemoryHost, seq: u64, mark: u64) {
    storage::save_index_price_state(h, MKT, &IndexPriceState { index_price: mark, timestamp: seq })
        .unwrap();
    storage::save_mark_price(h, MKT, mark).unwrap();
}

fn market(h: &mut InMemoryHost) -> std::sync::Arc<Market> {
    storage::load_market_ref(h, MKT).unwrap().unwrap()
}

fn oracle(h: &mut InMemoryHost, index: u64, ts: u64) {
    crate::risk::run_update_index_price(
        &updateIndexPriceCall { marketId: MKT, indexPrice: index, timestamp: ts }.abi_encode(),
        ADMIN,
        h,
    )
    .unwrap();
}

fn drain(h: &mut InMemoryHost) -> bool {
    let ret = crate::risk::run_drain_deferred_work(
        &drainDeferredWorkCall { marketId: MKT }.abi_encode(),
        h,
    )
    .unwrap();
    drainDeferredWorkCall::abi_decode_returns(&ret).unwrap()
}

fn ev<E: SolEvent>(log: &Log) -> Option<E> {
    (log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .then(|| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
}

/// What one pass of the trailing legs did, read off the event stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Pass {
    /// `AlgoOrderTriggered`, in order: (id, reported stop, mark).
    fired: Vec<(Id, u64, u64)>,
    /// Fired ids followed by `AlgoOrderCancelled(PositionClosed)`.
    fired_expired: Vec<Id>,
    /// The activation leg, in order: (id, true) activated, (id, false) spent as `PositionClosed`.
    activations: Vec<(Id, bool)>,
    fire_deferred: bool,
    act_deferred: bool,
}

fn read_pass(logs: &[Log]) -> Pass {
    let mut p = Pass::default();
    let mut last_fired: Option<Id> = None;
    for l in logs {
        if let Some(t) = ev::<AlgoOrderTriggered>(l) {
            p.fired.push((t.algoId.0, t.triggerPrice, t.markPrice));
            last_fired = Some(t.algoId.0);
        } else if let Some(a) = ev::<AlgoOrderActivated>(l) {
            p.activations.push((a.algoId.0, true));
        } else if let Some(c) = ev::<AlgoOrderCancelled>(l) {
            if c.reason == CancelReason::PositionClosed as u8 {
                if last_fired == Some(c.algoId.0) {
                    p.fired_expired.push(c.algoId.0);
                } else {
                    p.activations.push((c.algoId.0, false));
                }
            }
        }
    }
    p
}

/// One pass of the per-tick trailing legs in `run_deferred_sweeps`' order — the fire walk on
/// `budget`, then the activation walk on `cap` — with the stack step before it.
fn run_legs(h: &mut InMemoryHost, seq: u64, mark: u64, budget: u32, cap: u32) -> Pass {
    on_mark(h, MKT, seq, mark).unwrap();
    let m = market(h);
    let from = h.logs.len();
    let mut b = budget;
    let fd = run_fire_sweep(h, MKT, &m, mark, &mut b).unwrap();
    let mut c = cap;
    let ad = run_activation_sweep(h, MKT, &m, seq, mark, &mut c).unwrap();
    let mut p = read_pass(&h.logs[from..]);
    p.fire_deferred = fd;
    p.act_deferred = ad;
    p
}

// ── The predicate ─────────────────────────────────────────────────────────────────────────────

#[test]
fn the_threshold_is_exactly_the_cross_multiplied_predicate() {
    let mut r = Rng(0x5eed);
    for _ in 0..200_000 {
        let span = if r.chance(50) { 1_000 } else { i64::MAX as u64 };
        let peak = 1 + r.below(span);
        let cb = 10 * (1 + r.below(100)) as u16;
        for side in SIDES {
            let t = thr(side, peak, cb);
            for m in [t.saturating_sub(1), t, t.saturating_add(1), 1 + r.below(2 * peak.min(u64::MAX / 2))] {
                assert_eq!(fires(side, m, peak, cb), fires_thr(side, m, t), "{side:?} p={peak} cb={cb} m={m}");
            }
        }
    }
}

#[test]
fn the_reported_stop_agrees_with_the_predicate_on_the_tick_grid() {
    let mut r = Rng(7);
    for _ in 0..50_000 {
        let tick = [1, 5, 10, 25, 100][r.below(5) as usize];
        let peak = tick * (1 + r.below(1_000_000));
        let cb = 10 * (1 + r.below(100)) as u16;
        for side in SIDES {
            let stop = display_stop(side, peak, cb, tick);
            assert_eq!(stop % tick, 0);
            for k in [(stop / tick).saturating_sub(1), stop / tick, stop / tick + 1] {
                let m = k * tick;
                if m == 0 {
                    continue;
                }
                let by_stop = match side {
                    Side::Sell => m <= stop,
                    Side::Buy => m >= stop,
                };
                assert_eq!(fires(side, m, peak, cb), by_stop, "{side:?} p={peak} cb={cb} tick={tick} m={m}");
            }
        }
    }
}

/// L4, for every mark: an order whose peak IS the mark cannot fire at it.
#[test]
fn a_peak_never_fires_at_itself() {
    let mut r = Rng(11);
    for m in (1..2_000).chain((0..10_000).map(|_| 1 + r.below(i64::MAX as u64))) {
        for cb in (CALLBACK_MIN_BPS..=CALLBACK_MAX_BPS).step_by(CALLBACK_STEP_BPS as usize) {
            for side in SIDES {
                assert!(!fires(side, m, m, cb), "{side:?} m={m} cb={cb}");
            }
        }
    }
}

#[test]
fn the_callback_grid() {
    let ok: Vec<u64> = (0..=2_000).filter(|&b| valid_callback(b)).collect();
    assert_eq!(ok.len(), 100);
    assert_eq!((ok[0], ok[99]), (10, 1000));
    assert!(ok.iter().all(|b| b % 10 == 0));
    assert!(!valid_callback(u64::MAX));
}

// ── The full invariant checker ────────────────────────────────────────────────────────────────

/// Every stored (non-deleted) key → value, overlay over committed.
fn stored(h: &InMemoryHost) -> BTreeMap<B256, Vec<u8>> {
    let mut out: BTreeMap<B256, Vec<u8>> =
        h.committed.iter().map(|(k, (v, _))| (*k, v.clone())).collect();
    for (k, v) in &h.overlay {
        if v.is_empty() {
            out.remove(k);
        } else {
            out.insert(*k, v.clone());
        }
    }
    out
}

/// Every stored trailing key except the meta and the stack pages, plus the LIVE stack entries —
/// what an idempotent step must leave untouched (the GC may delete garbage pages).
fn structural(h: &InMemoryHost) -> BTreeMap<B256, Vec<u8>> {
    stored(h)
        .into_iter()
        .filter(|(k, _)| {
            [b"trld", b"trlb", b"trlo", b"tpnp", b"tpnl"].iter().any(|p| k.0[..4] == **p)
        })
        .collect()
}

/// The meta with the GC's own bookkeeping masked: the garbage range, and the index reset of a side
/// that holds nothing (an empty stack's position is not state).
fn without_gc(mut m: Meta) -> Meta {
    for s in &mut m.sides {
        s.gc_lo = 0;
        s.gc_hi = 0;
        if s.stack_empty() {
            s.base = 0;
            s.len = 0;
        }
    }
    m
}

fn is_family(k: &B256, prefix: &[u8; 4], side: Option<Side>) -> bool {
    k.0[..4] == *prefix
        && (prefix == b"trlo" || k.0[4..12] == MKT.to_be_bytes())
        && side.is_none_or(|s| k.0[12] == s as u8)
}

/// What the structure holds, as the checker walked it.
#[derive(Debug, Default)]
struct Walked {
    /// id → (cb, tag), per side, in bucket order.
    active: [BTreeMap<Id, (u16, u64)>; 2],
    /// id → activation price, per side.
    pending: [BTreeMap<Id, u64>; 2],
    /// Bucket orders and pending-level orders, as walked head → tail.
    queues: Vec<Vec<Id>>,
}

/// I1–I5 in full, the page accounting, and no orphan key. Panics with what broke.
fn check_structure(h: &mut InMemoryHost) -> Walked {
    let kv = stored(h);
    let meta = load_meta(h, MKT).unwrap();
    assert_eq!(
        kv.contains_key(&trail_meta_key(MKT)),
        meta != Meta::default(),
        "the meta key exists exactly when the meta is not all-default"
    );
    let mut w = Walked::default();
    let mut links = 0usize;
    for side in SIDES {
        let s = side as usize;
        let sm = meta.sides[s];
        assert!(sm.base <= sm.len);
        assert!(sm.gc_lo <= sm.base / PAGE && sm.len.div_ceil(PAGE) <= sm.gc_hi.max(sm.base / PAGE));
        // Stored pages lie in [gc_lo, gc_hi); the live ones are present.
        let pages: BTreeMap<u64, Vec<u8>> = kv
            .iter()
            .filter(|(k, _)| is_family(k, b"trls", Some(side)))
            .map(|(k, v)| (be64(&k.0[13..]), v.clone()))
            .collect();
        for p in pages.keys() {
            assert!(sm.gc_lo <= *p && *p < sm.gc_hi, "{side:?}: page {p} outside [{}, {})", sm.gc_lo, sm.gc_hi);
        }
        let entries: Vec<(u64, u64)> = (sm.base..sm.len)
            .map(|i| {
                let v = pages.get(&(i / PAGE)).unwrap_or_else(|| panic!("{side:?}: live page {} missing", i / PAGE));
                let o = (i % PAGE) as usize * 16;
                assert!(v.len() >= o + 16, "{side:?}: live entry {i} missing");
                (be64(&v[o..]), be64(&v[o + 8..]))
            })
            .collect();
        // I1.
        for e in entries.windows(2) {
            assert!(e[0].0 < e[1].0, "{side:?}: seq not strictly increasing {e:?}");
            assert!(better(side, e[0].1, e[1].1), "{side:?}: peak not strictly worsening {e:?}");
        }
        if let Some(top) = entries.last() {
            assert_eq!((sm.top_seq, sm.top_peak), *top, "{side:?}: top mirror");
        }
        // DIR and I4/I5.
        let dir = load_dir(h, MKT, side).unwrap();
        assert_eq!(sm.dir_len, dir.len() as u64);
        assert_eq!(entries.is_empty(), dir.is_empty(), "{side:?}: stack empty iff directory empty");
        for d in dir.windows(2) {
            assert!(front_key(side, &d[0]) < front_key(side, &d[1]), "{side:?}: directory order");
        }
        if let Some(f) = dir.first() {
            assert_eq!(sm.best_thr, thr(side, f.peak, f.cb));
            assert_eq!(sm.max_front, dir.iter().map(|f| f.elem).max().unwrap());
            assert!(sm.base <= dir.iter().map(|f| f.elem).min().unwrap());
        }
        let bucket_cbs: BTreeSet<u16> = kv
            .keys()
            .filter(|k| is_family(k, b"trlb", Some(side)))
            .map(|k| be64(&k.0[13..]) as u16)
            .collect();
        assert_eq!(bucket_cbs, dir.iter().map(|f| f.cb).collect(), "{side:?}: one entry per bucket");
        let mut max_tag = 0;
        for f in &dir {
            let q = load_queue(h, trail_bucket_key(MKT, side as u8, f.cb)).unwrap().expect("bucket");
            let (mut cur, mut prev, mut last_tag, mut first) = (q.head, None, 0u64, true);
            let mut order_ids = Vec::new();
            loop {
                let l = load_link(h, &cur).unwrap().expect("bucket link");
                links += 1;
                assert!(l.active && l.prev == prev);
                if first {
                    assert_eq!(l.tag, f.tag, "{side:?} cb {}: front tag", f.cb);
                    first = false;
                }
                assert!(l.tag >= last_tag, "{side:?} cb {}: I3", f.cb);
                last_tag = l.tag;
                max_tag = max_tag.max(l.tag);
                let o = storage::load_order(h, &cur).unwrap().expect("bucket order");
                assert!(o.trail_bps == f.cb && o.side == side && o.market_id == MKT);
                // I2 for EVERY order, not just the head.
                let elem = sm.base + entries.partition_point(|e| e.0 < l.tag) as u64;
                assert!(elem < sm.len, "{side:?}: tag above the top");
                w.active[s].insert(cur, (f.cb, l.tag));
                order_ids.push(cur);
                match l.next {
                    None => {
                        assert_eq!(q.tail, cur);
                        break;
                    }
                    Some(n) => {
                        prev = Some(cur);
                        cur = n;
                    }
                }
            }
            w.queues.push(order_ids);
            let elem = sm.base + entries.partition_point(|e| e.0 < f.tag) as u64;
            assert_eq!(f.elem, elem, "{side:?} cb {}: front elem", f.cb);
            assert_eq!(f.peak, entries[(elem - sm.base) as usize].1, "{side:?} cb {}: front peak", f.cb);
        }
        if !entries.is_empty() {
            assert!(max_tag <= sm.top_seq, "{side:?}: a live tag above top_seq");
        }
        // PENDING.
        let prices = load_pending_prices(h, MKT, side).unwrap();
        for p in prices.windows(2) {
            assert!(better(side, p[1], p[0]), "{side:?}: pending prices not strictly ordered {p:?}");
        }
        assert_eq!(sm.pend_front, prices.first().copied().unwrap_or(0));
        let level_prices: BTreeSet<u64> = kv
            .keys()
            .filter(|k| is_family(k, b"tpnl", Some(side)))
            .map(|k| be64(&k.0[13..]))
            .collect();
        assert_eq!(level_prices, prices.iter().copied().collect(), "{side:?}: one level per price");
        for &price in &prices {
            let q = load_queue(h, trail_pending_level_key(MKT, side as u8, price)).unwrap().expect("level");
            let (mut cur, mut prev) = (q.head, None);
            let mut order_ids = Vec::new();
            loop {
                let l = load_link(h, &cur).unwrap().expect("pending link");
                links += 1;
                assert!(!l.active && l.tag == 0 && l.prev == prev);
                let o = storage::load_order(h, &cur).unwrap().expect("pending order");
                assert!(o.is_trailing() && o.side == side && o.trigger_price == price);
                w.pending[s].insert(cur, price);
                order_ids.push(cur);
                match l.next {
                    None => {
                        assert_eq!(q.tail, cur);
                        break;
                    }
                    Some(n) => {
                        prev = Some(cur);
                        cur = n;
                    }
                }
            }
            w.queues.push(order_ids);
        }
    }
    let link_keys = kv.keys().filter(|k| is_family(k, b"trlo", None)).count();
    assert_eq!(link_keys, links, "every link is in exactly one queue");
    w
}

// ── The reference model ───────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct RefOrder {
    owner: Address,
    side: Side,
    cb: u16,
    /// Effective activation price (the stored mark when placed with 0).
    activation: u64,
    active: bool,
    tag: u64,
    peak: u64,
    /// The owner's position generation at placement.
    gen: u64,
    /// Placement order (FIFO within a pending level) and activation order (FIFO within a bucket).
    place_no: u64,
    act_no: u64,
}

/// The O(N) per-order reference: every activated order carries its own peak.
#[derive(Default)]
struct Model {
    orders: BTreeMap<Id, RefOrder>,
    gen: HashMap<Address, u64>,
    next_no: u64,
}

impl Model {
    fn no(&mut self) -> u64 {
        self.next_no += 1;
        self.next_no
    }

    #[allow(clippy::too_many_arguments)]
    fn place(&mut self, id: Id, owner: Address, side: Side, cb: u16, sent: u64, seq: u64, mark: u64) {
        let gen = *self.gen.entry(owner).or_default();
        let no = self.no();
        let o = if sent == 0 {
            RefOrder { owner, side, cb, activation: mark, active: true, tag: seq, peak: mark, gen, place_no: no, act_no: no }
        } else {
            RefOrder { owner, side, cb, activation: sent, active: false, tag: 0, peak: 0, gen, place_no: no, act_no: 0 }
        };
        self.orders.insert(id, o);
    }

    fn on_mark(&mut self, mark: u64) {
        for o in self.orders.values_mut().filter(|o| o.active) {
            if better(o.side, mark, o.peak) {
                o.peak = mark;
            }
        }
    }

    /// Scan everything; fire in priority order on `budget`.
    fn fire_sweep(&mut self, mark: u64, budget: &mut u32) -> (Vec<(Id, u64, u64)>, Vec<Id>, bool) {
        let (mut fired, mut expired, mut deferred) = (Vec::new(), Vec::new(), false);
        for side in SIDES {
            // The documented priority, written out independently of `front_key`: the threshold
            // the mark crosses first, then the older activation, then the smaller callback, then
            // FIFO within the bucket.
            let mut c: Vec<(Id, (i128, u64, u16, u64))> = self
                .orders
                .iter()
                .filter(|(_, o)| o.active && o.side == side && fires(side, mark, o.peak, o.cb))
                .map(|(id, o)| {
                    let x = o.peak as i128 * match side {
                        Side::Sell => 10_000 - o.cb as i128,
                        Side::Buy => 10_000 + o.cb as i128,
                    };
                    let rank = match side {
                        Side::Sell => -x.div_euclid(10_000),
                        Side::Buy => -(-x).div_euclid(10_000),
                    };
                    (*id, (rank, o.tag, o.cb, o.act_no))
                })
                .collect();
            c.sort_by_key(|x| x.1);
            for (id, _) in c {
                if *budget == 0 {
                    deferred = true;
                    break;
                }
                *budget -= 1;
                let o = self.orders.remove(&id).unwrap();
                fired.push((id, display_stop(side, o.peak, o.cb, TICK), mark));
                if self.gen[&o.owner] != o.gen {
                    expired.push(id);
                }
            }
        }
        (fired, expired, deferred)
    }

    fn activation_sweep(&mut self, seq: u64, mark: u64, cap: &mut u32) -> (Vec<(Id, bool)>, bool) {
        let (mut out, mut deferred) = (Vec::new(), false);
        for side in SIDES {
            let mut c: Vec<(Id, (u64, u64))> = self
                .orders
                .iter()
                .filter(|(_, o)| !o.active && o.side == side && activates(side, o.activation, mark))
                .map(|(id, o)| {
                    let rank = match side {
                        Side::Sell => o.activation,
                        Side::Buy => u64::MAX - o.activation,
                    };
                    (*id, (rank, o.place_no))
                })
                .collect();
            c.sort_by_key(|x| x.1);
            for (id, _) in c {
                if *cap == 0 {
                    deferred = true;
                    break;
                }
                *cap -= 1;
                let owner = self.orders[&id].owner;
                if self.gen[&owner] != self.orders[&id].gen {
                    self.orders.remove(&id);
                    out.push((id, false));
                } else {
                    let no = self.no();
                    let o = self.orders.get_mut(&id).unwrap();
                    o.active = true;
                    o.tag = seq;
                    o.peak = mark;
                    o.act_no = no;
                    out.push((id, true));
                }
            }
        }
        (out, deferred)
    }

    fn pass(&mut self, seq: u64, mark: u64, budget: u32, cap: u32) -> Pass {
        self.on_mark(mark);
        let mut b = budget;
        let (fired, fired_expired, fire_deferred) = self.fire_sweep(mark, &mut b);
        let mut c = cap;
        let (activations, act_deferred) = self.activation_sweep(seq, mark, &mut c);
        Pass { fired, fired_expired, activations, fire_deferred, act_deferred }
    }

    fn cancel_all(&mut self, owner: Address) {
        self.orders.retain(|_, o| o.owner != owner);
    }
}

/// The structure against the model for every live order: same activation state and, when active,
/// the same tag, peak and reported stop (through `getAlgoOrder`), same bucket FIFO order, same
/// pending FIFO order, and same per-user open lists.
fn compare(h: &mut InMemoryHost, m: &Model, users: &[Address], full: bool) {
    let w = check_structure(h);
    for side in SIDES {
        let s = side as usize;
        let want_active: BTreeMap<Id, (u16, u64)> = m
            .orders
            .iter()
            .filter(|(_, o)| o.side == side && o.active)
            .map(|(id, o)| (*id, (o.cb, o.tag)))
            .collect();
        assert_eq!(w.active[s], want_active, "{side:?}: activated set / buckets / tags");
        let want_pending: BTreeMap<Id, u64> = m
            .orders
            .iter()
            .filter(|(_, o)| o.side == side && !o.active)
            .map(|(id, o)| (*id, o.activation))
            .collect();
        assert_eq!(w.pending[s], want_pending, "{side:?}: pending set");
    }
    for q in &w.queues {
        let nos: Vec<u64> = q
            .iter()
            .map(|id| {
                let o = &m.orders[id];
                if o.active { o.act_no } else { o.place_no }
            })
            .collect();
        assert!(nos.windows(2).all(|x| x[0] < x[1]), "queue order is not FIFO: {nos:?}");
    }
    // Every order through the view on a full pass; otherwise a deterministic slice of them (the
    // structural check above already walked every order's tag and bucket).
    let stride = if full { 1 } else { 1 + m.orders.len() / 12 };
    for (id, o) in m.orders.iter().step_by(stride) {
        let v = view(h, id);
        assert_eq!(v.orderType, TRAIL);
        assert_eq!((v.callbackRateBps, v.price), (o.cb, o.cb as u64));
        assert_eq!(v.triggerPrice, o.activation);
        assert_eq!(v.activated, o.active);
        if o.active {
            assert_eq!(v.peakPrice, o.peak, "peak of {}", hex(id));
            assert_eq!(v.currentStop, display_stop(o.side, o.peak, o.cb, TICK));
        } else {
            assert_eq!((v.peakPrice, v.currentStop), (0, 0));
        }
    }
    if full {
        for &u in users {
            let want: BTreeSet<Id> =
                m.orders.iter().filter(|(_, o)| o.owner == u).map(|(id, _)| *id).collect();
            assert_eq!(open_algo_orders(h, u), want, "getOpenAlgoOrders of {u}");
        }
    }
}

fn hex(id: &Id) -> String {
    id[..4].iter().map(|b| format!("{b:02x}")).collect()
}

// ── Deterministic generator ───────────────────────────────────────────────────────────────────

/// splitmix64: a dependency-free deterministic generator, so a failing seed reproduces exactly.
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
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    fn pick<T: Copy>(&mut self, v: &[T]) -> T {
        v[self.below(v.len() as u64) as usize]
    }
}

// ── The differential property tests ──────────────────────────────────────────────────────────

struct World {
    h: InMemoryHost,
    m: Model,
    r: Rng,
    seq: u64,
    mark: u64,
    /// Even index: long (SELL trailers), odd: short (BUY trailers).
    users: Vec<Address>,
    st: Stats,
    /// 0 flat, 1 up, 2 down.
    seed_drift: u64,
}

/// What a differential run actually exercised — asserted at the end, so a generator change that
/// quietly stops reaching a path fails loudly instead of passing vacuously.
#[derive(Debug, Default)]
struct Stats {
    placed_now: u64,
    placed_pending: u64,
    fired: u64,
    fired_expired: u64,
    activated: u64,
    activation_expired: u64,
    /// Activations performed by a drain round or after a skipped drain (a later tick).
    late_activations: u64,
    fire_deferrals: u64,
    activation_deferrals: u64,
    skipped_drains: u64,
    multi_bucket_passes: u64,
    two_sided_passes: u64,
    head_cancels: u64,
    mid_cancels: u64,
    pending_cancels: u64,
    cancel_alls: u64,
    generations: u64,
    max_live: usize,
    max_stack: u64,
}

impl Stats {
    fn pass(&mut self, m: &Model, p: &Pass, late: bool, fired_from: &[(Side, u16)]) {
        self.fired += p.fired.len() as u64;
        self.fired_expired += p.fired_expired.len() as u64;
        let act = p.activations.iter().filter(|a| a.1).count() as u64;
        self.activated += act;
        self.activation_expired += p.activations.len() as u64 - act;
        if late {
            self.late_activations += act;
        }
        self.fire_deferrals += u64::from(p.fire_deferred);
        self.activation_deferrals += u64::from(p.act_deferred);
        let cbs: BTreeSet<(u8, u16)> = fired_from.iter().map(|(s, c)| (*s as u8, *c)).collect();
        self.multi_bucket_passes += u64::from(cbs.len() >= 2);
        let sides: BTreeSet<u8> = cbs.iter().map(|x| x.0).collect();
        self.two_sided_passes += u64::from(sides.len() == 2);
        self.max_live = self.max_live.max(m.orders.len());
    }

    fn absorb(&mut self, o: &Stats) {
        self.placed_now += o.placed_now;
        self.placed_pending += o.placed_pending;
        self.fired += o.fired;
        self.fired_expired += o.fired_expired;
        self.activated += o.activated;
        self.activation_expired += o.activation_expired;
        self.late_activations += o.late_activations;
        self.fire_deferrals += o.fire_deferrals;
        self.activation_deferrals += o.activation_deferrals;
        self.skipped_drains += o.skipped_drains;
        self.multi_bucket_passes += o.multi_bucket_passes;
        self.two_sided_passes += o.two_sided_passes;
        self.head_cancels += o.head_cancels;
        self.mid_cancels += o.mid_cancels;
        self.pending_cancels += o.pending_cancels;
        self.cancel_alls += o.cancel_alls;
        self.generations += o.generations;
        self.max_live = self.max_live.max(o.max_live);
        self.max_stack = self.max_stack.max(o.max_stack);
    }

    fn assert_covered(&self) {
        println!("{self:#?}");
        let floor = [
            ("placed_now", self.placed_now, 500),
            ("placed_pending", self.placed_pending, 500),
            ("fired", self.fired, 500),
            ("fired_expired", self.fired_expired, 5),
            ("activated", self.activated, 300),
            ("activation_expired", self.activation_expired, 5),
            ("late_activations", self.late_activations, 20),
            ("fire_deferrals", self.fire_deferrals, 50),
            ("activation_deferrals", self.activation_deferrals, 20),
            ("skipped_drains", self.skipped_drains, 20),
            ("multi_bucket_passes", self.multi_bucket_passes, 20),
            ("two_sided_passes", self.two_sided_passes, 3),
            ("head_cancels", self.head_cancels, 20),
            ("mid_cancels", self.mid_cancels, 20),
            ("pending_cancels", self.pending_cancels, 20),
            ("cancel_alls", self.cancel_alls, 5),
            ("generations", self.generations, 5),
            ("max_stack", self.max_stack, 8),
        ];
        for (name, got, want) in floor {
            assert!(got >= want, "coverage: {name} = {got} < {want}");
        }
    }
}

const CBS: [u16; 8] = [10, 20, 30, 50, 100, 250, 500, 1000];
const MARK0: u64 = 20_000;

impl World {
    fn new(seed: u64, n_users: usize) -> Self {
        let mut h = new_host(MARK0);
        let users: Vec<Address> = (0..n_users).map(user).collect();
        for (i, &u) in users.iter().enumerate() {
            seed_position(&mut h, u, if i % 2 == 0 { POS } else { -POS }, MARK0);
        }
        let mut m = Model::default();
        for &u in &users {
            m.gen.insert(u, storage::load_position(&mut h, u, MKT).unwrap().generation);
        }
        Self { h, m, r: Rng(seed), seq: 0, mark: MARK0, users, st: Stats::default(), seed_drift: seed % 3 }
    }

    /// (side, cb) of each order a pass will fire — read before the pass removes them.
    fn sources(&self, ids: &[(Id, u64, u64)]) -> Vec<(Side, u16)> {
        ids.iter().filter_map(|(id, _, _)| self.m.orders.get(id).map(|o| (o.side, o.cb))).collect()
    }

    fn note_stack(&mut self) {
        let meta = load_meta(&mut self.h, MKT).unwrap();
        for sm in meta.sides {
            self.st.max_stack = self.st.max_stack.max(sm.len - sm.base);
        }
    }

    fn side_of(&self, u: Address) -> Side {
        if self.users.iter().position(|x| *x == u).unwrap() % 2 == 0 {
            Side::Sell
        } else {
            Side::Buy
        }
    }

    /// Random placements, cancels, cancel-alls and position generations between two ticks.
    fn between_ticks(&mut self) {
        for _ in 0..self.r.below(5) {
            let u = self.r.pick(&self.users);
            if self.m.orders.values().filter(|o| o.owner == u).count()
                >= crate::algo::MAX_ALGO_ORDERS_PER_MARKET
            {
                continue;
            }
            let side = self.side_of(u);
            let cb = if self.r.chance(80) { self.r.pick(&CBS) } else { 10 * self.r.range(1, 100) as u16 };
            let sent = if self.r.chance(50) {
                0
            } else {
                let d = TICK * self.r.range(1, 40);
                match side {
                    Side::Sell => self.mark + d,
                    Side::Buy => self.mark - d,
                }
            };
            let id = place(&mut self.h, u, side, sent, cb);
            self.m.place(id, u, side, cb, sent, self.seq, self.mark);
            if sent == 0 {
                self.st.placed_now += 1;
            } else {
                self.st.placed_pending += 1;
            }
        }
        if !self.m.orders.is_empty() && self.r.chance(30) {
            for _ in 0..self.r.range(1, 3) {
                let ids: Vec<Id> = self.m.orders.keys().copied().collect();
                if ids.is_empty() {
                    break;
                }
                let id = self.r.pick(&ids);
                let owner = self.m.orders[&id].owner;
                match load_link(&mut self.h, &id).unwrap() {
                    Some(l) if !l.active => self.st.pending_cancels += 1,
                    Some(l) if l.prev.is_none() => self.st.head_cancels += 1,
                    _ => self.st.mid_cancels += 1,
                }
                cancel(&mut self.h, owner, &id);
                self.m.orders.remove(&id);
            }
        }
        if self.r.chance(3) {
            let u = self.r.pick(&self.users);
            let mk = market(&mut self.h);
            let reason = if self.r.chance(50) { CancelReason::Liquidation } else { CancelReason::Adl };
            crate::risk::cancel_all_orders_for_market(&mut self.h, u, MKT, &mk, reason).unwrap();
            self.m.cancel_all(u);
            self.st.cancel_alls += 1;
        }
        if self.r.chance(4) {
            let u = self.r.pick(&self.users);
            bump_generation(&mut self.h, u);
            *self.m.gen.get_mut(&u).unwrap() += 1;
            self.st.generations += 1;
        }
        if self.r.chance(40) {
            self.h.end_block();
        }
    }

    fn next_mark(&mut self) -> u64 {
        let ticks = match self.r.below(100) {
            0..=84 => self.r.below(4),
            85..=94 => self.r.range(10, 80),
            _ => self.r.range(200, 600),
        };
        // A per-seed drift: a trend is what grows a stack (each activation at a worse mark than
        // the top pushes), and what makes every order of a bucket share one high.
        let up = self.r.chance(match self.seed_drift {
            0 => 50,
            1 => 62,
            _ => 38,
        });
        let m = if up { self.mark + TICK * ticks } else { self.mark.saturating_sub(TICK * ticks) };
        m.max(2_000)
    }
}

/// THE main defence against a stop that silently never fires: the grouped-lazy structure against
/// the O(N) model, step for step, on random mark paths (small moves, gaps, crashes), random
/// placements (now / pending, eight-plus buckets, both sides), cancels at the head and mid-bucket,
/// liquidation / ADL cancel-alls, position generations (lazy expiry at fire AND at activation),
/// random block boundaries, and — the part a fixed budget never exercises — RANDOM fire budgets
/// and activation caps from 0, with drains that sometimes finish, sometimes stop part-way and
/// sometimes never come (missed fires, late activations).
#[test]
fn differential_random_budgets() {
    let seeds: u64 = std::env::var("TRAIL_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(36);
    let mut total = Stats::default();
    for seed in 0..seeds {
        let mut w = World::new(seed, 10);
        let mut late = false;
        for step in 0..220 {
            w.between_ticks();
            compare(&mut w.h, &w.m, &w.users, step % 8 == 0);
            w.seq += 15 * w.r.range(1, 3);
            w.mark = w.next_mark();
            set_mark(&mut w.h, w.seq, w.mark);
            let mut round = 0;
            loop {
                let (budget, cap) = (w.r.below(7) as u32, w.r.below(5) as u32);
                if round > 0 {
                    // A drain re-runs the stack step at the same (seq, mark): it must change
                    // nothing but the lazy GC's progress — no live entry, front or cache moves.
                    let before = ops::snapshot();
                    let (m0, kv0) = (load_meta(&mut w.h, MKT).unwrap(), structural(&w.h));
                    on_mark(&mut w.h, MKT, w.seq, w.mark).unwrap();
                    let (m1, kv1) = (load_meta(&mut w.h, MKT).unwrap(), structural(&w.h));
                    let o = ops::since(before);
                    assert!(o.writes <= 1, "seed {seed} step {step}: only the meta may be rewritten");
                    assert_eq!(kv0, kv1, "seed {seed} step {step}: step 1 not idempotent");
                    assert_eq!(without_gc(m0), without_gc(m1), "seed {seed} step {step}: step 1 not idempotent");
                }
                let got = run_legs(&mut w.h, w.seq, w.mark, budget, cap);
                let from = w.sources(&got.fired);
                let want = w.m.pass(w.seq, w.mark, budget, cap);
                assert_eq!(got, want, "seed {seed} step {step} round {round} (budget {budget}, cap {cap})");
                w.st.pass(&w.m, &got, late || round > 0, &from);
                w.note_stack();
                compare(&mut w.h, &w.m, &w.users, false);
                round += 1;
                late = got.act_deferred;
                if !(got.fire_deferred || got.act_deferred) {
                    break;
                }
                if w.r.chance(25) {
                    w.st.skipped_drains += 1;
                    break;
                }
                if w.r.chance(30) {
                    w.h.end_block();
                }
            }
        }
        compare(&mut w.h, &w.m, &w.users, true);
        total.absorb(&w.st);
    }
    total.assert_covered();
}

/// `deferred_work` after a full `run_deferred_sweeps` in which only trailing orders act: the two
/// trailing bits, plus the liquidation sweep's CONSERVATIVE report — it says "deferred" whenever
/// it finds the shared budget already spent with a registered position unexamined (every account
/// here holds one), even if nothing is liquidatable.
fn oracle_deferred(p: &Pass) -> bool {
    p.fire_deferred || p.act_deferred || p.fired.len() >= 50
}

/// The same comparison through the production path: `updateIndexPrice` (mark = index here: no
/// funding, no book) runs `run_deferred_sweeps` with the real 50 / 64 budgets, and
/// `drainDeferredWork` re-runs it. Big gaps and many orders make overflow routine.
#[test]
fn differential_through_the_oracle_path() {
    let seeds: u64 = std::env::var("TRAIL_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(12);
    for seed in 0..seeds {
        let mut w = World::new(1_000 + seed, 16);
        for step in 0..160 {
            for _ in 0..3 {
                w.between_ticks();
            }
            w.seq += 15 * w.r.range(1, 3);
            let index = w.next_mark();
            let from = w.h.logs.len();
            oracle(&mut w.h, index, w.seq);
            let mk = market(&mut w.h);
            assert_eq!(mk.mark_price, index, "fixture: the mark is the index");
            w.mark = index;
            let mut got = read_pass(&w.h.logs[from..]);
            let want = w.m.pass(w.seq, w.mark, 50, MAX_TRAILING_ACTIVATIONS_PER_UPDATE);
            let deferred = oracle_deferred(&want);
            got.fire_deferred = want.fire_deferred;
            got.act_deferred = want.act_deferred;
            assert_eq!(got, want, "seed {seed} step {step}");
            assert_eq!(mk.deferred_work != 0, deferred, "seed {seed} step {step}: deferred_work");
            compare(&mut w.h, &w.m, &w.users, step % 8 == 0);
            let mut more = deferred;
            while more && w.r.chance(85) {
                let from = w.h.logs.len();
                let still = drain(&mut w.h);
                let mut got = read_pass(&w.h.logs[from..]);
                let want = w.m.pass(w.seq, w.mark, 50, MAX_TRAILING_ACTIVATIONS_PER_UPDATE);
                more = oracle_deferred(&want);
                got.fire_deferred = want.fire_deferred;
                got.act_deferred = want.act_deferred;
                assert_eq!(got, want, "seed {seed} step {step} (drain)");
                assert_eq!(still, more, "seed {seed} step {step}: drain's moreRemaining");
                compare(&mut w.h, &w.m, &w.users, false);
            }
        }
    }
}

// ── The worst case: decline, then a spike ───────────────────────────────────────────────────

/// `d` ticks of a slow decline with one 10 %-callback SELL activated per tick: the stack grows by
/// one entry per tick, each activation being at a strictly lower mark than the top. Returns the
/// host and the last (seq, mark).
fn decline(d: usize) -> (InMemoryHost, u64, u64) {
    let m0: u64 = 10_000_000;
    let mut h = new_host(m0);
    let users = d.div_ceil(crate::algo::MAX_ALGO_ORDERS_PER_MARKET);
    for i in 0..users {
        seed_position(&mut h, user(i), POS, m0);
    }
    let (mut seq, mut mark) = (0u64, m0);
    for k in 0..d {
        seq += 15;
        mark -= TICK;
        set_mark(&mut h, seq, mark);
        let p = run_legs(&mut h, seq, mark, 50, MAX_TRAILING_ACTIVATIONS_PER_UPDATE);
        assert!(p.fired.is_empty());
        place(&mut h, user(k / crate::algo::MAX_ALGO_ORDERS_PER_MARKET), Side::Sell, 0, 1000);
        if k % 512 == 0 {
            h.end_block();
        }
    }
    h.end_block();
    (h, seq, mark)
}

/// [`decline`], then a spike above every peak. Returns the stack length before the spike and the
/// trailing storage operations of the spike tick (all three legs).
fn decline_then_spike(d: usize) -> (u64, ops::Ops, InMemoryHost) {
    let (mut h, mut seq, _) = decline(d);
    let sm = load_meta(&mut h, MKT).unwrap().sides[Side::Sell as usize];
    let s = sm.len - sm.base;
    seq += 15;
    let spike = 11_000_000;
    set_mark(&mut h, seq, spike);
    let before = ops::snapshot();
    let p = run_legs(&mut h, seq, spike, 50, MAX_TRAILING_ACTIVATIONS_PER_UPDATE);
    let o = ops::since(before);
    assert!(p.fired.is_empty() && p.activations.is_empty());
    (s, o, h)
}

#[test]
fn decline_then_spike_costs_log_pages_not_n() {
    let (s_small, small, _) = decline_then_spike(1_000);
    let (s_big, big, mut h) = decline_then_spike(18_000);
    assert_eq!((s_small, s_big), (1_000, 18_000), "one stack entry per declining tick");
    let log2_pages = |s: u64| 64 - (s.div_ceil(PAGE) + 1).leading_zeros() as u64;
    // meta + ≤ ⌈log2 pages⌉ + 1 distinct page probes + the directory (stack step), meta (fire
    // walk), meta (activation walk).
    for (s, o) in [(s_small, small), (s_big, big)] {
        assert!(o.reads <= 5 + log2_pages(s), "S={s}: {o:?}");
        assert_eq!(o.writes, 3, "S={s}: the cut page, the directory, the meta — {o:?}");
        assert!(o.deletes <= 2 * GC_PAGES_PER_REGION as u64, "S={s}: bounded GC — {o:?}");
    }
    assert!(big.reads - small.reads <= log2_pages(s_big) - log2_pages(s_small) + 2, "{small:?} vs {big:?}");
    println!("spike tick, S={s_small}: {small:?}; S={s_big}: {big:?}");

    // The stack collapsed to one entry; the 281 pages above it are collected 8 per tick, and once
    // they are gone a quiet tick is ONE read and nothing else.
    let sm = load_meta(&mut h, MKT).unwrap().sides[Side::Sell as usize];
    assert_eq!(sm.len - sm.base, 1);
    let (mut seq, mark) = (sm.top_seq, sm.top_peak);
    let garbage_ticks = (s_big.div_ceil(PAGE) - 1).div_ceil(GC_PAGES_PER_REGION as u64);
    for _ in 0..garbage_ticks {
        seq += 15;
        set_mark(&mut h, seq, mark);
        run_legs(&mut h, seq, mark, 50, 64);
    }
    let pages = stored(&h).keys().filter(|k| is_family(k, b"trls", Some(Side::Sell))).count();
    assert_eq!(pages, 1, "all garbage pages collected");
    check_structure(&mut h);
    seq += 15;
    set_mark(&mut h, seq, mark);
    let before = ops::snapshot();
    run_legs(&mut h, seq, mark, 50, 64);
    let o = ops::since(before);
    // One read of the meta key per leg (stack step, fire walk, activation walk), nothing else.
    assert_eq!((o.writes, o.deletes), (0, 0), "a quiet tick writes nothing: {o:?}");
    assert_eq!(o.reads, 3, "a quiet tick reads only the meta: {o:?}");

    // Every one of the 18,000 orders now trails the spike and fires together on a 10 % retrace.
    let first = storage::load_user_algo_orders(&mut h, user(0), MKT).unwrap()[0];
    assert_eq!(view(&mut h, &first).peakPrice, mark);
}

/// One fire and one activation each cost a constant number of trailing operations plus the
/// refront's binary search — the per-event rows of the module's work table, measured. The fires
/// here are the expensive kind: every order sits on its own stack element, so each pop moves the
/// front one element up and searches for it.
#[test]
fn per_fire_and_per_activation_cost_is_constant() {
    let (mut h, mut seq, mark) = decline(4_000);
    let sm = load_meta(&mut h, MKT).unwrap().sides[Side::Sell as usize];
    assert_eq!(sm.len - sm.base, 4_000);
    let pages = sm.len.div_ceil(PAGE) + 1;
    let log2_pages = 64 - pages.leading_zeros() as u64;
    // A crash through every 10 % stop: they all fire, oldest (highest peak) first, one per call.
    seq += 15;
    let crash = mark / 10 * 8;
    set_mark(&mut h, seq, crash);
    on_mark(&mut h, MKT, seq, crash).unwrap();
    let mk = market(&mut h);
    let mut worst = ops::Ops::default();
    for _ in 0..40 {
        let before = ops::snapshot();
        let mut budget = 1;
        let from = h.logs.len();
        assert!(run_fire_sweep(&mut h, MKT, &mk, crash, &mut budget).unwrap(), "more to fire");
        let o = ops::since(before);
        assert_eq!(read_pass(&h.logs[from..]).fired.len(), 1);
        // The call's own meta read and its post-fire re-read, plus one fire.
        assert!(o.reads <= 2 + 8 + log2_pages, "fire: {o:?}");
        assert!(o.writes + o.deletes <= 5, "fire: {o:?}");
        worst.reads = worst.reads.max(o.reads);
        worst.writes = worst.writes.max(o.writes + o.deletes);
    }
    println!("one fire, S=4000: worst {worst:?}");
    check_structure(&mut h);

    // Activations: a pending crowd at one price, crossed, taken one per call.
    let u = user(9_000);
    seed_position(&mut h, u, POS, crash);
    for _ in 0..20 {
        place(&mut h, u, Side::Sell, crash + 50, 30);
    }
    seq += 15;
    let up = crash + 100;
    set_mark(&mut h, seq, up);
    on_mark(&mut h, MKT, seq, up).unwrap();
    let mk = market(&mut h);
    let mut worst = ops::Ops::default();
    for k in 0..19 {
        let before = ops::snapshot();
        let mut cap = 1;
        assert!(run_activation_sweep(&mut h, MKT, &mk, seq, up, &mut cap).unwrap());
        let o = ops::since(before);
        // The call's meta and price-list reads, plus one activation (the first one also opens
        // the bucket's directory entry and pushes a stack element).
        assert!(o.reads <= 2 + 10, "activation {k}: {o:?}");
        assert!(o.writes + o.deletes <= 1 + 9, "activation {k}: {o:?}");
        worst.reads = worst.reads.max(o.reads);
        worst.writes = worst.writes.max(o.writes + o.deletes);
    }
    println!("one activation: worst {worst:?}");
    check_structure(&mut h);
}

/// A placement costs a constant number of trailing operations whatever the stack length.
#[test]
fn placement_and_cancel_cost_is_constant() {
    let (_, _, mut h) = decline_then_spike(2_000);
    let u = user(999);
    seed_position(&mut h, u, POS, 10_000_000);
    for (sent, cb) in [(0u64, 30u16), (0, 30), (12_000_000, 40)] {
        let before = ops::snapshot();
        let id = place(&mut h, u, Side::Sell, sent, cb);
        let o = ops::since(before);
        assert!(o.reads <= 8 && o.writes + o.deletes <= 8, "place: {o:?}");
        let before = ops::snapshot();
        cancel(&mut h, u, &id);
        let o = ops::since(before);
        assert!(o.reads <= 8 + 13 && o.writes + o.deletes <= 8, "cancel: {o:?}");
        let id2 = place(&mut h, u, Side::Sell, sent, cb);
        let _ = id2;
    }
    check_structure(&mut h);
}

// ── Scenarios through the real oracle path ───────────────────────────────────────────────────

const ALICE: usize = 0;
const BOB: usize = 1;

fn scenario(mark: u64) -> InMemoryHost {
    let mut h = new_host(mark);
    seed_position(&mut h, user(ALICE), POS, mark);
    seed_position(&mut h, user(BOB), -POS, mark);
    h
}

fn take_pass(h: &mut InMemoryHost, from: usize) -> Pass {
    read_pass(&h.logs[from..])
}

/// SELL trails the high and fires on the retrace — at the threshold exactly (inclusive), reporting
/// the tick-rounded stop and the mark that fired it.
#[test]
fn a_sell_trails_the_high_and_fires_on_the_retrace() {
    let mut h = scenario(20_000);
    let from = h.logs.len();
    let id = place(&mut h, user(ALICE), Side::Sell, 0, 100);
    let placed: Vec<AlgoOrderPlaced> = h.logs[from..].iter().filter_map(ev).collect();
    assert_eq!(placed.len(), 1);
    assert_eq!((placed[0].orderType, placed[0].triggerPrice, placed[0].price), (TRAIL, 20_000, 100));
    let act: Vec<AlgoOrderActivated> = h.logs[from..].iter().filter_map(ev).collect();
    assert_eq!(act.len(), 1, "activated in the placement transaction");
    assert_eq!((act[0].markPrice, act[0].stopPrice), (20_000, 19_800));
    let v = view(&mut h, &id);
    assert!(v.activated && v.reduceOnly && !v.closePosition);
    assert_eq!((v.peakPrice, v.currentStop, v.triggerPrice, v.price), (20_000, 19_800, 20_000, 100));

    oracle(&mut h, 20_100, 15);
    oracle(&mut h, 20_500, 30);
    oracle(&mut h, 20_400, 45);
    let v = view(&mut h, &id);
    assert_eq!((v.peakPrice, v.currentStop), (20_500, 20_295), "floor(20500 × 0.99) = 20295");
    let from = h.logs.len();
    oracle(&mut h, 20_300, 60);
    assert!(take_pass(&mut h, from).fired.is_empty(), "20300 > 20295");
    let from = h.logs.len();
    oracle(&mut h, 20_295, 75);
    let p = take_pass(&mut h, from);
    assert_eq!(p.fired, vec![(id, 20_295, 20_295)], "fires AT the threshold");
    assert!(storage::load_user_algo_orders(&mut h, user(ALICE), MKT).unwrap().is_empty());
    check_structure(&mut h);
    // The emptied stack's last page is garbage now; the next tick's GC takes it, and with it the
    // meta: a market whose trailing orders are all gone carries no trailing key at all.
    oracle(&mut h, 20_300, 90);
    check_structure(&mut h);
    assert!(
        stored(&h).keys().all(|k| !k.0.starts_with(b"trl") && !k.0.starts_with(b"tpn")),
        "the last trailing order leaving removes every trailing key"
    );
}

#[test]
fn a_buy_trails_the_low_and_fires_on_the_bounce() {
    let mut h = scenario(20_000);
    let id = place(&mut h, user(BOB), Side::Buy, 0, 50);
    oracle(&mut h, 19_000, 15);
    oracle(&mut h, 19_050, 30);
    let v = view(&mut h, &id);
    assert_eq!((v.peakPrice, v.currentStop), (19_000, 19_095), "ceil(19000 × 1.005) = 19095");
    let from = h.logs.len();
    oracle(&mut h, 19_090, 45);
    assert!(take_pass(&mut h, from).fired.is_empty());
    let from = h.logs.len();
    oracle(&mut h, 19_100, 60);
    assert_eq!(take_pass(&mut h, from).fired, vec![(id, 19_095, 19_100)]);
}

/// Activation is inclusive, and a gap through the activation price seeds the peak at the mark the
/// crossing tick brought, not at the activation price (Binance: the highest price since placement).
#[test]
fn a_pending_order_activates_on_reaching_its_price_and_seeds_at_the_crossing_mark() {
    let mut h = scenario(20_000);
    let a = place(&mut h, user(ALICE), Side::Sell, 20_500, 100);
    let b = place(&mut h, user(ALICE), Side::Sell, 20_400, 100);
    let v = view(&mut h, &a);
    assert!(!v.activated);
    assert_eq!((v.triggerPrice, v.peakPrice, v.currentStop), (20_500, 0, 0));
    let from = h.logs.len();
    oracle(&mut h, 20_495, 15);
    assert_eq!(take_pass(&mut h, from).activations, vec![(b, true)], "20400 reached, 20500 not");
    let from = h.logs.len();
    oracle(&mut h, 21_000, 30);
    let p = take_pass(&mut h, from);
    assert_eq!(p.activations, vec![(a, true)]);
    let act: Vec<AlgoOrderActivated> = h.logs[from..].iter().filter_map(ev).collect();
    assert_eq!((act[0].markPrice, act[0].stopPrice), (21_000, 20_790));
    assert_eq!(view(&mut h, &a).peakPrice, 21_000, "seeded at the crossing mark");
    assert_eq!(view(&mut h, &b).peakPrice, 21_000, "and b, activated earlier, trailed up to it");
    // Exactly at the activation price counts.
    let c = place(&mut h, user(ALICE), Side::Sell, 21_100, 100);
    let from = h.logs.len();
    oracle(&mut h, 21_100, 45);
    assert_eq!(take_pass(&mut h, from).activations, vec![(c, true)]);
    check_structure(&mut h);
}

#[test]
fn placement_refusals() {
    let mut h = scenario(20_000);
    let u = user(ALICE);
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (trailing_call(Side::Sell, 20_000, 100, 1, RO), "order would immediately trigger"),
        (trailing_call(Side::Sell, 19_000, 100, 1, RO), "order would immediately trigger"),
        (trailing_call(Side::Sell, 0, 5, 1, RO), "callbackRate"),
        (trailing_call(Side::Sell, 0, 0, 1, RO), "callbackRate"),
        (trailing_call(Side::Sell, 0, 15, 1, RO), "callbackRate"),
        (trailing_call(Side::Sell, 0, 1010, 1, RO), "callbackRate"),
        (trailing_call(Side::Sell, 0, 100, 1, 0), "requires reduceOnly"),
        (trailing_call(Side::Sell, 0, 100, 0, CP), "closePosition is not supported"),
        (trailing_call(Side::Sell, 0, 100, 1, RO | CP), "reduceOnly cannot be sent with closePosition"),
        (trailing_call(Side::Sell, 0, 100, 0, RO), "quantity must be non-zero"),
        (trailing_call(Side::Sell, 20_501, 100, 1, RO), "multiple of tickSize"),
        (trailing_call(Side::Buy, 0, 100, 1, RO), "requires an open position on the closing side"),
    ];
    for (call, want) in cases {
        let writes = h.perp_write_count();
        let nonce = storage::load_user_nonce(&mut h, u).unwrap();
        let e = crate::algo::run_place_algo_order(&call, u, &mut h).unwrap_err();
        assert!(format!("{e:?}").contains(want), "want {want:?}, got {e:?}");
        assert_eq!(h.perp_write_count(), writes, "{want}: write-clean");
        assert_eq!(storage::load_user_nonce(&mut h, u).unwrap(), nonce);
    }
    let mut tif = placeAlgoOrderCall::abi_decode(&trailing_call(Side::Sell, 0, 100, 1, RO)).unwrap();
    tif.tif = 1;
    let e = crate::algo::run_place_algo_order(&tif.abi_encode(), u, &mut h).unwrap_err();
    assert!(format!("{e:?}").contains("tif is unused"), "{e:?}");
    // BUY mirror of the wrong-side activation, and a flat owner.
    let e = try_place(&mut h, user(BOB), Side::Buy, 20_000, 100, 1).unwrap_err();
    assert!(format!("{e:?}").contains("would immediately trigger"), "{e:?}");
    let e = try_place(&mut h, user(7), Side::Sell, 0, 100, 1).unwrap_err();
    assert!(format!("{e:?}").contains("requires an open position"), "{e:?}");
    // Counted in the 32 conditional orders.
    for _ in 0..crate::algo::MAX_ALGO_ORDERS_PER_MARKET {
        place(&mut h, u, Side::Sell, 0, 100);
    }
    let e = try_place(&mut h, u, Side::Sell, 0, 100, 1).unwrap_err();
    assert!(format!("{e:?}").contains("too many open conditional orders"), "{e:?}");
}

/// Bound to the position (decision 1): spent with `PositionClosed` when the position ended — at
/// FIRE for an activated order (`Triggered` then `Cancelled(11)`, nothing placed), at ACTIVATION
/// for a pending one (`Cancelled(11)` alone) — even though a same-way position is open again.
#[test]
fn a_position_that_ended_spends_the_order_at_fire_and_at_activation() {
    let mut h = scenario(20_000);
    let u = user(ALICE);
    let active = place(&mut h, u, Side::Sell, 0, 100);
    let pending = place(&mut h, u, Side::Sell, 20_500, 100);
    bump_generation(&mut h, u);
    let from = h.logs.len();
    oracle(&mut h, 20_500, 15);
    let p = take_pass(&mut h, from);
    assert_eq!(p.activations, vec![(pending, false)], "spent at activation, never activated");
    assert!(!h.logs[from..].iter().any(|l| ev::<AlgoOrderTriggered>(l).is_some_and(|t| t.algoId.0 == pending)));
    let from = h.logs.len();
    oracle(&mut h, 19_000, 30);
    let p = take_pass(&mut h, from);
    assert_eq!(p.fired.iter().map(|f| f.0).collect::<Vec<_>>(), vec![active]);
    assert_eq!(p.fired_expired, vec![active]);
    assert_eq!(storage::load_position(&mut h, u, MKT).unwrap().amount, POS, "the new long is untouched");
    assert!(open_algo_orders(&mut h, u).is_empty());
    check_structure(&mut h);
}

/// Cancel from the head and from the middle of a bucket, and of a pending level; the survivors
/// keep their peaks and still fire in order.
#[test]
fn cancels_anywhere_leave_the_rest_consistent() {
    let mut h = scenario(20_000);
    let u = user(ALICE);
    let a = place(&mut h, u, Side::Sell, 0, 100);
    oracle(&mut h, 20_100, 15);
    let b = place(&mut h, u, Side::Sell, 0, 100);
    oracle(&mut h, 20_050, 30);
    let c = place(&mut h, u, Side::Sell, 0, 100);
    let d = place(&mut h, u, Side::Sell, 0, 100);
    let p1 = place(&mut h, u, Side::Sell, 20_600, 100);
    let p2 = place(&mut h, u, Side::Sell, 20_600, 100);
    check_structure(&mut h);
    cancel(&mut h, u, &c); // middle of the bucket
    cancel(&mut h, u, &a); // the head: the front moves to b's element
    cancel(&mut h, u, &p1); // head of a pending level
    let w = check_structure(&mut h);
    assert_eq!(w.active[Side::Sell as usize].keys().copied().collect::<BTreeSet<_>>(), [b, d].into());
    assert_eq!(w.pending[Side::Sell as usize].keys().copied().collect::<Vec<_>>(), vec![p2]);
    assert_eq!(view(&mut h, &b).peakPrice, 20_100);
    assert_eq!(view(&mut h, &d).peakPrice, 20_050);
    // 20_100 × 0.99 = 19_899 fires b only; d (peak 20_050 → 19_849.5) waits.
    let from = h.logs.len();
    oracle(&mut h, 19_895, 45);
    assert_eq!(take_pass(&mut h, from).fired.iter().map(|f| f.0).collect::<Vec<_>>(), vec![b]);
    let from = h.logs.len();
    oracle(&mut h, 19_845, 60);
    assert_eq!(take_pass(&mut h, from).fired.iter().map(|f| f.0).collect::<Vec<_>>(), vec![d]);
    check_structure(&mut h);
}

/// Liquidation / ADL cancel-all removes trailing orders in both states, with the caller's reason.
#[test]
fn cancel_all_removes_pending_and_active_trailing_orders() {
    for reason in [CancelReason::Liquidation, CancelReason::Adl] {
        let mut h = scenario(20_000);
        let u = user(ALICE);
        let a = place(&mut h, u, Side::Sell, 0, 100);
        let p = place(&mut h, u, Side::Sell, 20_500, 300);
        let other = place(&mut h, user(BOB), Side::Buy, 0, 100);
        let from = h.logs.len();
        let mk = market(&mut h);
        crate::risk::cancel_all_orders_for_market(&mut h, u, MKT, &mk, reason).unwrap();
        let got: BTreeSet<(Id, u8)> = h.logs[from..]
            .iter()
            .filter_map(ev::<AlgoOrderCancelled>)
            .map(|c| (c.algoId.0, c.reason))
            .collect();
        assert_eq!(got, [(a, reason as u8), (p, reason as u8)].into());
        let w = check_structure(&mut h);
        assert!(w.active[Side::Sell as usize].is_empty() && w.pending[Side::Sell as usize].is_empty());
        assert_eq!(w.active[Side::Buy as usize].keys().copied().collect::<Vec<_>>(), vec![other]);
    }
}

/// More than the shared budget fires on one tick: 50 go, `deferred_work` is set, and the drain —
/// same block, same mark — finishes them. The fired order closes against the book.
#[test]
fn a_budget_overflow_defers_to_the_drain() {
    let mut h = scenario(20_000);
    let mut ids = Vec::new();
    for i in 0..3 {
        let u = user(10 + i);
        seed_position(&mut h, u, POS, 20_000);
        for _ in 0..20 {
            ids.push(place(&mut h, u, Side::Sell, 0, 100));
        }
    }
    let from = h.logs.len();
    oracle(&mut h, 19_000, 15);
    let p = take_pass(&mut h, from);
    assert_eq!(p.fired.len(), 50);
    assert_eq!(market(&mut h).deferred_work, 1);
    let from = h.logs.len();
    assert!(!drain(&mut h));
    assert_eq!(take_pass(&mut h, from).fired.len(), 10);
    assert_eq!(market(&mut h).deferred_work, 0);
    check_structure(&mut h);
}

/// More than 64 activations due on one tick: 64 activate, the rest wait for the drain, which
/// activates them at the SAME seq and mark — so they share one stack element with the first 64.
#[test]
fn an_activation_overflow_defers_to_the_drain() {
    let mut h = scenario(20_000);
    let mut ids = Vec::new();
    for i in 0..3 {
        let u = user(10 + i);
        seed_position(&mut h, u, POS, 20_000);
        for _ in 0..25 {
            ids.push(place(&mut h, u, Side::Sell, 20_100, 100));
        }
    }
    let from = h.logs.len();
    oracle(&mut h, 20_200, 15);
    let p = take_pass(&mut h, from);
    assert_eq!(p.activations.len(), 64);
    assert_eq!(p.activations.iter().map(|a| a.0).collect::<Vec<_>>(), ids[..64].to_vec(), "FIFO");
    assert_eq!(market(&mut h).deferred_work, 1);
    let from = h.logs.len();
    assert!(!drain(&mut h));
    assert_eq!(take_pass(&mut h, from).activations.len(), 11);
    let sm = load_meta(&mut h, MKT).unwrap().sides[Side::Sell as usize];
    assert_eq!(sm.len - sm.base, 1, "one element for all 75");
    let w = check_structure(&mut h);
    assert!(w.active[Side::Sell as usize].values().all(|&(_, tag)| tag == 15));
}

/// Fixed triggers are served first from the shared budget, then trailing fires.
#[test]
fn fixed_triggers_spend_the_shared_budget_first() {
    let mut h = scenario(20_000);
    let u = user(10);
    seed_position(&mut h, u, POS, 20_000);
    let mut fixed = Vec::new();
    for _ in 0..30 {
        let out = crate::algo::run_place_algo_order(
            &placeAlgoOrderCall {
                algoType: 0,
                marketId: MKT,
                side: Side::Sell as u8,
                orderType: AlgoOrderType::StopMarket as u8,
                triggerPrice: 19_500,
                price: 0,
                quantity: 1,
                tif: 0,
                workingType: 0,
                clientAlgoId: FixedBytes::default(),
                flags: RO,
            }
            .abi_encode(),
            u,
            &mut h,
        )
        .unwrap();
        fixed.push(<Id>::try_from(&out[..32]).unwrap());
    }
    let u2 = user(11);
    seed_position(&mut h, u2, POS, 20_000);
    let trailing: Vec<Id> = (0..30).map(|_| place(&mut h, u2, Side::Sell, 0, 100)).collect();
    let from = h.logs.len();
    oracle(&mut h, 19_000, 15);
    let fired: Vec<Id> = take_pass(&mut h, from).fired.iter().map(|f| f.0).collect();
    assert_eq!(fired[..30], fixed[..]);
    assert_eq!(fired[30..], trailing[..20]);
    assert!(drain(&mut h) == false);
}

/// The not-latched rule: a fire the budget deferred and nobody drained is re-tested at the next
/// mark, and a bounce above the stop means it does not fire — it keeps trailing from the same peak.
#[test]
fn a_deferred_fire_that_nobody_drained_is_retested() {
    let mut h = scenario(20_000);
    let mut ids = Vec::new();
    for i in 0..3 {
        let u = user(10 + i);
        seed_position(&mut h, u, POS, 20_000);
        for _ in 0..20 {
            ids.push(place(&mut h, u, Side::Sell, 0, 100));
        }
    }
    oracle(&mut h, 19_000, 15);
    let from = h.logs.len();
    oracle(&mut h, 19_900, 30);
    assert!(take_pass(&mut h, from).fired.is_empty(), "19900 > 19800: the bounce saved them");
    let v = view(&mut h, &ids[59]);
    assert_eq!((v.activated, v.peakPrice), (true, 20_000));
}

/// `getAlgoOrder` on a fixed conditional reports the appended fields as zero.
#[test]
fn a_fixed_conditional_reports_no_trailing_fields() {
    let mut h = scenario(20_000);
    let out = crate::algo::run_place_algo_order(
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MKT,
            side: Side::Sell as u8,
            orderType: AlgoOrderType::TakeProfitMarket as u8,
            triggerPrice: 21_000,
            price: 0,
            quantity: 1,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO,
        }
        .abi_encode(),
        user(ALICE),
        &mut h,
    )
    .unwrap();
    let v = view(&mut h, &out[..32].try_into().unwrap());
    assert_eq!(v.orderType, AlgoOrderType::TakeProfitMarket as u8);
    assert_eq!((v.callbackRateBps, v.activated, v.peakPrice, v.currentStop), (0, false, 0, 0));
}

/// Placed before the first oracle tick: activated at seq 0 against the initial mark.
#[test]
fn an_order_placed_before_the_first_tick_seeds_at_seq_zero() {
    let mut h = scenario(20_000);
    let id = place(&mut h, user(ALICE), Side::Sell, 0, 100);
    let w = check_structure(&mut h);
    assert_eq!(w.active[Side::Sell as usize][&id], (100, 0));
    oracle(&mut h, 20_200, 15);
    assert_eq!(view(&mut h, &id).peakPrice, 20_200);
}

/// The signed path: the same 114-byte message, `orderType = 4`, `price` = the callback rate.
#[test]
fn the_signed_path_places_a_trailing_stop() {
    use ed25519_dalek::{Signer, SigningKey};
    let mut h = scenario(20_000);
    h.chain_id = 16_602;
    let u = user(ALICE);
    let sk = SigningKey::from_bytes(&[3u8; 32]);
    storage::save_api_key(&mut h, u, 0, ApiKey { pubkey: sk.verifying_key().to_bytes(), expiry: 0 })
        .unwrap();
    let (ts, recv) = (1u64, 60u64);
    let mut m = Vec::new();
    m.extend_from_slice(b"perpdex_v1_algo");
    m.extend_from_slice(&16_602u64.to_be_bytes());
    m.extend_from_slice(u.as_slice());
    m.push(0);
    m.extend_from_slice(&MKT.to_be_bytes());
    m.push(Side::Sell as u8);
    m.push(TRAIL);
    m.extend_from_slice(&0u64.to_be_bytes()); // activate now
    m.extend_from_slice(&250u64.to_be_bytes()); // 2.5 %
    m.extend_from_slice(&3u64.to_be_bytes());
    m.push(0);
    m.push(0);
    m.extend_from_slice(&[0u8; 16]);
    m.push(RO);
    m.extend_from_slice(&ts.to_be_bytes());
    m.extend_from_slice(&recv.to_be_bytes());
    m.push(0);
    assert_eq!(m.len(), 114);
    let sig = sk.sign(&m).to_bytes();
    let out = crate::algo::run_place_algo_order_signed(
        &placeAlgoOrderSignedCall {
            account: u,
            algoType: 0,
            marketId: MKT,
            side: Side::Sell as u8,
            orderType: TRAIL,
            triggerPrice: 0,
            price: 250,
            quantity: 3,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO,
            timestamp: ts,
            recvWindow: recv,
            keyId: 0,
            signature: sig.to_vec().into(),
        }
        .abi_encode(),
        &mut h,
    )
    .unwrap();
    let id: Id = out[..32].try_into().unwrap();
    let v = view(&mut h, &id);
    assert_eq!((v.orderType, v.callbackRateBps, v.activated, v.quantity), (TRAIL, 250, true, 3));
    assert_eq!(v.currentStop, 19_500);
}

/// The record carries the callback in its own trailing field, and `price` 0.
#[test]
fn the_order_record_round_trips_with_the_callback() {
    let mut h = scenario(20_000);
    let id = place(&mut h, user(ALICE), Side::Sell, 20_500, 330);
    let o = storage::load_order(&mut h, &id).unwrap().unwrap();
    assert_eq!((o.trail_bps, o.price, o.trigger_price, o.trigger_up), (330, 0, 20_500, true));
    assert!(o.reduce_only && !o.close_position && o.is_position_bound() && o.is_trailing());
    let enc = crate::storage::encode(&o).unwrap();
    let back: Order = crate::storage::decode(&enc).unwrap();
    assert_eq!(back, o);
    let plain = Order { trail_bps: 0, ..o.clone() };
    let enc0 = crate::storage::encode(&plain).unwrap();
    assert_eq!(*enc0.last().unwrap(), 0x00, "trail_bps is the trailing positional element");
}
