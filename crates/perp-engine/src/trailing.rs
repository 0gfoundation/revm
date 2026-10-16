//! Trailing stops (`TRAILING_STOP_MARKET`) — the grouped-lazy structure.
//!
//! # What is being computed
//!
//! A trailing SELL protects a long. Once ACTIVATED (the mark reached its activation price, or at
//! placement when none was given) it tracks `H`, the highest mark since activation, and fires when
//!
//! ```text
//! SELL:  10000 · mark  <=  H · (10000 − b)        BUY:  10000 · mark  >=  L · (10000 + b)
//! ```
//!
//! `b` is the callback rate in basis points (10..=1000, step 10) and `L` the lowest mark since
//! activation. The test is cross-multiplied in `u128`, so it is the real-valued Binance inequality
//! on the sampled mark with no rounding at all ([`fires`]). Equivalently `mark <= thr` with
//! `thr = floor(H(10000−b)/10000)` (SELL) / `mark >= thr` with `thr = ceil(L(10000+b)/10000)`
//! (BUY), which is exact because the mark is an integer ([`thr`]); the directory sorts on `thr`.
//! The tick-rounded stop ([`display_stop`]) is for events and the view only.
//!
//! The mark is sampled where every other conditional reads it: the STORED mark, written once per
//! oracle tick. A spike between two ticks is invisible (Binance tracks a continuous
//! `CONTRACT_PRICE` by default; we accept only `MARK_PRICE` — see the `placeAlgoOrder` docs).
//!
//! # Why not a peak per order
//!
//! Storing `H` on each order costs O(N) writes on every tick that makes a new high, and N is
//! unbounded: 32 per (user, market) times any number of accounts. Here a new extreme touches no
//! order. The observation: every activated SELL whose peak is `<=` a new mark gets peak exactly
//! that mark, and that set is always "the orders activated after some tick". So the peaks of ALL
//! activated orders of one side are encoded by one monotone stack.
//!
//! # The structure, per (market, side)
//!
//! ```text
//! seq      = IndexPriceState.timestamp of the stored mark (0 before the first oracle tick)
//! STACK    trls(page) — entries e[i] = (seq_i, peak_i), i in [base, len), 64 per page
//!            I1  seq strictly increases with i; peak strictly WORSENS with i
//!                (SELL: strictly decreases, BUY: strictly increases)
//!            I2  an activated order with tag τ (the seq it activated at) has
//!                    peak(τ) = e[lookup(τ)].peak,   lookup(τ) = first i in [base, len) with seq_i ≥ τ
//!                and that equals the best mark over [τ, now]
//! BUCKET   trlb(cb) — {head, tail} of a FIFO of the side's activated orders with callback cb,
//!          doubly linked through each order's trlo link {active, tag, prev, next}
//!            I3  tags are non-decreasing head → tail; every activated order is in exactly one
//!                bucket (its own cb), every link in a bucket says `active`
//! DIR      trld — one entry {cb, tag, elem, peak} per NON-EMPTY bucket, describing its HEAD:
//!            I4  tag = the head's tag, elem = lookup(tag), peak = e[elem].peak; sorted by
//!                (thr better-first, tag ascending, cb ascending)
//! META     trlm (both sides in one key) — base, len, the garbage-page range [gc_lo, gc_hi),
//!          a mirror of the top entry, and three caches:
//!            I5  dir_len = |DIR|; best_thr = thr(DIR[0]); max_front = max elem over DIR;
//!                base ≤ min elem over DIR; the stack is empty ⇔ DIR is empty;
//!                top_seq ≥ every live tag (so lookup always lands inside the stack)
//! PENDING  tpnp — distinct activation prices (SELL ascending, BUY descending), and per price a
//!          FIFO tpnl {head, tail} through the same trlo links with `active = false`;
//!          META caches the first price as pend_front (0 = none)
//! ```
//!
//! **Why only bucket FRONTS are indexed (L2).** Within a bucket tags are non-decreasing (I3), so
//! peaks are non-worsening toward the head and so is `thr`: if the head does not fire, nothing
//! behind it does. The directory therefore needs at most one entry per bucket — ≤ 100 — however
//! many orders there are, and the fire walk stops at the first front that does not fire.
//!
//! **Why a new activation or a fresh extreme cannot fire on its own tick (L4).** Its peak is the
//! current mark `m ≥ 1`, and `b ≥ 10` puts `thr` strictly on the far side of `m`. So the order of
//! the per-tick legs below changes no outcome, and activation can run after the fire walk.
//!
//! # Per-tick legs (`risk::run_deferred_sweeps`, in this order)
//!
//! 1. [`on_mark`] — stack update, NOT budgeted, idempotent at a fixed `(seq, mark)` (a drain
//!    re-runs it and no entry, front or cache moves; only the GC below makes progress). A mark
//!    that beats the top pops every entry it dominates by binary search and writes ONE slot; the
//!    directory is touched only if a front's element was popped (`max_front`). Then a bounded
//!    lazy GC of pages that left the live range.
//! 2. band GC, 3. fixed triggers (`algo::run_trigger_sweep`) — unchanged.
//! 4. [`run_fire_sweep`] — on the SAME 50-action budget as the fixed triggers and liquidations,
//!    SELL side first, then BUY; a separate call because the fixed sweep returns early when the
//!    budget runs out.
//! 5. [`run_activation_sweep`] — its OWN cap, [`MAX_TRAILING_ACTIVATIONS_PER_UPDATE`], so
//!    position-free crossings cannot eat the stop-loss and liquidation budget.
//! 6. liquidations.
//!
//! # Per-tick work bound (excluding `fire()` itself)
//!
//! `S` = live stack length of a side, `Pg = ⌈S/64⌉ + 1` its live pages.
//!
//! | event | trailing reads | trailing writes |
//! |---|---|---|
//! | nothing changes (no new extreme, nothing due) | 1 per leg (the meta key; 3 per tick) | 0 |
//! | new extreme, no front popped | + ≤ ⌈log2 Pg⌉ + 1 pages | the cut page, meta |
//! | new extreme, fronts popped | + directory | + directory (CPU O(100)) |
//! | lazy GC | 0 | ≤ 8 + 8 page deletes per side |
//! | each fire (≤ 50, shared with the fixed triggers) | ≤ 8 + ⌈log2 Pg⌉ + 1 pages | ≤ 5 |
//! | each activation (≤ 64) | ≤ 10 + the order and the position | ≤ 9 |
//! | any activation due on a side | + the pending price list, O(P) bytes | + ≤ 1 rewrite of it |
//!
//! (Pinned by `tests::per_fire_and_per_activation_cost_is_constant` and
//! `tests::decline_then_spike_costs_log_pages_not_n`.) None of it depends on N, the number of
//! trailing orders: the stack is the only per-side state that grows, and it is searched, never
//! scanned. So one tick costs at most ~2·(2 + ⌈log2 Pg⌉) + 50·(13 + ⌈log2 Pg⌉) + 64·19 + 32
//! trailing storage operations plus `fire()` itself — about 2k at a year-long stack — whatever N
//! is. The one term that is not constant in BYTES is the pending price list: 8 bytes per DISTINCT
//! pending activation price, read once and written at most once on a tick where an activation is
//! due (never on a quiet tick, `pend_front`). It is the fixed-trigger index's own shape
//! (`aupp`/`adnp`, which today's sweep reads in full on EVERY tick), reused here as specified;
//! every per-ORDER operation on it is O(1).
//!
//! `S` grows only by an activation at a mark strictly worse than the top — at most one entry per
//! tick per side, since one tick is one `seq` — and every new extreme shrinks it. An attacker who
//! activates one order per tick through an 18,000-tick decline buys an 18,000-entry stack (282
//! pages) for 18,000 placements, and the spike that pops it costs ≤ ⌈log2 283⌉ + 1 page reads and
//! three writes. The 281 pages it leaves are collected 8 per tick.
//!
//! # Fit with the not-latched rule
//!
//! The PEAK is latched (step 1 is never deferred), the FIRE is not: a fire the budget defers and
//! no drain finishes before the next tick is re-tested at the new mark, exactly like a fixed
//! trigger. An activation deferred past a tick re-tests too, and if it then activates its peak
//! starts at THAT tick's mark — a documented divergence of the same kind.

use crate::host::PerpHost;
use crate::interface::IPerpDex;
use crate::types::{CancelReason, Market, Order, Side};
use crate::{errors::perp_invariant_err, storage, PerpError, PERP_DEX_ADDRESS};
use alloy_primitives::IntoLogData;
use perp_core::keys::{
    trail_bucket_key, trail_dir_key, trail_link_key, trail_meta_key, trail_pending_level_key,
    trail_pending_prices_key, trail_stack_page_key,
};
use primitives::{Address, FixedBytes, Log, B256};

/// Smallest callback rate, in basis points: 0.1 % (Binance's algo-order minimum).
pub const CALLBACK_MIN_BPS: u16 = 10;
/// Largest callback rate: 10 %.
pub const CALLBACK_MAX_BPS: u16 = 1000;
/// Callback grid step: 0.1 %. With the bounds above it gives at most 100 buckets per side, which
/// is what bounds the directory (and every O(directory) step) by a constant.
pub const CALLBACK_STEP_BPS: u16 = 10;

/// Trailing activations (pending → activated, or pending → spent because the position ended) one
/// sweep may perform. Its OWN cap rather than the shared 50: activation needs no position to
/// arrange, so sharing would let a crowd of crossings starve stop-losses and liquidations. 64 is
/// `batch::MAX_BATCH_PLACE`, the same "orders one transaction may touch" unit the band GC uses.
pub const MAX_TRAILING_ACTIVATIONS_PER_UPDATE: u32 = 64;

/// Stack entries per page key.
const PAGE: u64 = 64;
/// Garbage pages deleted per region (below `base`, above `len`) per side per tick.
const GC_PAGES_PER_REGION: u32 = 8;
const BPS: u128 = 10_000;
const SIDES: [Side; 2] = [Side::Sell, Side::Buy];

// ── The predicate and its derived values ──────────────────────────────────────────────────────

/// `a` is STRICTLY more extreme than `b` for this side's trailed extreme: higher for SELL (which
/// trails the max), lower for BUY.
#[inline]
fn better(side: Side, a: u64, b: u64) -> bool {
    match side {
        Side::Sell => a > b,
        Side::Buy => a < b,
    }
}

/// THE firing predicate, cross-multiplied in `u128`: no rounding, the exact real-valued condition.
#[inline]
pub(crate) fn fires(side: Side, mark: u64, peak: u64, cb: u16) -> bool {
    let m = mark as u128 * BPS;
    match side {
        Side::Sell => m <= peak as u128 * (BPS - cb as u128),
        Side::Buy => m >= peak as u128 * (BPS + cb as u128),
    }
}

/// The integer threshold equivalent to [`fires`]: `mark <= thr` (SELL) / `mark >= thr` (BUY).
/// Exact because `mark` is an integer: `m ≤ X ⇔ m ≤ ⌊X⌋` and `m ≥ X ⇔ m ≥ ⌈X⌉`. A BUY peak is a
/// mark, so `≤ max_price ≤ i64::MAX` and `⌈1.1 · peak⌉` fits; the saturation is unreachable.
#[inline]
pub(crate) fn thr(side: Side, peak: u64, cb: u16) -> u64 {
    match side {
        Side::Sell => (peak as u128 * (BPS - cb as u128) / BPS) as u64,
        Side::Buy => {
            let x = (peak as u128 * (BPS + cb as u128)).div_ceil(BPS);
            u64::try_from(x).unwrap_or(u64::MAX)
        }
    }
}

#[inline]
fn fires_thr(side: Side, mark: u64, thr: u64) -> bool {
    match side {
        Side::Sell => mark <= thr,
        Side::Buy => mark >= thr,
    }
}

/// The stop as REPORTED (events, `getAlgoOrder`): [`thr`] rounded onto the tick grid away from the
/// mark — down for SELL, up for BUY. Since a mark is on the grid, `mark` passes this iff it passes
/// [`fires`] (the one exception, a mark clamped to an off-grid `max_price`, is why the decision
/// itself never reads this). Display only.
#[inline]
pub(crate) fn display_stop(side: Side, peak: u64, cb: u16, tick: u64) -> u64 {
    let t = thr(side, peak, cb);
    if tick == 0 {
        return t;
    }
    match side {
        Side::Sell => t / tick * tick,
        Side::Buy => t.div_ceil(tick).saturating_mul(tick),
    }
}

/// Does a pending order of `side` activate at `mark`? `mark >= A` for SELL, `mark <= A` for BUY —
/// `algo::trigger_fires` with the trailing activation direction, so placement's "would
/// immediately trigger" refusal and the sweep agree at the boundary.
#[inline]
fn activates(side: Side, activation: u64, mark: u64) -> bool {
    crate::algo::trigger_fires(side == Side::Sell, activation, mark)
}

/// Is `cb` on the callback grid?
#[inline]
pub(crate) fn valid_callback(cb: u64) -> bool {
    (CALLBACK_MIN_BPS as u64..=CALLBACK_MAX_BPS as u64).contains(&cb)
        && cb % CALLBACK_STEP_BPS as u64 == 0
}

// ── Raw byte access (counted under test) ──────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod ops {
    //! Per-thread counts of trailing-structure storage operations, for the work-bound tests.
    use core::cell::Cell;
    std::thread_local! {
        static READS: Cell<u64> = const { Cell::new(0) };
        static WRITES: Cell<u64> = const { Cell::new(0) };
        static DELETES: Cell<u64> = const { Cell::new(0) };
        static BYTES: Cell<u64> = const { Cell::new(0) };
    }
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Ops {
        pub reads: u64,
        pub writes: u64,
        pub deletes: u64,
        pub bytes: u64,
    }
    pub(crate) fn snapshot() -> Ops {
        Ops {
            reads: READS.with(Cell::get),
            writes: WRITES.with(Cell::get),
            deletes: DELETES.with(Cell::get),
            bytes: BYTES.with(Cell::get),
        }
    }
    pub(crate) fn since(before: Ops) -> Ops {
        let now = snapshot();
        Ops {
            reads: now.reads - before.reads,
            writes: now.writes - before.writes,
            deletes: now.deletes - before.deletes,
            bytes: now.bytes - before.bytes,
        }
    }
    pub(super) fn read(n: usize) {
        READS.with(|c| c.set(c.get() + 1));
        BYTES.with(|c| c.set(c.get() + n as u64));
    }
    pub(super) fn write(n: usize) {
        if n == 0 {
            DELETES.with(|c| c.set(c.get() + 1));
        } else {
            WRITES.with(|c| c.set(c.get() + 1));
        }
        BYTES.with(|c| c.set(c.get() + n as u64));
    }
}

fn rd<H: PerpHost>(context: &mut H, key: B256) -> Result<Vec<u8>, PerpError> {
    let buf = storage::load_blob(context, key)?;
    #[cfg(test)]
    ops::read(buf.len());
    Ok(buf)
}

fn wr<H: PerpHost>(context: &mut H, key: B256, buf: &[u8]) -> Result<(), PerpError> {
    #[cfg(test)]
    ops::write(buf.len());
    storage::store_blob(context, key, buf)
}

fn corrupt(what: &str) -> PerpError {
    perp_invariant_err(&format!("trailing: corrupt {what}"))
}

#[inline]
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().expect("8 bytes"))
}

#[inline]
fn id32(b: &[u8]) -> [u8; 32] {
    b[..32].try_into().expect("32 bytes")
}

// ── META ──────────────────────────────────────────────────────────────────────────────────────

/// One side's bookkeeping. Raw packed: ten `u64` BE.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SideMeta {
    /// Live stack range `[base, len)`. `base` never decreases.
    pub base: u64,
    pub len: u64,
    /// Every stored page lies in `[gc_lo, gc_hi)`; the ones outside the live range are garbage,
    /// deleted lazily by [`gc`].
    pub gc_lo: u64,
    pub gc_hi: u64,
    /// Mirror of `e[len − 1]` (meaningless while the stack is empty).
    pub top_seq: u64,
    pub top_peak: u64,
    /// Directory size, its first entry's threshold and its largest `elem` — so a tick where
    /// nothing fires and no front is popped never reads the directory.
    pub dir_len: u64,
    pub best_thr: u64,
    pub max_front: u64,
    /// The first pending activation price, 0 when nothing is pending — so a tick where nothing
    /// activates never reads the pending index.
    pub pend_front: u64,
}

impl SideMeta {
    #[inline]
    fn stack_empty(&self) -> bool {
        self.len == self.base
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Meta {
    /// Indexed by `Side as usize` (Buy 0, Sell 1).
    pub sides: [SideMeta; 2],
}

const SIDE_META_BYTES: usize = 80;

fn load_meta<H: PerpHost>(context: &mut H, market_id: u64) -> Result<Meta, PerpError> {
    let buf = rd(context, trail_meta_key(market_id))?;
    let mut m = Meta::default();
    if buf.is_empty() {
        return Ok(m);
    }
    if buf.len() != 2 * SIDE_META_BYTES {
        return Err(corrupt("meta"));
    }
    for (k, s) in m.sides.iter_mut().enumerate() {
        let f = |j: usize| be64(&buf[k * SIDE_META_BYTES + 8 * j..]);
        *s = SideMeta {
            base: f(0),
            len: f(1),
            gc_lo: f(2),
            gc_hi: f(3),
            top_seq: f(4),
            top_peak: f(5),
            dir_len: f(6),
            best_thr: f(7),
            max_front: f(8),
            pend_front: f(9),
        };
    }
    Ok(m)
}

fn save_meta<H: PerpHost>(context: &mut H, market_id: u64, m: &Meta) -> Result<(), PerpError> {
    // All-default (nothing pending, nothing activated, no stored page) deletes the key, so a market
    // whose trailing orders are all gone carries no trailing state at all.
    if *m == Meta::default() {
        return wr(context, trail_meta_key(market_id), &[]);
    }
    let mut buf = Vec::with_capacity(2 * SIDE_META_BYTES);
    for s in &m.sides {
        for v in [
            s.base, s.len, s.gc_lo, s.gc_hi, s.top_seq, s.top_peak, s.dir_len, s.best_thr,
            s.max_front, s.pend_front,
        ] {
            buf.extend_from_slice(&v.to_be_bytes());
        }
    }
    wr(context, trail_meta_key(market_id), &buf)
}

// ── STACK pages ───────────────────────────────────────────────────────────────────────────────

/// Pages read during one operation, so a binary search reads each page once.
struct Pages {
    market_id: u64,
    side: Side,
    cache: Vec<(u64, Vec<(u64, u64)>)>,
}

impl Pages {
    fn new(market_id: u64, side: Side) -> Self {
        Self { market_id, side, cache: Vec::new() }
    }

    fn page<H: PerpHost>(&mut self, context: &mut H, p: u64) -> Result<usize, PerpError> {
        if let Some(k) = self.cache.iter().position(|(q, _)| *q == p) {
            return Ok(k);
        }
        let buf = rd(context, trail_stack_page_key(self.market_id, self.side as u8, p))?;
        if buf.len() % 16 != 0 || buf.len() > 16 * PAGE as usize {
            return Err(corrupt("stack page"));
        }
        let v = buf.chunks_exact(16).map(|c| (be64(c), be64(&c[8..]))).collect();
        self.cache.push((p, v));
        Ok(self.cache.len() - 1)
    }

    fn get<H: PerpHost>(&mut self, context: &mut H, idx: u64) -> Result<(u64, u64), PerpError> {
        let k = self.page(context, idx / PAGE)?;
        self.cache[k]
            .1
            .get((idx % PAGE) as usize)
            .copied()
            .ok_or_else(|| corrupt("stack (live entry missing)"))
    }

    /// Write `e[idx] = entry` and DROP everything after it in the page: every write is either the
    /// new top or the cut point of a pop, so what follows `idx` is no longer live.
    fn set_top<H: PerpHost>(
        &mut self,
        context: &mut H,
        idx: u64,
        entry: (u64, u64),
    ) -> Result<(), PerpError> {
        let p = idx / PAGE;
        let o = (idx % PAGE) as usize;
        let k = if o == 0 {
            // A push that opens a page: whatever is stored there is garbage, so it is not read.
            match self.cache.iter().position(|(q, _)| *q == p) {
                Some(k) => k,
                None => {
                    self.cache.push((p, Vec::new()));
                    self.cache.len() - 1
                }
            }
        } else {
            self.page(context, p)?
        };
        let v = &mut self.cache[k].1;
        // Offsets below `o` in this page are live or below `base`; the latter may be missing once
        // the page was reused after a reset, and padding them is harmless (never read).
        v.resize(o, (0, 0));
        v.push(entry);
        let mut buf = Vec::with_capacity(16 * v.len());
        for (s, pk) in v.iter() {
            buf.extend_from_slice(&s.to_be_bytes());
            buf.extend_from_slice(&pk.to_be_bytes());
        }
        wr(context, trail_stack_page_key(self.market_id, self.side as u8, p), &buf)
    }

    /// First index in `[lo, hi)` whose entry fails `pred` (the entries satisfying it form a
    /// prefix), by binary search: ⌈log2(hi − lo)⌉ probes, ≈ ⌈log2 pages⌉ + 1 distinct page reads.
    fn partition_point<H: PerpHost>(
        &mut self,
        context: &mut H,
        mut lo: u64,
        mut hi: u64,
        pred: impl Fn((u64, u64)) -> bool,
    ) -> Result<u64, PerpError> {
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if pred(self.get(context, mid)?) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }
}

/// I2's `lookup`: the element an activated order with tag `tag` resolves to — the first entry at
/// or after `from` (a known lower bound) whose `seq ≥ tag`.
fn lookup<H: PerpHost>(
    context: &mut H,
    pages: &mut Pages,
    sm: &SideMeta,
    tag: u64,
    from: u64,
) -> Result<u64, PerpError> {
    let i = pages.partition_point(context, from.max(sm.base), sm.len, |(s, _)| s < tag)?;
    if i >= sm.len {
        // top_seq ≥ every live tag (I5), so this cannot happen.
        return Err(perp_invariant_err("trailing: a live tag is above the stack top"));
    }
    Ok(i)
}

// ── DIRECTORY ─────────────────────────────────────────────────────────────────────────────────

/// One bucket's head, as the directory sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Front {
    pub cb: u16,
    /// The head's activation tag.
    pub tag: u64,
    /// `lookup(tag)`.
    pub elem: u64,
    /// `e[elem].peak` — cached so the fire test and the sort need no page read.
    pub peak: u64,
}

const FRONT_BYTES: usize = 26;

/// Firing priority: the threshold the mark crosses first (highest for SELL, lowest for BUY), then
/// the older head, then the smaller callback. Unique per entry (one entry per `cb`).
fn front_key(side: Side, f: &Front) -> (u64, u64, u16) {
    let t = thr(side, f.peak, f.cb);
    let rank = match side {
        Side::Sell => u64::MAX - t,
        Side::Buy => t,
    };
    (rank, f.tag, f.cb)
}

fn load_dir<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
) -> Result<Vec<Front>, PerpError> {
    let buf = rd(context, trail_dir_key(market_id, side as u8))?;
    if buf.len() % FRONT_BYTES != 0 {
        return Err(corrupt("directory"));
    }
    Ok(buf
        .chunks_exact(FRONT_BYTES)
        .map(|c| Front {
            cb: u16::from_be_bytes([c[0], c[1]]),
            tag: be64(&c[2..]),
            elem: be64(&c[10..]),
            peak: be64(&c[18..]),
        })
        .collect())
}

/// Sort, store, and refresh the META caches (I5) from the directory as stored.
fn save_dir<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    dir: &mut [Front],
    sm: &mut SideMeta,
) -> Result<(), PerpError> {
    dir.sort_by_key(|f| front_key(side, f));
    let mut buf = Vec::with_capacity(dir.len() * FRONT_BYTES);
    for f in dir.iter() {
        buf.extend_from_slice(&f.cb.to_be_bytes());
        buf.extend_from_slice(&f.tag.to_be_bytes());
        buf.extend_from_slice(&f.elem.to_be_bytes());
        buf.extend_from_slice(&f.peak.to_be_bytes());
    }
    wr(context, trail_dir_key(market_id, side as u8), &buf)?;
    sm.dir_len = dir.len() as u64;
    sm.best_thr = dir.first().map_or(0, |f| thr(side, f.peak, f.cb));
    sm.max_front = dir.iter().map(|f| f.elem).max().unwrap_or(0);
    debug_assert!(
        dir.iter().all(|f| f.elem >= sm.base && f.elem < sm.len),
        "trailing: a front outside the live stack"
    );
    debug_assert!(
        dir.iter().all(|f| valid_callback(f.cb as u64)),
        "trailing: a directory entry off the callback grid"
    );
    Ok(())
}

/// Advance `base` to the oldest front (nothing below it is reachable by any live tag, because
/// every live tag is ≥ its bucket head's). With no front left the stack is empty: `base = len`,
/// rounded up to a page boundary so the partial page becomes ordinary low garbage.
fn prune(dir: &[Front], sm: &mut SideMeta) {
    match dir.iter().map(|f| f.elem).min() {
        Some(m) => {
            debug_assert!(m >= sm.base, "trailing: a front below base");
            sm.base = m;
        }
        None => {
            let b = sm.len.div_ceil(PAGE) * PAGE;
            sm.base = b;
            sm.len = b;
            sm.top_seq = 0;
            sm.top_peak = 0;
        }
    }
}

// ── QUEUES (buckets and pending levels) and their links ──────────────────────────────────────

/// `{head, tail}` of a FIFO; absent key = empty queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueueRec {
    head: [u8; 32],
    tail: [u8; 32],
}

/// A trailing order's position in its queue. `active`: in a callback bucket (`tag` = the seq it
/// activated at) rather than a pending level (`tag` = 0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Link {
    pub active: bool,
    pub tag: u64,
    pub prev: Option<[u8; 32]>,
    pub next: Option<[u8; 32]>,
}

const LINK_BYTES: usize = 1 + 8 + 32 + 32;

fn load_queue<H: PerpHost>(context: &mut H, key: B256) -> Result<Option<QueueRec>, PerpError> {
    let buf = rd(context, key)?;
    match buf.len() {
        0 => Ok(None),
        64 => Ok(Some(QueueRec { head: id32(&buf), tail: id32(&buf[32..]) })),
        _ => Err(corrupt("queue")),
    }
}

fn save_queue<H: PerpHost>(
    context: &mut H,
    key: B256,
    q: Option<QueueRec>,
) -> Result<(), PerpError> {
    match q {
        None => wr(context, key, &[]),
        Some(q) => {
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(&q.head);
            buf[32..].copy_from_slice(&q.tail);
            wr(context, key, &buf)
        }
    }
}

pub(crate) fn load_link<H: PerpHost>(
    context: &mut H,
    id: &[u8; 32],
) -> Result<Option<Link>, PerpError> {
    let buf = rd(context, trail_link_key(id))?;
    if buf.is_empty() {
        return Ok(None);
    }
    if buf.len() != LINK_BYTES || buf[0] & !0b111 != 0 {
        return Err(corrupt("link"));
    }
    let f = buf[0];
    Ok(Some(Link {
        active: f & 1 != 0,
        tag: be64(&buf[1..]),
        prev: (f & 2 != 0).then(|| id32(&buf[9..])),
        next: (f & 4 != 0).then(|| id32(&buf[41..])),
    }))
}

fn save_link<H: PerpHost>(context: &mut H, id: &[u8; 32], l: &Link) -> Result<(), PerpError> {
    let mut buf = [0u8; LINK_BYTES];
    buf[0] = u8::from(l.active) | u8::from(l.prev.is_some()) << 1 | u8::from(l.next.is_some()) << 2;
    buf[1..9].copy_from_slice(&l.tag.to_be_bytes());
    if let Some(p) = l.prev {
        buf[9..41].copy_from_slice(&p);
    }
    if let Some(n) = l.next {
        buf[41..73].copy_from_slice(&n);
    }
    wr(context, trail_link_key(id), &buf)
}

/// Append `id` at the tail. Returns whether the queue was empty. O(1): the queue record, the old
/// tail's link, the new link.
fn queue_push_back<H: PerpHost>(
    context: &mut H,
    key: B256,
    id: &[u8; 32],
    active: bool,
    tag: u64,
) -> Result<bool, PerpError> {
    let mut link = Link { active, tag, prev: None, next: None };
    let q = match load_queue(context, key)? {
        None => {
            save_link(context, id, &link)?;
            save_queue(context, key, Some(QueueRec { head: *id, tail: *id }))?;
            return Ok(true);
        }
        Some(q) => q,
    };
    let mut tail = load_link(context, &q.tail)?.ok_or_else(|| corrupt("queue tail link"))?;
    debug_assert!(tail.next.is_none(), "trailing: queue tail has a successor");
    debug_assert!(tail.active == active, "trailing: queue mixes pending and active links");
    debug_assert!(!active || tail.tag <= tag, "trailing: a bucket tag went backwards (I3)");
    tail.next = Some(*id);
    save_link(context, &q.tail, &tail)?;
    link.prev = Some(q.tail);
    save_link(context, id, &link)?;
    save_queue(context, key, Some(QueueRec { head: q.head, tail: *id }))?;
    Ok(false)
}

/// What [`queue_unlink`] changed at the head.
struct Unlinked {
    was_head: bool,
    /// The new head and its link, when `was_head` and the queue is not empty now.
    new_head: Option<([u8; 32], Link)>,
    now_empty: bool,
}

/// Remove `id` (whose link is `link`) from anywhere in the queue and delete its link. O(1).
fn queue_unlink<H: PerpHost>(
    context: &mut H,
    key: B256,
    id: &[u8; 32],
    link: &Link,
) -> Result<Unlinked, PerpError> {
    let mut q = load_queue(context, key)?.ok_or_else(|| corrupt("queue (unlink from empty)"))?;
    let mut new_head = None;
    match link.prev {
        Some(p) => {
            let mut pl = load_link(context, &p)?.ok_or_else(|| corrupt("prev link"))?;
            pl.next = link.next;
            save_link(context, &p, &pl)?;
        }
        None => {
            debug_assert_eq!(q.head, *id, "trailing: a link with no prev is not the head");
        }
    }
    match link.next {
        Some(n) => {
            let mut nl = load_link(context, &n)?.ok_or_else(|| corrupt("next link"))?;
            nl.prev = link.prev;
            save_link(context, &n, &nl)?;
            if link.prev.is_none() {
                new_head = Some((n, nl));
            }
        }
        None => {
            debug_assert_eq!(q.tail, *id, "trailing: a link with no next is not the tail");
        }
    }
    wr(context, trail_link_key(id), &[])?;
    let now_empty = link.prev.is_none() && link.next.is_none();
    if now_empty {
        save_queue(context, key, None)?;
    } else {
        if link.prev.is_none() {
            q.head = link.next.expect("non-empty");
        }
        if link.next.is_none() {
            q.tail = link.prev.expect("non-empty");
        }
        if link.prev.is_none() || link.next.is_none() {
            save_queue(context, key, Some(q))?;
        }
    }
    Ok(Unlinked { was_head: link.prev.is_none(), new_head, now_empty })
}

// ── PENDING index ─────────────────────────────────────────────────────────────────────────────

fn load_pending_prices<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
) -> Result<Vec<u64>, PerpError> {
    let buf = rd(context, trail_pending_prices_key(market_id, side as u8))?;
    if buf.len() % 8 != 0 {
        return Err(corrupt("pending prices"));
    }
    Ok(buf.chunks_exact(8).map(be64).collect())
}

fn save_pending_prices<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    prices: &[u64],
) -> Result<(), PerpError> {
    let mut buf = Vec::with_capacity(prices.len() * 8);
    for p in prices {
        buf.extend_from_slice(&p.to_be_bytes());
    }
    wr(context, trail_pending_prices_key(market_id, side as u8), &buf)
}

/// Position of `price` in a side's pending list: SELL ascending, BUY descending — the end that
/// activates first is the front.
fn pending_search(side: Side, prices: &[u64], price: u64) -> Result<usize, usize> {
    match side {
        Side::Sell => prices.binary_search(&price),
        Side::Buy => prices.binary_search_by(|p| price.cmp(p)),
    }
}

fn pending_insert<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    sm: &mut SideMeta,
    price: u64,
    id: &[u8; 32],
) -> Result<(), PerpError> {
    if queue_push_back(context, trail_pending_level_key(market_id, side as u8, price), id, false, 0)? {
        let mut prices = load_pending_prices(context, market_id, side)?;
        if let Err(i) = pending_search(side, &prices, price) {
            prices.insert(i, price);
        }
        save_pending_prices(context, market_id, side, &prices)?;
        sm.pend_front = prices[0];
    }
    Ok(())
}

fn pending_remove<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    sm: &mut SideMeta,
    price: u64,
    id: &[u8; 32],
    link: &Link,
) -> Result<(), PerpError> {
    let key = trail_pending_level_key(market_id, side as u8, price);
    if queue_unlink(context, key, id, link)?.now_empty {
        let mut prices = load_pending_prices(context, market_id, side)?;
        if let Ok(i) = pending_search(side, &prices, price) {
            prices.remove(i);
        }
        save_pending_prices(context, market_id, side, &prices)?;
        sm.pend_front = prices.first().copied().unwrap_or(0);
    }
    Ok(())
}

// ── Stack mutation ────────────────────────────────────────────────────────────────────────────

/// Step 1 for one side: fold the mark into the stack. A no-op unless `mark` is STRICTLY better
/// than the top's peak (`==` is the lazy case: the top already holds it). Otherwise every entry
/// whose peak the mark reaches is popped in ONE write — the first such entry becomes `(seq, mark)`
/// and the stack ends there — and every front whose element was popped now resolves to it.
///
/// I2 is kept: a tag that resolved to a popped entry now resolves to the cut, whose peak is the
/// mark, and its old peak was `≤` that mark; entries below the cut keep peaks strictly better than
/// the mark, which is still their best. Idempotent at a fixed `(seq, mark)`.
fn advance_stack<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    sm: &mut SideMeta,
    seq: u64,
    mark: u64,
) -> Result<(), PerpError> {
    if sm.stack_empty() || !better(side, mark, sm.top_peak) {
        return Ok(());
    }
    let mut pages = Pages::new(market_id, side);
    // The surviving entries are exactly those whose peak is strictly better than the mark — a
    // prefix (I1). The top is not among them, so the cut lands inside the stack.
    let i = pages.partition_point(context, sm.base, sm.len, |(_, p)| better(side, p, mark))?;
    debug_assert!(i < sm.len);
    debug_assert!(seq > sm.top_seq || sm.stack_empty(), "trailing: a new extreme at an old seq");
    pages.set_top(context, i, (seq, mark))?;
    sm.len = i + 1;
    sm.top_seq = seq;
    sm.top_peak = mark;
    if sm.dir_len > 0 && i <= sm.max_front {
        let mut dir = load_dir(context, market_id, side)?;
        for f in dir.iter_mut().filter(|f| f.elem >= i) {
            f.elem = i;
            f.peak = mark;
            // L4: a peak equal to the mark cannot fire at that mark.
            debug_assert!(!fires(side, mark, mark, f.cb), "trailing: L4 violated");
        }
        save_dir(context, market_id, side, &mut dir, sm)?;
        prune(&dir, sm);
    }
    Ok(())
}

/// The element an order activating NOW at `(seq, mark)` resolves to: the top, extended to `seq`
/// when it already holds this peak, else a new top. `advance_stack` first, which is a no-op on
/// every path that reaches here after the tick's step 1 (the top is then at least as good as the
/// mark), and keeps I1 on any path that did not.
fn ensure<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    sm: &mut SideMeta,
    seq: u64,
    mark: u64,
) -> Result<u64, PerpError> {
    advance_stack(context, market_id, side, sm, seq, mark)?;
    let mut pages = Pages::new(market_id, side);
    if !sm.stack_empty() {
        if sm.top_seq == seq {
            // The mark at a seq is unique, so a top at this seq holds this very mark.
            debug_assert_eq!(sm.top_peak, mark, "trailing: two marks at one seq");
            return Ok(sm.len - 1);
        }
        if sm.top_peak == mark {
            // Lazy-equal top: extend it to cover this tag (no live tag lies in (top_seq, seq)).
            pages.set_top(context, sm.len - 1, (seq, mark))?;
            sm.top_seq = seq;
            return Ok(sm.len - 1);
        }
        debug_assert!(better(side, sm.top_peak, mark), "trailing: ensure on an unpopped stack");
    }
    let idx = sm.len;
    pages.set_top(context, idx, (seq, mark))?;
    sm.len = idx + 1;
    sm.top_seq = seq;
    sm.top_peak = mark;
    sm.gc_hi = sm.gc_hi.max(sm.len.div_ceil(PAGE));
    Ok(idx)
}

/// Lazy GC: delete up to [`GC_PAGES_PER_REGION`] pages from each garbage region — below the page
/// holding `base`, and at or above the first page past `len` — and reset a side that holds nothing
/// at all, so its indices start from 0 again.
fn gc<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    sm: &mut SideMeta,
) -> Result<(), PerpError> {
    let live_lo = sm.base / PAGE;
    let live_hi = sm.len.div_ceil(PAGE).max(live_lo);
    let mut n = GC_PAGES_PER_REGION;
    while n > 0 && sm.gc_lo < live_lo {
        wr(context, trail_stack_page_key(market_id, side as u8, sm.gc_lo), &[])?;
        sm.gc_lo += 1;
        n -= 1;
    }
    let mut n = GC_PAGES_PER_REGION;
    while n > 0 && sm.gc_hi > live_hi.max(sm.gc_lo) {
        sm.gc_hi -= 1;
        wr(context, trail_stack_page_key(market_id, side as u8, sm.gc_hi), &[])?;
        n -= 1;
    }
    if sm.stack_empty() && sm.dir_len == 0 && sm.pend_front == 0 && sm.gc_lo >= sm.gc_hi {
        *sm = SideMeta::default();
    }
    Ok(())
}

// ── Activation and removal ────────────────────────────────────────────────────────────────────

/// Put an order into its callback bucket at tag `seq` with peak `mark`, and say so.
#[allow(clippy::too_many_arguments)]
fn activate<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    sm: &mut SideMeta,
    id: &[u8; 32],
    owner: Address,
    cb: u16,
    seq: u64,
    mark: u64,
    tick: u64,
) -> Result<(), PerpError> {
    let idx = ensure(context, market_id, side, sm, seq, mark)?;
    if queue_push_back(context, trail_bucket_key(market_id, side as u8, cb), id, true, seq)? {
        let mut dir = load_dir(context, market_id, side)?;
        debug_assert!(dir.iter().all(|f| f.cb != cb), "trailing: an empty bucket in the directory");
        dir.push(Front { cb, tag: seq, elem: idx, peak: mark });
        save_dir(context, market_id, side, &mut dir, sm)?;
        prune(&dir, sm);
    }
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AlgoOrderActivated {
            user: owner,
            marketId: market_id,
            algoId: FixedBytes(*id),
            markPrice: mark,
            stopPrice: display_stop(side, mark, cb, tick),
        }
        .to_log_data(),
    });
    Ok(())
}

/// Take an ACTIVATED order out of its bucket (a cancel anywhere in the queue, or the fire walk
/// popping a head) and repair the directory if the head changed.
fn unlink_active<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    sm: &mut SideMeta,
    id: &[u8; 32],
    link: &Link,
    cb: u16,
) -> Result<(), PerpError> {
    let u = queue_unlink(context, trail_bucket_key(market_id, side as u8, cb), id, link)?;
    if !u.was_head {
        return Ok(()); // the front is unchanged
    }
    let mut dir = load_dir(context, market_id, side)?;
    let k = dir
        .iter()
        .position(|f| f.cb == cb)
        .ok_or_else(|| corrupt("directory (non-empty bucket missing)"))?;
    match u.new_head {
        None => {
            debug_assert!(u.now_empty);
            dir.remove(k);
        }
        Some((_, hl)) => {
            let f = &mut dir[k];
            debug_assert!(hl.active && hl.tag >= f.tag, "trailing: I3 at refront");
            if hl.tag != f.tag {
                let mut pages = Pages::new(market_id, side);
                f.elem = lookup(context, &mut pages, sm, hl.tag, f.elem)?;
                f.peak = pages.get(context, f.elem)?.1;
                f.tag = hl.tag;
            }
        }
    }
    save_dir(context, market_id, side, &mut dir, sm)?;
    prune(&dir, sm);
    Ok(())
}

// ── Entry points ──────────────────────────────────────────────────────────────────────────────

/// Index a freshly placed trailing order (its record and `ualg` entry are already written, and
/// `AlgoOrderPlaced` emitted). `activate_at = Some(seq)` activates it now at the stored mark —
/// Binance's "no activatePrice = the latest price"; `None` queues it at `order.trigger_price`.
pub(crate) fn place<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &Market,
    id: &[u8; 32],
    order: &Order,
    activate_at: Option<u64>,
) -> Result<(), PerpError> {
    let side = order.side;
    let mut meta = load_meta(context, market_id)?;
    let sm = &mut meta.sides[side as usize];
    match activate_at {
        Some(seq) => activate(
            context,
            market_id,
            side,
            sm,
            id,
            Address::from(order.owner),
            order.trail_bps,
            seq,
            market.mark_price,
            market.tick_size,
        )?,
        None => pending_insert(context, market_id, side, sm, order.trigger_price, id)?,
    }
    save_meta(context, market_id, &meta)
}

/// Remove a trailing order from whichever structure holds it (cancel, liquidation / ADL
/// cancel-all). Idempotent: an order already out of the structure has no link.
pub(crate) fn remove<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    id: &[u8; 32],
    order: &Order,
) -> Result<(), PerpError> {
    let Some(link) = load_link(context, id)? else {
        return Ok(());
    };
    let side = order.side;
    let mut meta = load_meta(context, market_id)?;
    let sm = &mut meta.sides[side as usize];
    if link.active {
        unlink_active(context, market_id, side, sm, id, &link, order.trail_bps)?;
    } else {
        pending_remove(context, market_id, side, sm, order.trigger_price, id, &link)?;
    }
    save_meta(context, market_id, &meta)
}

/// Step 1 of the per-tick legs: fold the stored mark into both sides' stacks, then the lazy GC.
/// Unbudgeted and idempotent at a fixed `(seq, mark)`; one meta read when nothing moves.
pub(crate) fn on_mark<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    seq: u64,
    mark: u64,
) -> Result<(), PerpError> {
    let mut meta = load_meta(context, market_id)?;
    if meta == Meta::default() {
        return Ok(());
    }
    let before = meta;
    for side in SIDES {
        let sm = &mut meta.sides[side as usize];
        advance_stack(context, market_id, side, sm, seq, mark)?;
        gc(context, market_id, side, sm)?;
    }
    if meta != before {
        save_meta(context, market_id, &meta)?;
    }
    Ok(())
}

/// The trailing fire walk, on the shared sweep budget: per side (SELL, then BUY) pop the
/// directory's first front while it fires. Returns whether the budget stopped it with a firing
/// front left.
pub(crate) fn run_fire_sweep<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &Market,
    mark: u64,
    budget: &mut u32,
) -> Result<bool, PerpError> {
    let mut deferred = false;
    let mut meta = load_meta(context, market_id)?;
    for side in SIDES {
        loop {
            let sm = &mut meta.sides[side as usize];
            if sm.dir_len == 0 || !fires_thr(side, mark, sm.best_thr) {
                break; // L2: the best front does not fire, so nothing on this side does
            }
            if *budget == 0 {
                deferred = true;
                break;
            }
            let f = *load_dir(context, market_id, side)?
                .first()
                .ok_or_else(|| corrupt("directory (empty, dir_len > 0)"))?;
            debug_assert!(fires(side, mark, f.peak, f.cb), "trailing: thr and fires disagree");
            let q = load_queue(context, trail_bucket_key(market_id, side as u8, f.cb))?
                .ok_or_else(|| corrupt("bucket (front without queue)"))?;
            let id = q.head;
            let link = load_link(context, &id)?.ok_or_else(|| corrupt("head link"))?;
            unlink_active(context, market_id, side, sm, &id, &link, f.cb)?;
            save_meta(context, market_id, &meta)?;
            *budget -= 1;
            let stop = display_stop(side, f.peak, f.cb, market.tick_size);
            match storage::load_order(context, &id)? {
                Some(order) => {
                    crate::algo::fire_trailing(context, market_id, market, mark, &id, &order, stop)?
                }
                // Every removal path unlinks, so the structure never holds a dangling id.
                None => debug_assert!(false, "trailing: dangling id in a bucket"),
            }
            // Re-read rather than trust the copy across `fire`: it runs the whole ordinary
            // placement path, and nothing there may be assumed not to reach this structure.
            meta = load_meta(context, market_id)?;
        }
    }
    Ok(deferred)
}

/// The activation walk, on its own `cap`: per side (SELL, then BUY) take pending orders from the
/// front of the price index while their activation price is reached. Each one either activates at
/// `(seq, mark)` or — its position having ended since placement — is spent with
/// `PositionClosed`. Returns whether the cap stopped it with an activation still due.
pub(crate) fn run_activation_sweep<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &Market,
    seq: u64,
    mark: u64,
    cap: &mut u32,
) -> Result<bool, PerpError> {
    let mut meta = load_meta(context, market_id)?;
    let before = meta;
    let mut deferred = false;
    for side in SIDES {
        let sm = &mut meta.sides[side as usize];
        if sm.pend_front == 0 || !activates(side, sm.pend_front, mark) {
            continue;
        }
        let prices = load_pending_prices(context, market_id, side)?;
        let mut emptied = 0usize;
        'levels: for &price in prices.iter() {
            if !activates(side, price, mark) {
                break;
            }
            let key = trail_pending_level_key(market_id, side as u8, price);
            loop {
                if *cap == 0 {
                    deferred = true;
                    break 'levels;
                }
                let q = load_queue(context, key)?.ok_or_else(|| corrupt("pending level"))?;
                let id = q.head;
                let link = load_link(context, &id)?.ok_or_else(|| corrupt("pending link"))?;
                let u = queue_unlink(context, key, &id, &link)?;
                *cap -= 1;
                let order = storage::load_order(context, &id)?
                    .ok_or_else(|| corrupt("pending id without order"))?;
                let owner = Address::from(order.owner);
                let pos = storage::load_position_ref(context, owner, market_id)?;
                if pos.generation != order.position_generation {
                    // Bound to a position that has ended (Order::is_position_bound): it would
                    // only ever trail a position it was not placed against.
                    crate::algo::retire_algo_order(
                        context,
                        owner,
                        market_id,
                        &id,
                        CancelReason::PositionClosed,
                    )?;
                } else {
                    activate(
                        context,
                        market_id,
                        side,
                        sm,
                        &id,
                        owner,
                        order.trail_bps,
                        seq,
                        mark,
                        market.tick_size,
                    )?;
                }
                if u.now_empty {
                    emptied += 1;
                    continue 'levels;
                }
            }
        }
        if emptied > 0 {
            save_pending_prices(context, market_id, side, &prices[emptied..])?;
            sm.pend_front = prices.get(emptied).copied().unwrap_or(0);
        }
    }
    if meta != before {
        save_meta(context, market_id, &meta)?;
    }
    Ok(deferred)
}

/// What `getAlgoOrder` reports for a trailing order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TrailView {
    pub activated: bool,
    /// Best mark since activation (0 while pending).
    pub peak: u64,
    /// [`display_stop`] of that peak (0 while pending).
    pub stop: u64,
}

/// O(log S) view of one trailing order.
pub(crate) fn view<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    id: &[u8; 32],
    order: &Order,
    tick: u64,
) -> Result<TrailView, PerpError> {
    let Some(link) = load_link(context, id)? else {
        return Ok(TrailView::default());
    };
    if !link.active {
        return Ok(TrailView::default());
    }
    let side = order.side;
    let meta = load_meta(context, market_id)?;
    let sm = &meta.sides[side as usize];
    let mut pages = Pages::new(market_id, side);
    let i = lookup(context, &mut pages, sm, link.tag, sm.base)?;
    let peak = pages.get(context, i)?.1;
    Ok(TrailView { activated: true, peak, stop: display_stop(side, peak, order.trail_bps, tick) })
}

#[cfg(test)]
#[path = "trailing_tests.rs"]
mod tests;

/// The `fire()` capacity fix, closePosition parity and trailing stops, written independently from
/// the spec rather than from this implementation (or its `tests`).
#[cfg(test)]
#[path = "trailing_spec_tests.rs"]
mod spec_tests;
