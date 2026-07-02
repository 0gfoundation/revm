//! Parallel place/cancel execution for the PerpDEX precompile (catalog #21 spike, step 3c).
//!
//! This module is the perp-specific brain of the parallel matching driver: given a decoded order
//! and the current BBO, it decides how the gated phase must treat the operation. The thread pool,
//! per-slot context, and the AccountGate/BboTicketLock wiring live in the driver; this file holds
//! the pure decision so it can be reasoned about and tested without any concurrency.
//!
//! Decisions (encoding the scheme's rules 2–5 + the review's refinements):
//! - **`RejectInBody`** — a PostOnly limit that crosses: the precompile body self-rejects (revert);
//!   handled entirely in 3c, NOT a downgrade.
//! - **`DowngradeToBarrier`** — anything that must run on the serial barrier (step 3d): a matching
//!   limit (GTC) that crosses (it would take liquidity), an IOC/FOK or Market (taker semantics), or
//!   a cancel that EMPTIES the best level (the new BBO must be recomputed by scanning the book,
//!   which requires all lower-txn_id inserts to have landed — only the all-lower-done barrier
//!   guarantees that). 3c only marks these; the serial re-run is 3d.
//! - **`HoldTicket`** — a non-crossing rest that MOVES the BBO (improves the best on its side): the
//!   margin check + insert + best-update must complete while holding the BBO ticket (rule 5).
//! - **`ReleaseTicket`** — a non-crossing rest that does NOT move the BBO (rests below the best), or
//!   a cancel that does not empty the best level: release the BBO ticket after the decision and do
//!   the account/level mutation under the per-level lock, in parallel (rule 4.iii).
//!
//! `best == 0` is the "no order on this side" sentinel (an empty book side).

use crate::perp_dex::interface::IPerpDex::{
    cancelOrderCall, cancelOrderSignedCall, placeOrderCall, placeOrderSignedCall,
};
use crate::perp_dex::types::order::{OrderType, Side, TimeInForce};
use crate::perp_dex::{
    encode_revert_string, storage,
    trading::{
        cancel_order_core, compute_order_id, decode_cancel_order, decode_place_order,
        decode_place_order_pending, encode_place_order_id, place_order_core,
        verify_cancel_order_signed, verify_place_order_signed, CancelParams, PlaceParams,
    },
};
use crate::PrecompileError;
use alloy_sol_types::SolCall;
use context::journal::perp_pool::PerpPool;
use context::journal::perp_sched::{AccountGate, BboTicketLock, BookSideLock, MarketCompletion};
use context::journal::shared_perp::SharedPerpBook;
use context::journaled_state::JournalCheckpoint;
use context::{ContextTr, JournalTr};
use primitives::{Address, Log};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

/// How the gated phase must treat one place/cancel under the BBO ticket. See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockPlan {
    /// PostOnly limit that crosses → the body self-rejects (revert). 3c handles it; no barrier.
    RejectInBody,
    /// Must run on the serial barrier (3d): crossing matching-limit / IOC / FOK / Market / a cancel
    /// that empties the best level.
    DowngradeToBarrier,
    /// Non-crossing rest that moves the BBO → execute fully while holding the BBO ticket.
    HoldTicket,
    /// Non-crossing rest below the best, or a cancel that doesn't empty the best level → release the
    /// ticket and mutate under the per-level lock (parallel).
    ReleaseTicket,
}

/// Per-block parallel-execution profile (catalog #21, `parallel-instrumentation-spec.md`). A struct of
/// atomics, created once per block by the driver, shared (Arc) into every pool slot so each op can bump
/// it lock-free. Near-zero cost when the driver doesn't create one (the `Option<&PerpBlockProfile>` in
/// `BatchEnv` is `None`). The node reads it at block-end and emits the `PERP_PROF …` line.
///
/// §1 achieved concurrency (Little's law): `avg_concurrency = sum_body_ns / phase_wall_ns`; `max_conc`
/// is the in-flight high-water. §A histogram = the block's op shape. §H = block totals.
#[derive(Debug, Default)]
pub struct PerpBlockProfile {
    // §A classification histogram (bumped at the classify decision).
    pub n_mover: AtomicU64,        // HoldTicket
    pub n_release: AtomicU64,      // ReleaseTicket (non-mover, parallel body)
    pub n_inline_taker: AtomicU64, // DowngradeToBarrier (crossing taker, inline under ticket)
    pub n_reject: AtomicU64,       // RejectInBody (PostOnly cross)
    pub n_cancel: AtomicU64,       // cancel ops (coarse)
    // §1 achieved concurrency.
    pub sum_body_ns: AtomicU64,    // Σ per-op body wall-time (may exceed phase_wall — that IS concurrency)
    pub inflight: AtomicUsize,     // transient in-flight body counter
    pub max_concurrency: AtomicUsize, // high-water of `inflight`
    // §H block totals (phase_wall + n_ops set by the driver around run_batch).
    pub phase_wall_ns: AtomicU64,
    pub n_ops: AtomicU64,
    // §B BBO-ticket wait vs held. `bbo.run(ticket, f)` = wait (blocked on the serve cursor) + held
    // (running `f`: the 2 best-reads + classify + any inline mover/taker body). The ticket is a strict
    // serial cursor (only one op holds it at a time), so Σ`bbo_held_ns` intervals do NOT overlap →
    // it is a lower bound on the phase's SERIAL critical path; `bbo_held/phase` ≈ the serial-ticket
    // fraction (spec §B/§7 top row). `bbo_wait_ns` is summed across parallel workers → overlaps (a
    // queue-depth proxy, not wall time).
    pub bbo_wait_ns: AtomicU64,
    pub bbo_held_ns: AtomicU64,
    // §C per-op cost (subset): Σ time doing the 2 best-reads (`load_best_bid`+`load_best_ask`) +
    // `classify_place`, i.e. the classify work that runs under the ticket for EVERY op.
    pub classify_ns: AtomicU64,
    // §B producer-side serve-cursor handoff: Σ time in `ServeAdvance::drop` (lock serve + advance +
    // `notify_all`), read from the BboTicketLock at block-end. Isolates the notify machinery from the
    // consumer-side `bbo_wait` (condvar wake + re-check). Set by the driver, not per-op.
    pub advance_notify_ns: AtomicU64,
    // §B rest — the other sched-lock waits, harvested from the per-block locks at block-end. All ~0 on
    // maxParallel (accounts ≫ ops, distinct prices, no crossers) → confirms the constraint is the BBO
    // cursor, NOT account/level/taker contention.
    pub acct_wait_ns: AtomicU64,       // AccountGate: same-account serialization
    pub book_wait_ns: AtomicU64,       // BookSideLock: same-(market,side,price) contention
    pub completion_wait_ns: AtomicU64, // MarketCompletion: inline-taker / emptying-cancel wait (crossers)
    // §F BookSideLock granularity: new level-lock entries (first touch of a price) vs hits. inserts ≈
    // ops with book_wait ≈ 0 ⇒ the per-price lock is pure per-op overhead on scattered-price flow.
    pub level_inserts: AtomicU64,
    pub level_hits: AtomicU64,
}

impl PerpBlockProfile {
    /// Mark a body entering the concurrent region: bump in-flight + push the high-water mark.
    fn body_enter(&self) {
        let now = self.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        self.max_concurrency.fetch_max(now, Ordering::Relaxed);
    }
    /// Mark a body leaving: accumulate its wall-time and decrement in-flight.
    fn body_exit(&self, ns: u64) {
        self.sum_body_ns.fetch_add(ns, Ordering::Relaxed);
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }
    fn bump_plan(&self, plan: LockPlan) {
        match plan {
            LockPlan::HoldTicket => &self.n_mover,
            LockPlan::ReleaseTicket => &self.n_release,
            LockPlan::DowngradeToBarrier => &self.n_inline_taker,
            LockPlan::RejectInBody => &self.n_reject,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    /// Format the one-line `PERP_PROF …` block record the node emits to reth.log. `blk` + the
    /// prephase timings (§D) are supplied by the node — they live outside the parallel driver — while
    /// everything else is read from the block-local atomics. Fields: §H totals (`ops`, `phase_ms`),
    /// §1 achieved concurrency (`concurrency_avg = sum_body_ns / phase_wall_ns` by Little's law,
    /// `max_inflight` = the in-flight high-water), §A classification histogram (`n_mover` …
    /// `n_cancel`), §D prephase (`scan_ms` = serial classify scan, `prephase_ms` = total pre-phase).
    pub fn format_prof_line(&self, blk: u64, prephase_total_ns: u64, scan_ns: u64) -> String {
        let phase = self.phase_wall_ns.load(Ordering::Relaxed);
        let body = self.sum_body_ns.load(Ordering::Relaxed);
        let conc = if phase > 0 { body as f64 / phase as f64 } else { 0.0 };
        let ms = |ns: u64| ns as f64 / 1e6;
        format!(
            "PERP_PROF blk={blk} ops={} concurrency_avg={conc:.2} max_inflight={} \
             phase_ms={:.3} body_ms_sum={:.3} prephase_ms={:.3} scan_ms={:.3} \
             bbo_held_ms={:.3} bbo_wait_ms={:.3} classify_ms={:.3} advance_notify_ms={:.3} \
             acct_wait_ms={:.3} book_wait_ms={:.3} completion_wait_ms={:.3} level_inserts={} level_hits={} \
             n_mover={} n_release={} n_inline_taker={} n_reject={} n_cancel={}",
            self.n_ops.load(Ordering::Relaxed),
            self.max_concurrency.load(Ordering::Relaxed),
            ms(phase),
            ms(body),
            ms(prephase_total_ns),
            ms(scan_ns),
            ms(self.bbo_held_ns.load(Ordering::Relaxed)),
            ms(self.bbo_wait_ns.load(Ordering::Relaxed)),
            ms(self.classify_ns.load(Ordering::Relaxed)),
            ms(self.advance_notify_ns.load(Ordering::Relaxed)),
            ms(self.acct_wait_ns.load(Ordering::Relaxed)),
            ms(self.book_wait_ns.load(Ordering::Relaxed)),
            ms(self.completion_wait_ns.load(Ordering::Relaxed)),
            self.level_inserts.load(Ordering::Relaxed),
            self.level_hits.load(Ordering::Relaxed),
            self.n_mover.load(Ordering::Relaxed),
            self.n_release.load(Ordering::Relaxed),
            self.n_inline_taker.load(Ordering::Relaxed),
            self.n_reject.load(Ordering::Relaxed),
            self.n_cancel.load(Ordering::Relaxed),
        )
    }
}

/// RAII timer for one parallel body: on construct bumps in-flight + starts an `Instant`; on drop
/// accumulates elapsed ns into `sum_body_ns` and decrements in-flight. `None` = no-op (profiling off).
/// Wraps each `gated_*` call so `sum_body_ns / phase_wall_ns` = achieved concurrency (§1).
struct BodyTimer<'a> {
    prof: Option<&'a PerpBlockProfile>,
    start: Option<Instant>,
}
impl<'a> BodyTimer<'a> {
    fn new(prof: Option<&'a PerpBlockProfile>) -> Self {
        if let Some(p) = prof {
            p.body_enter();
        }
        BodyTimer {
            prof,
            start: prof.map(|_| Instant::now()),
        }
    }
}
impl Drop for BodyTimer<'_> {
    fn drop(&mut self) {
        if let (Some(p), Some(s)) = (self.prof, self.start) {
            p.body_exit(s.elapsed().as_nanos() as u64);
        }
    }
}

/// Decide how to handle a place order given the current BBO. Pure; the driver supplies the BBO it
/// read under the ticket. `best_bid`/`best_ask` are the cached bests (0 = that side is empty).
pub fn classify_place(
    side: Side,
    order_type: OrderType,
    tif: TimeInForce,
    price: u64,
    best_bid: u64,
    best_ask: u64,
) -> LockPlan {
    // Only resting limit orders are parallel-eligible; market + taker TIFs take liquidity → barrier.
    if !matches!(order_type, OrderType::Limit) {
        return LockPlan::DowngradeToBarrier;
    }
    if matches!(tif, TimeInForce::Ioc | TimeInForce::Fok) {
        return LockPlan::DowngradeToBarrier;
    }
    // Cross against the opposite best (0 = that side is empty → cannot cross).
    let crosses = match side {
        Side::Buy => best_ask != 0 && price >= best_ask,
        Side::Sell => best_bid != 0 && price <= best_bid,
    };
    if crosses {
        return match tif {
            // PostOnly that would cross is rejected by the body (revert), not matched.
            TimeInForce::PostOnly => LockPlan::RejectInBody,
            // A matching limit (GTC) that crosses takes liquidity → serial barrier.
            _ => LockPlan::DowngradeToBarrier,
        };
    }
    // Non-crossing rest: does it improve (move) the best on its own side?
    let moves_bbo = match side {
        Side::Buy => best_bid == 0 || price > best_bid,
        Side::Sell => best_ask == 0 || price < best_ask,
    };
    if moves_bbo {
        LockPlan::HoldTicket
    } else {
        LockPlan::ReleaseTicket
    }
}

/// Whether a cancel sits AT the best on its own side. Pure; the driver supplies the BBO it read
/// under the ticket. A resting order's price is always at-or-inside the side's best (bids: best_bid
/// is the MAX bid ≥ any bid price; asks: best_ask is the MIN ask ≤ any ask price), so the only two
/// cases are "at best" (`==`) and "below/worse than best".
///
/// - **below best** → removing it cannot move the BBO → remove in parallel (ReleaseTicket-style).
/// - **at best** → removing it MIGHT move the BBO (only if it is the level's last order). That can't
///   be decided from the cached BBO alone: it needs the level membership AS OF all lower-txn_id ops
///   at this price. So the driver waits ([`PriceCompletion::wait_for`]) for those, then re-reads the
///   level under the book-side lock: if others remain → remove in parallel; if it is the sole order
///   → the BBO moves → defer to the serial barrier (3d).
///
/// During the parallel phase the best is monotone (only movers — which IMPROVE it — and downgraded
/// best-worseners run), so a below-best cancel can never become at-best, and an at-best cancel can
/// only drift below best (handled identically: it just won't empty the *current* best level).
pub fn cancel_at_best(side: Side, price: u64, best_bid: u64, best_ask: u64) -> bool {
    match side {
        Side::Buy => price == best_bid,
        Side::Sell => price == best_ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── place: matching limit (GTC) ───────────────────────────────────────────
    #[test]
    fn gtc_buy_below_best_ask_rests() {
        // best_bid=100, best_ask=110; buy at 105 < 110 → non-cross, and 105 > 100 → moves best_bid.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 105, 100, 110),
            LockPlan::HoldTicket
        );
        // buy at 90 < best_bid 100 → non-cross, doesn't move best_bid → release.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 90, 100, 110),
            LockPlan::ReleaseTicket
        );
    }

    #[test]
    fn gtc_buy_at_or_above_best_ask_downgrades() {
        // buy at 110 >= best_ask 110 → crosses → matches → downgrade.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 110, 100, 110),
            LockPlan::DowngradeToBarrier
        );
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 115, 100, 110),
            LockPlan::DowngradeToBarrier
        );
    }

    #[test]
    fn gtc_sell_mirrors_buy() {
        // sell at 105 > best_bid 100 → non-cross; 105 < best_ask 110 → moves best_ask → hold.
        assert_eq!(
            classify_place(
                Side::Sell,
                OrderType::Limit,
                TimeInForce::Gtc,
                105,
                100,
                110
            ),
            LockPlan::HoldTicket
        );
        // sell at 120 > best_ask 110 → non-cross, doesn't move best_ask → release.
        assert_eq!(
            classify_place(
                Side::Sell,
                OrderType::Limit,
                TimeInForce::Gtc,
                120,
                100,
                110
            ),
            LockPlan::ReleaseTicket
        );
        // sell at 100 <= best_bid 100 → crosses → downgrade.
        assert_eq!(
            classify_place(
                Side::Sell,
                OrderType::Limit,
                TimeInForce::Gtc,
                100,
                100,
                110
            ),
            LockPlan::DowngradeToBarrier
        );
    }

    // ── place: PostOnly ────────────────────────────────────────────────────────
    #[test]
    fn postonly_cross_rejects_not_downgrades() {
        // PostOnly buy at 110 >= best_ask 110 → crosses → REJECT (body reverts), not downgrade.
        assert_eq!(
            classify_place(
                Side::Buy,
                OrderType::Limit,
                TimeInForce::PostOnly,
                110,
                100,
                110
            ),
            LockPlan::RejectInBody
        );
        // PostOnly buy at 105 < 110 → non-cross, moves best_bid → hold.
        assert_eq!(
            classify_place(
                Side::Buy,
                OrderType::Limit,
                TimeInForce::PostOnly,
                105,
                100,
                110
            ),
            LockPlan::HoldTicket
        );
    }

    // ── place: taker TIFs / market → barrier ────────────────────────────────────
    #[test]
    fn ioc_fok_market_downgrade_regardless() {
        for tif in [TimeInForce::Ioc, TimeInForce::Fok] {
            assert_eq!(
                classify_place(Side::Buy, OrderType::Limit, tif, 90, 100, 110),
                LockPlan::DowngradeToBarrier
            );
        }
        assert_eq!(
            classify_place(Side::Buy, OrderType::Market, TimeInForce::Gtc, 0, 100, 110),
            LockPlan::DowngradeToBarrier
        );
    }

    // ── place: empty book side (best == 0) ──────────────────────────────────────
    #[test]
    fn empty_opposite_side_never_crosses_and_first_order_moves_best() {
        // empty ask side (best_ask=0): a buy can't cross; best_bid=0 too → first order moves best.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 105, 0, 0),
            LockPlan::HoldTicket
        );
        // ask side empty (0) but a resting bid exists at 100; buy at 90 < 100 → non-mover.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::Gtc, 90, 100, 0),
            LockPlan::ReleaseTicket
        );
    }

    // ── cancel: at-best vs below-best ─────────────────────────────────────────────
    #[test]
    fn cancel_at_best_detects_top_of_book() {
        // Buy at best_bid → at best (might move BBO → driver waits + checks emptiness).
        assert!(cancel_at_best(Side::Buy, 100, 100, 110));
        // Buy below best_bid → cannot move BBO → parallel.
        assert!(!cancel_at_best(Side::Buy, 90, 100, 110));
        // Sell at best_ask → at best.
        assert!(cancel_at_best(Side::Sell, 110, 100, 110));
        // Sell worse than best_ask → below best → parallel.
        assert!(!cancel_at_best(Side::Sell, 120, 100, 110));
    }
}

// ── Driver: single-work-item place lock-body (step 3c part 2) ─────────────────

/// One parallel-eligible place to run in a slot. The ed25519 verify + decode is hoisted upstream
/// (the dominant cost, parallel before the gates); this is the gated mutation. `rank`/`ticket` are
/// assigned by the batch in txn_id order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceWork {
    pub maker: Address,
    pub order_id: [u8; 32],
    pub market_id: u64,
    pub side: u8,
    pub price: u64,
    pub qty: u64,
    pub order_type: u8,
    pub tif: u8,
    pub client_order_id: [u8; 16],
    /// This maker's block-order rank (AccountGate, rule 6).
    pub rank: u64,
    /// This op's BBO ticket (txn_id order within the batch; rules 2+3).
    pub ticket: u64,
}

/// Outcome of a slot's place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceOutcome {
    /// Rested (or no-op) and committed to the book.
    Executed,
    /// The body rejected (PostOnly cross, insufficient margin, …); this slot's book writes were
    /// rolled back. Still a processed tx (gas charged upstream).
    Reverted,
    /// Must run on the serial barrier (step 3d): crossing matching-limit / IOC / FOK / Market.
    Downgrade,
}

/// Whether a perp body committed or was reverted (under the serial error policy).
enum BodyDisposition {
    Committed,
    Reverted,
}

/// Apply the serial perp_dex error policy to a body Result run inside `cp`: commit on `Ok`; on error
/// revert this slot's writes and either PROPAGATE a `Fatal` (storage/system bug → abort the whole
/// block) or report a per-tx `Reverted` (any other error, incl. `[INVARIANT]`). Mirrors the serial
/// dispatch in `perp_dex::mod`, so a parallel block aborts byte-identically to a serial one.
fn dispose_body<CTX: ContextTr, T>(
    ctx: &mut CTX,
    cp: JournalCheckpoint,
    r: Result<T, PrecompileError>,
) -> Result<BodyDisposition, PrecompileError> {
    match r {
        Ok(_) => {
            ctx.journal_mut().checkpoint_commit();
            Ok(BodyDisposition::Committed)
        }
        Err(e) => {
            ctx.journal_mut().checkpoint_revert(cp);
            if matches!(e, PrecompileError::Fatal(_)) {
                Err(e)
            } else {
                Ok(BodyDisposition::Reverted)
            }
        }
    }
}

/// Run `place_order_core` under the maker's account turn AND the book-side lock for the order's side,
/// inside a journal checkpoint so a (non-fatal) rejection rolls back this slot's off-trie writes via
/// the write-set (step 3b). Lock order: AccountGate (per maker) → BookSideLock (per market+side);
/// the body's level FIFO + price-list mutations on `work.side` are atomic against concurrent slots.
fn gated_execute<CTX: ContextTr>(
    ctx: &mut CTX,
    account_gate: &AccountGate,
    book_lock: &BookSideLock,
    work: &PlaceWork,
) -> Result<PlaceOutcome, PrecompileError> {
    account_gate.run(work.maker, work.rank, || {
        book_lock.run(work.market_id, work.side, work.price, || {
            let cp = ctx.journal_mut().checkpoint();
            let r = place_order_core(
                work.maker,
                work.order_id,
                work.market_id,
                work.side,
                work.price,
                work.qty,
                work.order_type,
                work.tif,
                work.client_order_id,
                ctx,
            );
            Ok(match dispose_body(ctx, cp, r)? {
                BodyDisposition::Committed => PlaceOutcome::Executed,
                BodyDisposition::Reverted => PlaceOutcome::Reverted,
            })
        })
    })
}

/// Shared per-block batch state threaded into every slot. Created ONCE by the driver (and the test
/// batch helpers): the gates plus the inline-taker machinery — the per-market completion watermark,
/// the dirty-levels set for on-demand FIFO sort, and the `{order_id -> ticket}` map the sort consults.
struct BatchEnv<'a> {
    /// The single market this batch operates on (spike scope); also the completion key for an op whose
    /// own market is unknown (a None-resolved cancel).
    market: u64,
    account_gate: &'a AccountGate,
    book_lock: &'a BookSideLock,
    bbo: &'a BboTicketLock,
    completion: &'a MarketCompletion,
    /// (side, market, price) levels that took a this-block rest and have not been FIFO-sorted since;
    /// an inline taker drains + sorts them (by `placed_tickets`) before it matches.
    dirty: &'a Mutex<HashSet<(u8, u64, u64)>>,
    /// order_id -> ticket for every Place op in the batch (incl. a taker's OWN rested remainder, whose
    /// order_id is the taker's). The FIFO sort key — order_id itself is keccak/per-caller, not ordered.
    placed_tickets: &'a HashMap<[u8; 32], u64>,
    /// In-memory `market → (best_bid, best_ask)` mirror, plain u64s: the classify hot path reads best
    /// from HERE (one never-contended mutex + a map get) instead of two overlay/storage reads. Seeded
    /// from storage at each market's FIRST classify; refreshed (re-read + upsert) after every
    /// held-ticket body that may move the BBO (mover / reject / inline taker / inline at-best cancel).
    /// SAFE because every access runs under the held BBO ticket — strictly serialized, and published
    /// ticket→ticket by the serve handoff — and best is only ever mutated by held-ticket bodies
    /// (released bodies are non-movers by classification). Storage stays the truth; this never
    /// writes back.
    best_mirror: &'a Mutex<HashMap<u64, (u64, u64)>>,
    /// Optional per-block profile (catalog #21 instrumentation). `Some` only when the node set
    /// `PERP_PROF=1`; `None` = zero-cost. Bodies bump it via [`BodyTimer`]; classify bumps the histogram.
    prof: Option<&'a PerpBlockProfile>,
}

/// Marks this op's `ticket` done in the market-wide [`MarketCompletion`] on EVERY exit (Ok / `?` /
/// panic), constructed at the TOP of parallel_place/parallel_cancel BEFORE any early return (incl. the
/// None-resolved cancel), so a later inline taker's `wait_below` can never hang on a missing signal.
/// Drops AFTER the body, so "done" follows the body's write-through book store.
struct MarketDoneOnDrop<'a> {
    completion: &'a MarketCompletion,
    market: u64,
    ticket: u64,
}

impl Drop for MarketDoneOnDrop<'_> {
    fn drop(&mut self) {
        self.completion.mark_done(self.market, self.ticket);
    }
}

/// Best `(bid, ask)` for `market` from the in-memory mirror (see [`BatchEnv::best_mirror`]); seeds it
/// from storage on the market's first classify. Runs under the held BBO ticket, so the get-or-seed is
/// race-free and the mutex is never contended. Poison-tolerant.
fn mirror_best<CTX: ContextTr>(
    ctx: &mut CTX,
    mirror: &Mutex<HashMap<u64, (u64, u64)>>,
    market: u64,
) -> Result<(u64, u64), PrecompileError> {
    if let Some(&v) = mirror
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&market)
    {
        return Ok(v);
    }
    let best_bid = storage::load_best_bid(ctx, market)?;
    let best_ask = storage::load_best_ask(ctx, market)?;
    mirror
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(market, (best_bid, best_ask));
    Ok((best_bid, best_ask))
}

/// Re-read best from storage and upsert the mirror — called under the held ticket AFTER any body that
/// may have moved the BBO (mover / reject-in-body / inline taker / inline at-best cancel), BEFORE the
/// ticket advances, so the next classify reads the post-body best. A reverted body rolls its writes
/// back first, so the re-read returns the original values (refresh is unconditional + harmless).
fn mirror_refresh<CTX: ContextTr>(
    ctx: &mut CTX,
    mirror: &Mutex<HashMap<u64, (u64, u64)>>,
    market: u64,
) -> Result<(), PrecompileError> {
    let best_bid = storage::load_best_bid(ctx, market)?;
    let best_ask = storage::load_best_ask(ctx, market)?;
    mirror
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(market, (best_bid, best_ask));
    Ok(())
}

/// Record that a this-block rest landed at `(side, market, price)` so the next inline taker FIFO-sorts
/// it before matching. Idempotent (a set). Poison-tolerant.
fn mark_dirty(dirty: &Mutex<HashSet<(u8, u64, u64)>>, side: u8, market: u64, price: u64) {
    dirty
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert((side, market, price));
}

/// On-demand FIFO sort: drain the dirty-levels set and re-sort each level's THIS-BLOCK entries by
/// `ticket` (via `placed_tickets`), leaving prior-block entries at the front — the same restoration
/// [`finalize_place_batch_ordering`] does, applied just-in-time before an inline taker reads the book
/// (parallel non-mover rests append in racy arrival order; the taker must see time-priority order).
/// Runs under the taker's held BBO ticket AFTER its completion-wait, so the levels are settled +
/// uncontended.
fn sort_dirty<CTX: ContextTr>(
    ctx: &mut CTX,
    dirty: &Mutex<HashSet<(u8, u64, u64)>>,
    placed_tickets: &HashMap<[u8; 32], u64>,
) -> Result<(), PrecompileError> {
    let levels: Vec<(u8, u64, u64)> = {
        let mut g = dirty.lock().unwrap_or_else(|p| p.into_inner());
        g.drain().collect()
    };
    for (side, market, price) in levels {
        let fifo = if side == 0 {
            storage::load_bid_level(ctx, market, price)?
        } else {
            storage::load_ask_level(ctx, market, price)?
        };
        let mut prior: Vec<[u8; 32]> = Vec::new();
        let mut this_block: Vec<[u8; 32]> = Vec::new();
        for oid in &fifo {
            if placed_tickets.contains_key(oid) {
                this_block.push(*oid);
            } else {
                prior.push(*oid);
            }
        }
        this_block.sort_by_key(|oid| placed_tickets[oid]);
        prior.extend(this_block);
        if prior != fifo {
            if side == 0 {
                storage::save_bid_level(ctx, market, price, &prior)?;
            } else {
                storage::save_ask_level(ctx, market, price, &prior)?;
            }
        }
    }
    Ok(())
}

/// Run an INLINE taker (a crossing place) serially while the caller holds the BBO ticket: the full
/// match body ([`place_order_core`]) under the taker's OWN account turn ONLY — NO book-side lock and
/// NO re-entry of the BBO ticket. Exclusivity comes from the held ticket + the completion-wait (no
/// lower op is still writing, no higher op has been served), so the match's level RMWs are
/// uncontended. The account gate (rank monotone in ticket — [`normalize_schedule`]) advances the
/// taker's account lane; counterparty makers are settled by the lock-free engine, attributed to this
/// txn exactly as in serial.
fn gated_taker<CTX: ContextTr>(
    ctx: &mut CTX,
    account_gate: &AccountGate,
    work: &PlaceWork,
) -> Result<PlaceOutcome, PrecompileError> {
    account_gate.run(work.maker, work.rank, || {
        let cp = ctx.journal_mut().checkpoint();
        let r = place_order_core(
            work.maker,
            work.order_id,
            work.market_id,
            work.side,
            work.price,
            work.qty,
            work.order_type,
            work.tif,
            work.client_order_id,
            ctx,
        );
        Ok(match dispose_body(ctx, cp, r)? {
            BodyDisposition::Committed => PlaceOutcome::Executed,
            BodyDisposition::Reverted => PlaceOutcome::Reverted,
        })
    })
}

/// Run an INLINE cancel at the best price serially while holding the BBO ticket (removing it may move
/// the BBO): [`cancel_order_core`] under the canceller's account turn ONLY — NO book lock (the held
/// ticket + completion-wait give exclusivity), so the removal + best-refresh run serial-equivalently.
fn gated_cancel_inline<CTX: ContextTr>(
    ctx: &mut CTX,
    account_gate: &AccountGate,
    work: &CancelWork,
) -> Result<CancelOutcome, PrecompileError> {
    account_gate.run(work.canceller, work.rank, || {
        let cp = ctx.journal_mut().checkpoint();
        let r = cancel_order_core(work.canceller, work.order_id, ctx);
        Ok(match dispose_body(ctx, cp, r)? {
            BodyDisposition::Committed => CancelOutcome::Executed,
            BodyDisposition::Reverted => CancelOutcome::Reverted,
        })
    })
}

/// Execute one parallel place (the lock-body). Classify under the BBO ticket; a mover / PostOnly
/// cross executes under the held ticket (the account rank is waited at the place — Q3), a non-mover
/// releases the ticket then executes under the account gate (rule 4.iii), a crossing matcher / taker
/// runs INLINE under the held ticket (step 4 — after a completion-wait + on-demand FIFO sort).
pub(crate) fn parallel_place<CTX: ContextTr>(
    ctx: &mut CTX,
    env: &BatchEnv<'_>,
    work: &PlaceWork,
) -> Result<PlaceOutcome, PrecompileError> {
    // Mark this ticket done in the market-wide completion watermark on EVERY exit (Ok / `?` / panic),
    // AFTER the body — so a later inline taker's `wait_below` can never hang on a missing signal.
    let _done = MarketDoneOnDrop {
        completion: env.completion,
        market: env.market,
        ticket: work.ticket,
    };
    enum UnderTicket {
        Done(Result<PlaceOutcome, PrecompileError>),
        Release,
    }
    // §B: time the whole `bbo.run` (= wait on the serve cursor + held running the closure) and, via the
    // inner closure, the held portion alone → wait = total − held. `held_ns` is written by the closure
    // (runs inline on this thread) and read after the call.
    let held_ns = std::cell::Cell::new(0u64);
    let run_start = env.prof.map(|_| Instant::now());
    let under = env
        .bbo
        .run(work.ticket, || -> Result<UnderTicket, PrecompileError> {
            let held_start = env.prof.map(|_| Instant::now());
            let out = (|| -> Result<UnderTicket, PrecompileError> {
                let cls_start = env.prof.map(|_| Instant::now());
                let plan = match (
                    Side::from_u8(work.side),
                    OrderType::from_u8(work.order_type),
                    TimeInForce::from_u8(work.tif),
                ) {
                    (Some(side), Some(order_type), Some(tif)) => {
                        // Plain-u64 mirror read — no overlay/storage on the classify hot path.
                        let (best_bid, best_ask) =
                            mirror_best(ctx, env.best_mirror, work.market_id)?;
                        classify_place(side, order_type, tif, work.price, best_bid, best_ask)
                    }
                    // Unparseable order fields → let the body reject it under the ticket.
                    _ => LockPlan::RejectInBody,
                };
                if let (Some(p), Some(cs)) = (env.prof, cls_start) {
                    // §C: the 2 best-reads + classify_place (runs under the ticket for EVERY op).
                    p.classify_ns.fetch_add(cs.elapsed().as_nanos() as u64, Ordering::Relaxed);
                }
                if let Some(p) = env.prof {
                    p.bump_plan(plan); // §A classification histogram
                }
                match plan {
                    // A crossing matcher / taker (GTC-cross / Market / IOC / FOK) runs INLINE under the
                    // held BBO ticket (step 4 — replaces the deferred barrier + contagion): wait until
                    // every lower-txn_id op's book write has landed (the serial book state at this
                    // ticket), then FIFO-sort the parallel rests it may sweep, then match. No higher
                    // ticket has been served (BBO cursor), no lower op is still writing → uncontended.
                    LockPlan::DowngradeToBarrier => {
                        env.completion.wait_below(env.market, work.ticket);
                        sort_dirty(ctx, env.dirty, env.placed_tickets)?;
                        let r = {
                            let _bt = BodyTimer::new(env.prof); // §1 time the inline-taker body
                            gated_taker(ctx, env.account_gate, work)
                        };
                        // The match may have moved the BBO — refresh the mirror before the ticket
                        // advances so the next classify reads the post-body best.
                        mirror_refresh(ctx, env.best_mirror, work.market_id)?;
                        Ok(UnderTicket::Done(r))
                    }
                    // A mover (or a PostOnly-cross self-reject) executes while holding the BBO ticket so
                    // its best-update is serialized; the body itself takes the book-side lock.
                    LockPlan::HoldTicket | LockPlan::RejectInBody => {
                        let r = {
                            let _bt = BodyTimer::new(env.prof); // §1 time the mover/reject body
                            gated_execute(ctx, env.account_gate, env.book_lock, work)
                        };
                        // A committed mover moved the BBO (a reject rolled back → re-read = original).
                        mirror_refresh(ctx, env.best_mirror, work.market_id)?;
                        Ok(UnderTicket::Done(r))
                    }
                    LockPlan::ReleaseTicket => Ok(UnderTicket::Release),
                }
            })();
            if let (Some(_p), Some(hs)) = (env.prof, held_start) {
                held_ns.set(hs.elapsed().as_nanos() as u64); // §B held = time under the ticket
            }
            out
        })?;
    if let (Some(p), Some(rs)) = (env.prof, run_start) {
        let total = rs.elapsed().as_nanos() as u64;
        let held = held_ns.get();
        p.bbo_held_ns.fetch_add(held, Ordering::Relaxed);
        // §B wait = total − held (blocked on the serve cursor before the closure ran).
        p.bbo_wait_ns.fetch_add(total.saturating_sub(held), Ordering::Relaxed);
    }
    let outcome = match under {
        UnderTicket::Done(r) => r,
        // Ticket released (non-mover); rest under the account gate + book-side lock, in parallel.
        UnderTicket::Release => {
            let _bt = BodyTimer::new(env.prof); // §1 time the parallel non-mover body
            gated_execute(ctx, env.account_gate, env.book_lock, work)
        }
    };
    // A rest (mover / non-mover / a taker's partial-fill remainder) landed at `work.price` → mark the
    // level dirty so the next inline taker FIFO-sorts it before matching. Idempotent on no-rest.
    if matches!(outcome, Ok(PlaceOutcome::Executed)) {
        mark_dirty(env.dirty, work.side, work.market_id, work.price);
    }
    outcome
}

/// Run a batch of parallel-eligible places concurrently against ONE shared book, gated by the
/// `AccountGate` (per maker) and `BboTicketLock` (per-market BBO). Each worker builds a FRESH slot
/// context thread-locally via `make_ctx` (so the journal never crosses a thread → no `Send` bound on
/// the journal), then runs [`parallel_place`]. Results are returned in `items` order. `make_ctx`
/// must return a context whose journal already routes to `book` (i.e. `set_perp_shared(book)` done);
/// the book must already hold the block's prior state (the driver/test seeds it).
pub fn run_place_batch<CTX, F>(
    book: &Arc<SharedPerpBook>,
    account_gate: &AccountGate,
    book_lock: &BookSideLock,
    bbo: &BboTicketLock,
    items: &[PlaceWork],
    make_ctx: F,
) -> Vec<Result<PlaceOutcome, PrecompileError>>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX + Sync,
{
    // Build the inline-taker batch state internally so the external signature (used by tests) is
    // unchanged: a fresh per-market completion watermark, dirty-levels set, and {order_id->ticket} map
    // for this place-only batch. `market` = the items' (single) market.
    let completion = MarketCompletion::new();
    let dirty = Mutex::new(HashSet::new());
    let placed_tickets: HashMap<[u8; 32], u64> =
        items.iter().map(|w| (w.order_id, w.ticket)).collect();
    let best_mirror = Mutex::new(HashMap::new());
    let market = items.first().map(|w| w.market_id).unwrap_or(0);
    let env = BatchEnv {
        market,
        account_gate,
        book_lock,
        bbo,
        completion: &completion,
        dirty: &dirty,
        placed_tickets: &placed_tickets,
        best_mirror: &best_mirror,
        prof: None,
    };
    let (make_ctx, env) = (&make_ctx, &env);
    thread::scope(|s| {
        let handles: Vec<_> = items
            .iter()
            .map(|item| {
                let book = book.clone();
                s.spawn(move || {
                    let mut ctx = make_ctx(book);
                    parallel_place(&mut ctx, env, item)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// Barrier step (end of a parallel batch / step-3d phase boundary): restore deterministic FIFO
/// time-priority. During the batch, this block's inserts were appended to each level in racy
/// arrival order; here — serially, after all inserts have landed and BEFORE any match reads a level
/// — we re-sort each touched level's THIS-BLOCK entries by `ticket` (= block-local txn_id order),
/// leaving prior-block entries (already consensus-frozen) in place at the front. The `ticket` is a
/// transient sort key; the committed FIFO stays `Vec<order_id>` (no format change). Idempotent:
/// levels already in order are skipped.
pub fn finalize_place_batch_ordering<CTX, F>(
    book: &Arc<SharedPerpBook>,
    items: &[PlaceWork],
    results: &[Result<PlaceOutcome, PrecompileError>],
    make_ctx: F,
) -> Result<(), PrecompileError>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX,
{
    use std::collections::HashMap;
    // (side, market, price) -> { order_id -> ticket } for this block's executed (rested) places.
    let mut levels: HashMap<(u8, u64, u64), HashMap<[u8; 32], u64>> = HashMap::new();
    for (item, res) in items.iter().zip(results) {
        if matches!(res, Ok(PlaceOutcome::Executed)) {
            levels
                .entry((item.side, item.market_id, item.price))
                .or_default()
                .insert(item.order_id, item.ticket);
        }
    }
    if levels.is_empty() {
        return Ok(());
    }
    let mut ctx = make_ctx(book.clone());
    for ((side, market, price), tickets) in levels {
        let fifo = if side == 0 {
            storage::load_bid_level(&mut ctx, market, price)?
        } else {
            storage::load_ask_level(&mut ctx, market, price)?
        };
        // Split: prior-block entries (keep order) ++ this-block entries (re-sort by ticket).
        let mut prior: Vec<[u8; 32]> = Vec::new();
        let mut this_block: Vec<[u8; 32]> = Vec::new();
        for oid in &fifo {
            if tickets.contains_key(oid) {
                this_block.push(*oid);
            } else {
                prior.push(*oid);
            }
        }
        this_block.sort_by_key(|oid| tickets[oid]);
        prior.extend(this_block);
        if prior != fifo {
            if side == 0 {
                storage::save_bid_level(&mut ctx, market, price, &prior)?;
            } else {
                storage::save_ask_level(&mut ctx, market, price, &prior)?;
            }
        }
    }
    Ok(())
}

// ── Driver: parallel cancel (step 3c part 4) ──────────────────────────────────

/// One cancel to run in a slot. The ed25519 verify + decode is hoisted upstream; this is the gated
/// mutation. The order's market/side/price are NOT known until it is loaded, so they are resolved by
/// the pre-scan ([`plan_cancel_batch`]). `rank`/`ticket` are assigned by the batch in txn_id order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelWork {
    pub canceller: Address,
    pub order_id: [u8; 32],
    /// This canceller's block-order rank (AccountGate, rule 6).
    pub rank: u64,
    /// This op's BBO ticket (txn_id order within the batch; rules 2+3).
    pub ticket: u64,
}

/// Outcome of a slot's cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// Removed from the book (and committed).
    Executed,
    /// The body rejected (not owner / not cancellable / order missing); this slot's writes rolled
    /// back. Still a processed tx (gas charged upstream).
    Reverted,
    /// At the best level AND the sole order there → removing it moves the BBO, which needs a book
    /// scan after all lower-txn_id ops land → deferred to the serial barrier (step 3d). Not applied
    /// in the parallel phase.
    Downgrade,
}

/// Resolved cancel produced by the pre-scan: the order's `(market, side, price)`. `resolved == None`
/// means the order was not loadable (→ the body reverts). (#21 step-4: the inline-taker driver's
/// market-wide completion-wait replaced the per-(market,price) `required` wait set.)
#[derive(Debug, Clone)]
struct CancelPlanItem {
    work: CancelWork,
    resolved: Option<(u64, u8, u64)>,
}

/// Run `cancel_order_core` under the canceller's account turn + the order side's book lock, inside a
/// journal checkpoint so a (non-fatal) rejection rolls back this slot's off-trie writes. `market`/
/// `side` scope the book lock; pass the resolved side (or, for an unloadable order, skip the book
/// lock since the body cannot reach a level — it reverts on the missing-order load).
fn gated_cancel<CTX: ContextTr>(
    ctx: &mut CTX,
    account_gate: &AccountGate,
    book_lock: &BookSideLock,
    work: &CancelWork,
    market: u64,
    side: u8,
    price: u64,
) -> Result<CancelOutcome, PrecompileError> {
    account_gate.run(work.canceller, work.rank, || {
        book_lock.run(market, side, price, || {
            let cp = ctx.journal_mut().checkpoint();
            let r = cancel_order_core(work.canceller, work.order_id, ctx);
            Ok(match dispose_body(ctx, cp, r)? {
                BodyDisposition::Committed => CancelOutcome::Executed,
                BodyDisposition::Reverted => CancelOutcome::Reverted,
            })
        })
    })
}

/// Execute one parallel cancel. Classifies under the BBO ticket: below-best → remove in parallel
/// (cannot move the BBO); a NON-owner at-best cancel reverts touching nothing → also parallel (3d-7:
/// a griefer must not serialize the block); an OWNER at-best cancel may EMPTY the level (move the BBO)
/// → run INLINE under the held ticket (step 4 — replaces the deferred barrier): wait until every
/// lower-txn_id op's book write has landed, then remove + refresh best, serial-equivalently. The
/// outermost [`MarketDoneOnDrop`] marks this ticket done on EVERY exit (incl. the None-resolved
/// early return), so a later inline taker's `wait_below` can never hang.
fn parallel_cancel<CTX: ContextTr>(
    ctx: &mut CTX,
    env: &BatchEnv<'_>,
    plan: &CancelPlanItem,
) -> Result<CancelOutcome, PrecompileError> {
    // Mark this ticket done on EVERY exit (incl. the None branch below, BEFORE it returns). Keyed by
    // the batch market — a None-resolved cancel has no resolved market of its own.
    let _done = MarketDoneOnDrop {
        completion: env.completion,
        market: env.market,
        ticket: plan.work.ticket,
    };
    if let Some(p) = env.prof {
        p.n_cancel.fetch_add(1, Ordering::Relaxed); // §A cancel op
    }
    let (market, side_u8, price) = match plan.resolved {
        Some(t) => t,
        // Order not loadable → revert (missing order), touching no level → no book lock, no wait. Still
        // takes its BBO ticket turn (empty body) so the serve cursor advances; the guard marks done.
        None => {
            env.bbo.run(plan.work.ticket, || ());
            return env.account_gate.run(plan.work.canceller, plan.work.rank, || {
                let cp = ctx.journal_mut().checkpoint();
                let r = cancel_order_core(plan.work.canceller, plan.work.order_id, ctx);
                Ok(match dispose_body(ctx, cp, r)? {
                    BodyDisposition::Committed => CancelOutcome::Executed,
                    BodyDisposition::Reverted => CancelOutcome::Reverted,
                })
            });
        }
    };
    let side = Side::from_u8(side_u8).expect("pre-scan resolved a valid side");

    enum Flow {
        Done(Result<CancelOutcome, PrecompileError>),
        RunParallel,
    }
    // Classify under the BBO ticket. Below-best → parallel removal. At-best non-owner → parallel
    // (reverts, BBO unchanged — 3d-7). At-best owner → may move the BBO → INLINE: wait for every
    // lower-txn_id op to apply, then remove + refresh best under the held ticket. (An owner at-best
    // cancel that does NOT empty the level also runs inline — a minor over-serialization vs a parallel
    // removal; deferred perf TODO. Ownership is fixed, so it is checked without waiting.)
    // §B: same wait/held split as parallel_place — time the whole bbo.run, and held via the inner
    // closure. classify_ns counts the 2 best-reads (consistent with place).
    let held_ns = std::cell::Cell::new(0u64);
    let run_start = env.prof.map(|_| Instant::now());
    let flow = env
        .bbo
        .run(plan.work.ticket, || -> Result<Flow, PrecompileError> {
            let held_start = env.prof.map(|_| Instant::now());
            let out = (|| -> Result<Flow, PrecompileError> {
                let cls_start = env.prof.map(|_| Instant::now());
                // Plain-u64 mirror read — no overlay/storage on the classify hot path.
                let (best_bid, best_ask) = mirror_best(ctx, env.best_mirror, market)?;
                if let (Some(p), Some(cs)) = (env.prof, cls_start) {
                    p.classify_ns.fetch_add(cs.elapsed().as_nanos() as u64, Ordering::Relaxed); // §C
                }
                if !cancel_at_best(side, price, best_bid, best_ask) {
                    return Ok(Flow::RunParallel);
                }
                let is_owner = storage::load_order(ctx, &plan.work.order_id)?
                    .map(|o| o.owner == plan.work.canceller.0 .0)
                    .unwrap_or(false);
                if !is_owner {
                    return Ok(Flow::RunParallel);
                }
                env.completion.wait_below(env.market, plan.work.ticket);
                let r = {
                    let _bt = BodyTimer::new(env.prof); // §1 time the inline at-best cancel body
                    gated_cancel_inline(ctx, env.account_gate, &plan.work)
                };
                // Removing the at-best order may have moved the BBO — refresh before advancing.
                mirror_refresh(ctx, env.best_mirror, market)?;
                Ok(Flow::Done(r))
            })();
            if let (Some(_p), Some(hs)) = (env.prof, held_start) {
                held_ns.set(hs.elapsed().as_nanos() as u64); // §B held = time under the ticket
            }
            out
        })?;
    if let (Some(p), Some(rs)) = (env.prof, run_start) {
        let total = rs.elapsed().as_nanos() as u64;
        let held = held_ns.get();
        p.bbo_held_ns.fetch_add(held, Ordering::Relaxed);
        // §B wait = total − held (blocked on the serve cursor before the closure ran).
        p.bbo_wait_ns.fetch_add(total.saturating_sub(held), Ordering::Relaxed);
    }
    match flow {
        Flow::Done(r) => r,
        // Below best, or at-best non-owner → ticket released; remove under the account gate +
        // book-side lock, in parallel.
        Flow::RunParallel => {
            let _bt = BodyTimer::new(env.prof); // §1 time the parallel cancel body
            gated_cancel(
                ctx,
                env.account_gate,
                env.book_lock,
                &plan.work,
                market,
                side_u8,
                price,
            )
        }
    }
}

/// Pre-scan (serial, before the parallel phase): resolve each cancel's order to `(market, side,
/// price)` and compute, for each, the tickets of all lower-txn_id ops at the SAME `(market, price)`.
/// An at-best cancel waits for that set so the level membership reflects every lower-txn_id removal
/// before it decides emptiness — the serial-equivalent view.
fn plan_cancel_batch<CTX, F>(
    book: &Arc<SharedPerpBook>,
    items: &[CancelWork],
    make_ctx: &F,
) -> Vec<CancelPlanItem>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX,
{
    let mut ctx = make_ctx(book.clone());
    let resolved: Vec<Option<(u64, u8, u64)>> = items
        .iter()
        .map(|w| {
            storage::load_order(&mut ctx, &w.order_id)
                .ok()
                .flatten()
                .map(|o| (o.market_id, o.side as u8, o.price))
        })
        .collect();

    items
        .iter()
        .enumerate()
        .map(|(i, w)| CancelPlanItem {
            work: w.clone(),
            resolved: resolved[i],
        })
        .collect()
}

/// Run a batch of cancels concurrently against ONE shared book, gated by the `AccountGate` (per
/// canceller), `BookSideLock` (per market+side), and `BboTicketLock` (per-market BBO), with a
/// per-block [`PriceCompletion`] tracking the at-best-cancel waits. Each worker builds a FRESH slot
/// context via `make_ctx` (so the journal never crosses a thread). Results are returned in `items`
/// order. Like the place batch, `make_ctx` must route the journal to `book`.
pub fn run_cancel_batch<CTX, F>(
    book: &Arc<SharedPerpBook>,
    account_gate: &AccountGate,
    book_lock: &BookSideLock,
    bbo: &BboTicketLock,
    items: &[CancelWork],
    make_ctx: F,
) -> Vec<Result<CancelOutcome, PrecompileError>>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX + Sync,
{
    let plans = plan_cancel_batch(book, items, &make_ctx);
    // Cancel-only batch: build the inline-taker batch state internally (external signature unchanged).
    // No places → empty `placed_tickets`/`dirty`; `market` = the first resolvable cancel's market.
    let completion = MarketCompletion::new();
    let dirty = Mutex::new(HashSet::new());
    let placed_tickets: HashMap<[u8; 32], u64> = HashMap::new();
    let best_mirror = Mutex::new(HashMap::new());
    let market = plans
        .iter()
        .find_map(|p| p.resolved.map(|(m, _, _)| m))
        .unwrap_or(0);
    let env = BatchEnv {
        market,
        account_gate,
        book_lock,
        bbo,
        completion: &completion,
        dirty: &dirty,
        placed_tickets: &placed_tickets,
        best_mirror: &best_mirror,
        prof: None,
    };
    let (make_ctx, plans, env) = (&make_ctx, &plans, &env);
    thread::scope(|s| {
        let handles: Vec<_> = plans
            .iter()
            .map(|plan| {
                let book = book.clone();
                s.spawn(move || {
                    let mut ctx = make_ctx(book);
                    parallel_cancel(&mut ctx, env, plan)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

// ── Driver: unified parallel block (step 3d) ──────────────────────────────────

/// One perp op in a block, in txn_id order. `Place`/`Cancel` carry their pre-assigned `ticket`
/// (= block-local txn_id, dense 0..n) and per-maker `rank`. The driver fans them out to the parallel
/// phase; any op that downgrades is re-run on the serial barrier (step 3d-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerpOp {
    Place(PlaceWork),
    Cancel(CancelWork),
}

/// Per-op result, returned in txn_id order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpResult {
    Place(PlaceOutcome),
    Cancel(CancelOutcome),
}

/// One op's result PLUS the EVM logs its matching emitted, returned by [`transact_block_parallel`] in
/// txn_id order. The matching runs in throwaway per-slot / barrier contexts, so the driver drains each
/// op's logs (Trade / OrderPlaced / OrderRested / OrderCancelled / PositionChanged) here; the node
/// carries them into [`PerpReplayResult::logs`] so the serial replay re-emits them and the canonical
/// receipts carry the same perp events as serial execution. Empty for a reverted op (logs rolled back).
#[derive(Debug, Clone)]
pub struct OpReplay {
    /// The op outcome (Executed / Reverted / Downgrade).
    pub result: OpResult,
    /// The EVM logs the op emitted, in emission order.
    pub logs: Vec<Log>,
}

/// How the step-4b parallel pre-phase treats one perp transaction's calldata. The node calls
/// [`classify_perp_tx`] SERIALLY in block order for each top-level `0x…1003` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerpTxClass {
    /// A trading op to match in the parallel phase. `success_output` is the precompile return bytes to
    /// replay if the driver Executes it (the ABI-encoded orderId for a place; empty for a cancel — a
    /// driver Revert is replayed as a revert with a generic reason, which is not consensus-relevant).
    Trade { op: PerpOp, success_output: Vec<u8> },
    /// Decode/verify REJECTED this trading call before matching (bad calldata / sig / key / recv-window
    /// / duplicate). It never enters the driver; phase B replays this revert. `output` = the ABI revert.
    Reject { output: Vec<u8> },
    /// Not one of the four trading selectors (a non-trading perp call, or undecodable) → runs normally
    /// in the serial EVM pass; no replay entry, not counted by the replay cursor.
    NotTrading,
}

/// Classify a perp transaction for canonical parallel execution (step 4b): decode + (for signed)
/// authenticate via the SINGLE-SOURCE [`crate::perp_dex::trading`] helpers (so verify cannot drift from
/// the serial precompile path), then build a [`PerpOp`] for the driver — WITHOUT matching. Runs
/// SERIALLY per tx (verify is intentionally not parallelized here). A non-fatal decode/verify error
/// becomes [`PerpTxClass::Reject`] (the serial EVM pass replays it as a revert); a `Fatal`
/// (storage/system) propagates to abort the block. `context` is a perp-cold-read context — for a direct
/// `placeOrder` it also advances the sequential order-id counter (written to the shared book).
pub fn classify_perp_tx<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<PerpTxClass, PrecompileError> {
    let Some(selector) = input_bytes
        .get(..4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
    else {
        return Ok(PerpTxClass::NotTrading);
    };

    if selector == placeOrderCall::SELECTOR {
        match decode_place_order(input_bytes, caller, context) {
            Ok(p) => Ok(place_trade(p)),
            Err(e) => reject_or_fatal(e),
        }
    } else if selector == placeOrderSignedCall::SELECTOR {
        match verify_place_order_signed(input_bytes, context) {
            Ok(p) => Ok(place_trade(p)),
            Err(e) => reject_or_fatal(e),
        }
    } else if selector == cancelOrderCall::SELECTOR {
        match decode_cancel_order(input_bytes, caller) {
            Ok(c) => Ok(cancel_trade(c)),
            Err(e) => reject_or_fatal(e),
        }
    } else if selector == cancelOrderSignedCall::SELECTOR {
        match verify_cancel_order_signed(input_bytes, context) {
            Ok(c) => Ok(cancel_trade(c)),
            Err(e) => reject_or_fatal(e),
        }
    } else {
        Ok(PerpTxClass::NotTrading)
    }
}

/// A non-fatal decode/verify error → [`PerpTxClass::Reject`] (the serial EVM pass replays it as a
/// revert); a `Fatal` (storage/system) propagates to abort the block. Shared by [`classify_perp_tx`]
/// and [`classify_perp_tx_pending`].
fn reject_or_fatal(e: PrecompileError) -> Result<PerpTxClass, PrecompileError> {
    match e {
        PrecompileError::Fatal(f) => Err(PrecompileError::Fatal(f)),
        other => Ok(PerpTxClass::Reject {
            output: encode_revert_string(&other.to_string()).to_vec(),
        }),
    }
}

/// Build a `Trade` for a verified place. `rank`/`ticket` are placeholders — the driver assigns them
/// in [`normalize_schedule`].
fn place_trade(p: PlaceParams) -> PerpTxClass {
    PerpTxClass::Trade {
        success_output: encode_place_order_id(p.order_id).to_vec(),
        op: PerpOp::Place(PlaceWork {
            maker: p.maker,
            order_id: p.order_id,
            market_id: p.market_id,
            side: p.side,
            price: p.price,
            qty: p.qty,
            order_type: p.order_type,
            tif: p.tif,
            client_order_id: p.client_order_id,
            rank: 0,
            ticket: 0,
        }),
    }
}

/// Build a `Trade` for a verified cancel. `cancel_order_core` returns empty bytes on success, so the
/// replayed success output is empty.
fn cancel_trade(c: CancelParams) -> PerpTxClass {
    PerpTxClass::Trade {
        success_output: Vec::new(),
        op: PerpOp::Cancel(CancelWork {
            canceller: c.canceller,
            order_id: c.order_id,
            rank: 0,
            ticket: 0,
        }),
    }
}

/// A perp tx classified by the PARALLEL pre-phase (#3), with a direct place's order id DEFERRED.
/// Produced by [`classify_perp_tx_pending`] (READ-ONLY on perp state → the node runs it concurrently
/// across the block's txs); resolved to a [`PerpTxClass`] by [`finalize_pending_classes`], which
/// assigns direct-place ids serially in txn order (the nonce read-modify-write cannot race across
/// same-account places).
#[derive(Debug, Clone)]
pub enum PendingPerpTx {
    /// A direct (plain) `placeOrder`: its id is `keccak256(account ‖ nonce)` assigned in the serial
    /// pass, `nonce` starting at the committed `base_nonce` (read here in parallel).
    PendingPlace {
        maker: Address,
        base_nonce: u64,
        market_id: u64,
        side: u8,
        price: u64,
        qty: u64,
        order_type: u8,
        tif: u8,
        client_order_id: [u8; 16],
    },
    /// A class already fully resolved in parallel — a signed place (id = keccak(sig), no nonce), a
    /// cancel, a reject, or a non-trading call — passed through unchanged.
    Resolved(PerpTxClass),
}

/// PARALLEL-phase classify (#3): like [`classify_perp_tx`] but a direct `placeOrder` is returned as
/// [`PendingPerpTx::PendingPlace`] with its id DEFERRED — so this fn is READ-ONLY on the perp state
/// (no nonce write) and the node can run it CONCURRENTLY across the block's txs (one cold-read ctx per
/// tx). Every other class (signed place, cancel, reject, not-trading) is fully resolved here, exactly
/// as [`classify_perp_tx`] would. A `Fatal` propagates; a non-fatal error becomes a `Resolved(Reject)`.
pub fn classify_perp_tx_pending<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<PendingPerpTx, PrecompileError> {
    let is_direct_place = input_bytes
        .get(..4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        == Some(placeOrderCall::SELECTOR);
    if is_direct_place {
        // Direct place: defer the id (its nonce RMW is applied serially in txn order downstream).
        match decode_place_order_pending(input_bytes, caller, context) {
            Ok(p) => Ok(PendingPerpTx::PendingPlace {
                maker: p.maker,
                base_nonce: p.base_nonce,
                market_id: p.market_id,
                side: p.side,
                price: p.price,
                qty: p.qty,
                order_type: p.order_type,
                tif: p.tif,
                client_order_id: p.client_order_id,
            }),
            Err(e) => Ok(PendingPerpTx::Resolved(reject_or_fatal(e)?)),
        }
    } else {
        // Signed place (id = keccak(sig), no nonce) / cancel / signed cancel / not-trading are all
        // id-resolved (or id-free) in parallel — delegate to the single-source classify.
        classify_perp_tx(input_bytes, caller, context).map(PendingPerpTx::Resolved)
    }
}

/// SERIAL resolve of the parallel-classified block (#3): assign each direct place's order id from its
/// maker's nonce sequence in txn order — `keccak256(account ‖ nonce)`, `nonce` starting at the
/// committed base and incrementing once per direct place — then write each toucher's FINAL nonce to
/// the shared book. Produces the [`PerpTxClass`] list in txn order, byte-identical to what a serial
/// [`classify_perp_tx`] scan produces (a signed place / cancel / reject passes through untouched, and
/// a signed place never advances the nonce, matching serial). `context` must route to the block's
/// shared book (the nonce writes land there, as the serial classify's do).
pub fn finalize_pending_classes<CTX: ContextTr>(
    pending: Vec<PendingPerpTx>,
    context: &mut CTX,
) -> Result<Vec<PerpTxClass>, PrecompileError> {
    // Per-maker running nonce: seeded at the committed base on first sight, incremented per direct
    // place in txn order. BTreeMap → the final-nonce write loop is deterministically ordered (the
    // commitment is order-independent regardless, but this removes any doubt).
    let mut running: std::collections::BTreeMap<Address, u64> = std::collections::BTreeMap::new();
    let mut out = Vec::with_capacity(pending.len());
    for p in pending {
        match p {
            PendingPerpTx::Resolved(c) => out.push(c),
            PendingPerpTx::PendingPlace {
                maker,
                base_nonce,
                market_id,
                side,
                price,
                qty,
                order_type,
                tif,
                client_order_id,
            } => {
                let n = running.entry(maker).or_insert(base_nonce);
                let order_id = compute_order_id(maker, *n);
                *n += 1;
                out.push(place_trade(PlaceParams {
                    maker,
                    order_id,
                    market_id,
                    side,
                    price,
                    qty,
                    order_type,
                    tif,
                    client_order_id,
                }));
            }
        }
    }
    // Persist each toucher's final nonce (= base + count) — the same final state serial leaves (serial
    // writes base+1..base+count; last-write-wins per key → identical committed value).
    for (acct, final_nonce) in running {
        storage::save_user_nonce(context, acct, final_nonce)?;
    }
    Ok(out)
}

/// One op prepared by the serial pre-scan: a place (unchanged) or a cancel with its resolved
/// `(market, side, price)` + the lower-ticket SAME-(market, price) ticket set it must wait for.
#[derive(Debug, Clone)]
enum PreparedOp {
    Place(PlaceWork),
    Cancel(CancelPlanItem),
}

/// Serial pre-scan over the WHOLE block (places + cancels): resolve each op's `(market, side, price)`
/// and, for each cancel, the tickets of all lower-txn_id ops (place OR cancel) at the SAME
/// `(market, price)` — the set the at-best cancel waits for so the level membership is settled before
/// it decides emptiness. (In a non-crossing book a price belongs to exactly one side, so the
/// `(market, price)` key — which PriceCompletion is also keyed on — implies the side.)
fn plan_block<CTX, F>(book: &Arc<SharedPerpBook>, ops: &[PerpOp], make_ctx: &F) -> Vec<PreparedOp>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX,
{
    let mut ctx = make_ctx(book.clone());
    // Same-block place linkage: a cancel of an order PLACED earlier this block can't resolve from the
    // book (the pre-scan runs before the parallel phase rests it), so link it to the place op's
    // (market, side, price) by order_id. order_ids are unique, so a cancel resolves from the book
    // (prior-block order) OR this map (same-block place), never both.
    let mut placed: std::collections::HashMap<[u8; 32], (u64, u8, u64)> =
        std::collections::HashMap::new();
    for op in ops {
        if let PerpOp::Place(w) = op {
            placed.insert(w.order_id, (w.market_id, w.side, w.price));
        }
    }
    // (market, side, price) per op; None = a cancel whose order is neither in the book nor placed
    // this block.
    let resolved: Vec<Option<(u64, u8, u64)>> = ops
        .iter()
        .map(|op| match op {
            PerpOp::Place(w) => Some((w.market_id, w.side, w.price)),
            PerpOp::Cancel(w) => storage::load_order(&mut ctx, &w.order_id)
                .ok()
                .flatten()
                .map(|o| (o.market_id, o.side as u8, o.price))
                .or_else(|| placed.get(&w.order_id).copied()),
        })
        .collect();

    ops.iter()
        .enumerate()
        .map(|(i, op)| match op {
            PerpOp::Place(w) => PreparedOp::Place(w.clone()),
            PerpOp::Cancel(w) => PreparedOp::Cancel(CancelPlanItem {
                work: w.clone(),
                resolved: resolved[i],
            }),
        })
        .collect()
}

/// Assign each op's scheduling metadata from its position: `ticket = index` (txn_id order) and
/// `rank = the maker's running count`. So per maker, `rank` is monotone in `ticket` BY CONSTRUCTION
/// — the driver OWNS this, callers just supply ops in txn_id order. Without it the held-ticket →
/// AccountGate wait could deadlock: a place MOVER pins the BBO serve cursor at its ticket while
/// waiting on the AccountGate for its rank predecessor; if that predecessor had a HIGHER ticket
/// (rank/ticket inversion) it would be blocked behind the pinned cursor → cycle. Any `ticket`/`rank`
/// on the input works is overwritten.
fn normalize_schedule(ops: &[PerpOp]) -> Vec<PerpOp> {
    let mut next_rank: std::collections::HashMap<Address, u64> = std::collections::HashMap::new();
    ops.iter()
        .enumerate()
        .map(|(i, op)| {
            let ticket = i as u64;
            match op {
                PerpOp::Place(w) => {
                    let rank = next_rank.entry(w.maker).or_insert(0);
                    let mut w = w.clone();
                    w.ticket = ticket;
                    w.rank = *rank;
                    *rank += 1;
                    PerpOp::Place(w)
                }
                PerpOp::Cancel(w) => {
                    let rank = next_rank.entry(w.canceller).or_insert(0);
                    let mut w = w.clone();
                    w.ticket = ticket;
                    w.rank = *rank;
                    *rank += 1;
                    PerpOp::Cancel(w)
                }
            }
        })
        .collect()
}

/// Execute a block's perp ops with place/cancel running concurrently, producing the SAME net book
/// delta + per-op results as serial txn_id-order execution (step 3d + step-4 segmentation).
///
/// **Segmented parallelism.** The block is processed as a sequence of segments; each segment is
///   1. a parallel batch on the persistent FIFO `pool` over the remaining ops, under FRESH
///      per-segment gates (`AccountGate` + `BookSideLock` + `BboTicketLock` + `PriceCompletion` + a
///      taker-contagion floor), the ops re-normalized to dense segment-local tickets + per-maker ranks
///      ([`normalize_schedule`]) and pre-scanned ([`plan_block`]);
///   2. a FIFO finalize of that segment's parallel rests ([`finalize_place_batch_ordering`]);
///   3. the FIRST op that downgraded — the contagion floor (taker / crossing limit / emptying cancel
///      that forces serialization) — re-run SERIALLY in place against the now-FIFO-sorted book
///      ([`run_barrier_op`]).
/// The strictly-higher tail (all forced to `Downgrade` by contagion, so never executed) is then
/// re-dispatched as the next segment — it RE-PARALLELIZES against the post-serial-op book instead of
/// collapsing onto one end-of-block barrier as in the pre-segmentation driver. A block-end
/// order-independent mid sample (D3-b) runs once over every market touched.
///
/// **Serial-equivalence.** The floor op runs against exactly `[all lower ops applied]` (the segment
/// executed them first) — identical to serial — and the re-dispatched tail then sees `[floor applied]`
/// too. Per-segment gates are mandatory: the downgraded tail already consumed the previous segment's
/// tickets/ranks, so reusing those gates would double-consume (fail-stop). Segment-local tickets stay
/// globally FIFO-correct because [`finalize_place_batch_ordering`] preserves prior level order and only
/// appends THIS segment's rests, and segments run in block order.
///
/// `ops` must be in txn_id order. Returns results in that order. `pool` is reused across segments (and,
/// by the caller, across blocks).
fn transact_block_parallel_inner<CTX, F>(
    pool: &PerpPool,
    book: &Arc<SharedPerpBook>,
    ops: &[PerpOp],
    make_ctx: F,
    prof: Option<Arc<PerpBlockProfile>>,
) -> Result<Vec<OpReplay>, PrecompileError>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX + Clone + Send + 'static,
{
    // ONE pass (step 4 — replaces the segmented re-dispatch): normalize to global dense tickets +
    // per-maker ranks + pre-scan ONCE, then ONE parallel batch. Inline takers execute under the held
    // BBO ticket (no downgrade), so nothing is deferred / re-dispatched and the gates + plan are built
    // exactly once. Each op is served + classified + executed exactly once → O(n) (no per-taker tail
    // re-march/re-plan).
    let ops = normalize_schedule(ops);
    let prepared = plan_block(book, &ops, &make_ctx);

    let mut markets_touched = std::collections::BTreeSet::new();
    for p in &prepared {
        match p {
            PreparedOp::Place(w) => {
                markets_touched.insert(w.market_id);
            }
            PreparedOp::Cancel(c) => {
                if let Some((m, _, _)) = c.resolved {
                    markets_touched.insert(m);
                }
            }
        }
    }

    // The single market this batch operates on (spike scope) — also the completion key for a
    // None-resolved cancel (which has no market of its own). 0 if the block is all-unresolvable
    // cancels (then nothing waits on the completion).
    let market = prepared
        .iter()
        .find_map(|p| match p {
            PreparedOp::Place(w) => Some(w.market_id),
            PreparedOp::Cancel(c) => c.resolved.map(|(m, _, _)| m),
        })
        .unwrap_or(0);

    // Gates + inline-taker machinery, built ONCE; Arc'd so the pool's 'static jobs can share them.
    // `placed_tickets` ({order_id -> ticket} for every place, incl. a taker's own remainder) is the
    // on-demand FIFO sort key; `dirty` tracks levels needing that sort; `completion` is the per-market
    // watermark inline takers / emptying cancels wait on.
    let placed_tickets: HashMap<[u8; 32], u64> = prepared
        .iter()
        .filter_map(|p| match p {
            PreparedOp::Place(w) => Some((w.order_id, w.ticket)),
            PreparedOp::Cancel(_) => None,
        })
        .collect();
    let bbo = BboTicketLock::new();
    if prof.is_some() {
        bbo.set_profiled(true); // §B: time the producer-side serve-cursor advance (notify_all)
    }
    let shared = Arc::new((
        AccountGate::new(),
        BookSideLock::new(),
        bbo,
        MarketCompletion::new(),
        Mutex::new(HashSet::<(u8, u64, u64)>::new()),
        placed_tickets,
        prof, // element .6: Option<Arc<PerpBlockProfile>> (Some under PERP_PROF)
        Mutex::new(HashMap::<u64, (u64, u64)>::new()), // element .7: the best (bid, ask) mirror
    ));
    if shared.6.is_some() {
        // §B/§F: enable the sched-lock wait timers + level insert/hit counting for this block.
        shared.0.set_profiled(true); // AccountGate (acct_wait)
        shared.1.set_profiled(true); // BookSideLock (book_wait + level inserts/hits)
        shared.3.set_profiled(true); // MarketCompletion (completion_wait)
    }

    let final_results: Vec<OpReplay> = {
        let tasks: Vec<_> = prepared
            .iter()
            .map(|p| {
                let p = p.clone();
                let book = book.clone();
                let make_ctx = make_ctx.clone();
                let shared = shared.clone();
                move || -> Result<OpReplay, PrecompileError> {
                    let (
                        account_gate,
                        book_lock,
                        bbo,
                        completion,
                        dirty,
                        placed_tickets,
                        prof,
                        best_mirror,
                    ) = &*shared;
                    let env = BatchEnv {
                        market,
                        account_gate,
                        book_lock,
                        bbo,
                        completion,
                        dirty,
                        placed_tickets,
                        best_mirror,
                        prof: prof.as_deref(),
                    };
                    let mut ctx = make_ctx(book);
                    let result = match &p {
                        PreparedOp::Place(w) => {
                            OpResult::Place(parallel_place(&mut ctx, &env, w)?)
                        }
                        PreparedOp::Cancel(plan) => {
                            OpResult::Cancel(parallel_cancel(&mut ctx, &env, plan)?)
                        }
                    };
                    // Drain THIS op's emitted logs from its slot ctx (empty for a reverted op — logs
                    // rolled back). Re-emitted verbatim on replay so canonical receipts match serial.
                    let logs = ctx.journal_mut().take_logs();
                    Ok(OpReplay { result, logs })
                }
            })
            .collect();
        let phase_start = Instant::now(); // §1/§H: time the whole parallel batch
        let res = pool
            .run_batch(tasks)
            .into_iter()
            .collect::<Result<Vec<_>, _>>();
        if let Some(p) = &shared.6 {
            p.phase_wall_ns
                .store(phase_start.elapsed().as_nanos() as u64, Ordering::Relaxed);
            p.n_ops.store(ops.len() as u64, Ordering::Relaxed);
            // §B: harvest the producer-side serve-cursor advance (notify_all) total from the ticket lock.
            p.advance_notify_ns
                .store(shared.2.advance_ns(), Ordering::Relaxed);
            // §B rest + §F: harvest the other sched-lock waits + level insert/hit counts.
            p.acct_wait_ns.store(shared.0.wait_ns(), Ordering::Relaxed);
            p.book_wait_ns.store(shared.1.wait_ns(), Ordering::Relaxed);
            p.completion_wait_ns
                .store(shared.3.wait_ns(), Ordering::Relaxed);
            let (ins, hits) = shared.1.level_lock_stats();
            p.level_inserts.store(ins, Ordering::Relaxed);
            p.level_hits.store(hits, Ordering::Relaxed);
        }
        res?
    };

    // FIFO-sort the FINAL book: inline takers sorted (via `dirty`) the levels they swept just-in-time,
    // but rests after the last taker — and levels no taker ever read — may still hold racy arrival
    // order; restore time-priority for the next block / view reads. Idempotent (sorted levels skip).
    {
        let mut place_items: Vec<PlaceWork> = Vec::new();
        let mut place_results: Vec<Result<PlaceOutcome, PrecompileError>> = Vec::new();
        for (op, r) in ops.iter().zip(&final_results) {
            if let (PerpOp::Place(w), OpResult::Place(o)) = (op, &r.result) {
                place_items.push(w.clone());
                place_results.push(Ok(*o));
            }
        }
        finalize_place_batch_ordering(book, &place_items, &place_results, &make_ctx)?;
    }

    // Block-end order-independent mid sample (D3-b) for every market the block touched.
    let mut barrier_ctx = make_ctx(book.clone());
    for m in markets_touched {
        crate::perp_dex::risk::finalize_block_mid_sample(&mut barrier_ctx, m)?;
    }

    Ok(final_results)
}

/// Public driver (no profiling) — the node's normal path. Returns per-op replays (with logs).
pub fn transact_block_parallel_logged<CTX, F>(
    pool: &PerpPool,
    book: &Arc<SharedPerpBook>,
    ops: &[PerpOp],
    make_ctx: F,
) -> Result<Vec<OpReplay>, PrecompileError>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX + Clone + Send + 'static,
{
    transact_block_parallel_inner(pool, book, ops, make_ctx, None)
}

/// Profiled driver (catalog #21 `PERP_PROF`): identical execution to [`transact_block_parallel_logged`]
/// but returns the per-block [`PerpBlockProfile`] (achieved concurrency + classification histogram +
/// block totals) for the node to emit as the `PERP_PROF …` line. Profiling adds one `Instant` + a few
/// relaxed atomics per op; the non-profiled path is unaffected.
pub fn transact_block_parallel_logged_profiled<CTX, F>(
    pool: &PerpPool,
    book: &Arc<SharedPerpBook>,
    ops: &[PerpOp],
    make_ctx: F,
) -> Result<(Vec<OpReplay>, Arc<PerpBlockProfile>), PrecompileError>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX + Clone + Send + 'static,
{
    let prof = Arc::new(PerpBlockProfile::default());
    let out = transact_block_parallel_inner(pool, book, ops, make_ctx, Some(prof.clone()))?;
    Ok((out, prof))
}

/// Outcome-only wrapper around [`transact_block_parallel_logged`] — drops the captured per-op logs.
/// The node uses the `_logged` variant (it needs the logs to replay perp events); this convenience
/// form is for callers/tests that only assert outcomes + the byte-identical state delta.
pub fn transact_block_parallel<CTX, F>(
    pool: &PerpPool,
    book: &Arc<SharedPerpBook>,
    ops: &[PerpOp],
    make_ctx: F,
) -> Result<Vec<OpResult>, PrecompileError>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX + Clone + Send + 'static,
{
    Ok(transact_block_parallel_logged(pool, book, ops, make_ctx)?
        .into_iter()
        .map(|r| r.result)
        .collect())
}

/// SERIAL reference execution of a block's perp ops into the shared `book`: each op via its normal
/// place/cancel core under a checkpoint (commit on Ok; propagate `Fatal`; else `Reverted`), capturing
/// per-op logs, then the same block-end mid sample the parallel driver does. Produces the per-op
/// results + logs + (via `book.take_delta()`) the net delta that TRUE serial execution would, given
/// the same cold-read — the ground truth the parallel path must match.
///
/// This is the diff target for the node's `PERP_PARALLEL_AUDIT` (a debug cross-check that re-runs each
/// block's ops serially and compares state + events against the parallel path to localize a node-side
/// divergence). NOT used on the production hot path. `ops` must be in txn_id order.
pub fn transact_block_serial<CTX, F>(
    book: &Arc<SharedPerpBook>,
    ops: &[PerpOp],
    make_ctx: F,
) -> Result<Vec<OpReplay>, PrecompileError>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX,
{
    let mut ctx = make_ctx(book.clone());
    let mut out = Vec::with_capacity(ops.len());
    let mut markets_touched = std::collections::BTreeSet::new();
    for op in ops {
        let cp = ctx.journal_mut().checkpoint();
        let result = match op {
            PerpOp::Place(w) => {
                markets_touched.insert(w.market_id);
                let r = place_order_core(
                    w.maker,
                    w.order_id,
                    w.market_id,
                    w.side,
                    w.price,
                    w.qty,
                    w.order_type,
                    w.tif,
                    w.client_order_id,
                    &mut ctx,
                );
                OpResult::Place(match dispose_body(&mut ctx, cp, r)? {
                    BodyDisposition::Committed => PlaceOutcome::Executed,
                    BodyDisposition::Reverted => PlaceOutcome::Reverted,
                })
            }
            PerpOp::Cancel(w) => {
                let r = cancel_order_core(w.canceller, w.order_id, &mut ctx);
                OpResult::Cancel(match dispose_body(&mut ctx, cp, r)? {
                    BodyDisposition::Committed => CancelOutcome::Executed,
                    BodyDisposition::Reverted => CancelOutcome::Reverted,
                })
            }
        };
        let logs = ctx.journal_mut().take_logs();
        out.push(OpReplay { result, logs });
    }
    // Same block-end order-independent mid sample as the parallel driver, so the two deltas match on
    // the price-basis window too (it is the only per-op-vs-block-end-differing artifact).
    for m in markets_touched {
        crate::perp_dex::risk::finalize_block_mid_sample(&mut ctx, m)?;
    }
    Ok(out)
}

#[cfg(test)]
mod driver_tests {
    use super::*;
    use crate::perp_dex::types::Market;
    use context::journal::shared_perp::SharedPerpBook;
    use context::{BlockEnv, CfgEnv, Context, Journal, TxEnv};
    use database::InMemoryDB;
    use primitives::{address, hardfork::SpecId};
    use std::sync::Arc;

    type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

    /// A single persistent FIFO pool shared by every driver test — exactly how the node will reuse one
    /// pool across blocks. 8 workers exposes the place/cancel races the differential tests probe for.
    fn test_pool() -> &'static PerpPool {
        static POOL: std::sync::OnceLock<PerpPool> = std::sync::OnceLock::new();
        POOL.get_or_init(|| PerpPool::new(8))
    }

    /// classify_perp_tx dispatch (step 4b pre-phase): a direct place/cancel → Trade with the right
    /// PerpOp (maker/canceller = caller, fields preserved, place replays the orderId / cancel replays
    /// empty); a trading selector with bad args → Reject; a non-trading / too-short input → NotTrading.
    /// (The signed verify_* paths reuse the same helpers exercised by the golden scenario.)
    #[test]
    fn classify_perp_tx_dispatches_place_cancel_reject_and_nontrading() {
        use crate::perp_dex::interface::IPerpDex::{cancelOrderCall, placeOrderCall};
        use alloy_sol_types::SolCall;
        use primitives::FixedBytes;

        let a = user_addr(1);
        let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut ctx, &[a]);

        // Direct placeOrder → Trade(Place), maker = caller, fields preserved, non-empty success output.
        let place_input = placeOrderCall {
            marketId: MID,
            side: 0,
            price: 100 * TICK,
            quantity: QTY,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes([0u8; 16]),
        }
        .abi_encode();
        match classify_perp_tx(&place_input, a, &mut ctx).unwrap() {
            PerpTxClass::Trade {
                op: PerpOp::Place(w),
                success_output,
            } => {
                assert_eq!(w.maker, a);
                assert_eq!(w.market_id, MID);
                assert_eq!(w.side, 0);
                assert_eq!(w.price, 100 * TICK);
                assert_eq!(w.qty, QTY);
                assert!(!success_output.is_empty(), "place replays the orderId");
            }
            other => panic!("expected Trade(Place), got {other:?}"),
        }

        // Direct cancelOrder → Trade(Cancel), canceller = caller, empty success output.
        let cancel_input = cancelOrderCall {
            orderId: FixedBytes(oid(7)),
            marketId: MID,
        }
        .abi_encode();
        match classify_perp_tx(&cancel_input, a, &mut ctx).unwrap() {
            PerpTxClass::Trade {
                op: PerpOp::Cancel(c),
                success_output,
            } => {
                assert_eq!(c.canceller, a);
                assert_eq!(c.order_id, oid(7));
                assert!(success_output.is_empty(), "cancel replays empty");
            }
            other => panic!("expected Trade(Cancel), got {other:?}"),
        }

        // A trading selector with garbage args → Reject (replayed as a revert).
        let mut bad = placeOrderCall::SELECTOR.to_vec();
        bad.extend_from_slice(&[0u8; 8]);
        assert!(matches!(
            classify_perp_tx(&bad, a, &mut ctx).unwrap(),
            PerpTxClass::Reject { .. }
        ));

        // A non-trading 4-byte selector and a too-short input → NotTrading (serial pass handles them).
        assert!(matches!(
            classify_perp_tx(&[0xAA, 0xBB, 0xCC, 0xDD], a, &mut ctx).unwrap(),
            PerpTxClass::NotTrading
        ));
        assert!(matches!(
            classify_perp_tx(&[0x01, 0x02], a, &mut ctx).unwrap(),
            PerpTxClass::NotTrading
        ));
    }

    /// #3 (parallel-decode) equivalence: `classify_perp_tx_pending` (read-only, run per-tx as the
    /// concurrent pre-phase does — all direct places see the SAME committed base nonce) + the serial
    /// `finalize_pending_classes` must produce the EXACT same `PerpTxClass` list (ops + success_output,
    /// hence order_ids) as a serial `classify_perp_tx` scan (which does the nonce read-modify-write
    /// inline). The load-bearing case: MULTIPLE direct places from ONE account must get sequential ids
    /// base, base+1, … even though the parallel decode read the same base for all of them — the serial
    /// finalize assigns them by a per-maker running counter in txn order.
    #[test]
    fn classify_pending_then_finalize_matches_serial_classify() {
        use crate::perp_dex::interface::IPerpDex::{cancelOrderCall, placeOrderCall};
        use alloy_sol_types::SolCall;
        use primitives::FixedBytes;

        let a = user_addr(1);
        let b = user_addr(2);
        let place = |caller: Address, side: u8, price: u64| {
            (
                placeOrderCall {
                    marketId: MID,
                    side,
                    price,
                    quantity: QTY,
                    orderType: 0,
                    tif: 0,
                    clientOrderId: FixedBytes([0u8; 16]),
                }
                .abi_encode(),
                caller,
            )
        };
        let cancel = |caller: Address, o: [u8; 32]| {
            (
                cancelOrderCall {
                    orderId: FixedBytes(o),
                    marketId: MID,
                }
                .abi_encode(),
                caller,
            )
        };
        // Interleaved block: A×2 direct places, B cancel, B place, A place → A places 3 total, so its
        // ids MUST be base, base+1, base+2 in txn order (the ids are 0th, 1st, 4th txs).
        let txs: Vec<(Vec<u8>, Address)> = vec![
            place(a, 0, 100 * TICK),
            place(a, 0, 99 * TICK),
            cancel(b, oid(7)),
            place(b, 1, 200 * TICK),
            place(a, 0, 98 * TICK),
        ];

        // SERIAL reference: classify_perp_tx does the nonce RMW inline, one ctx, in txn order.
        let mut serial_ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial_ctx, &[a, b]);
        let serial_classes: Vec<PerpTxClass> = txs
            .iter()
            .map(|(cd, c)| classify_perp_tx(cd, *c, &mut serial_ctx).unwrap())
            .collect();

        // PARALLEL: classify_perp_tx_pending is read-only, so running every tx on ONE ctx yields the
        // SAME committed base nonce for all of A's places (exactly what N concurrent cold-read ctxs
        // would each see) — no cross-tx nonce visibility. finalize then assigns ids serially.
        let mut pctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut pctx, &[a, b]);
        let pending: Vec<PendingPerpTx> = txs
            .iter()
            .map(|(cd, c)| classify_perp_tx_pending(cd, *c, &mut pctx).unwrap())
            .collect();
        // Sanity: A's three direct places all read the SAME base nonce in the parallel decode.
        let a_bases: Vec<u64> = pending
            .iter()
            .filter_map(|p| match p {
                PendingPerpTx::PendingPlace {
                    maker, base_nonce, ..
                } if *maker == a => Some(*base_nonce),
                _ => None,
            })
            .collect();
        assert_eq!(a_bases, vec![0, 0, 0], "parallel decode reads committed base for all");

        let mut fctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut fctx, &[a, b]);
        let parallel_classes = finalize_pending_classes(pending, &mut fctx).unwrap();

        assert_eq!(
            serial_classes, parallel_classes,
            "parallel classify+finalize must be byte-identical to serial classify"
        );

        // Explicit: A's three places carry sequential keccak(a‖0/1/2) ids in txn order.
        let a_ids: Vec<[u8; 32]> = parallel_classes
            .iter()
            .filter_map(|c| match c {
                PerpTxClass::Trade {
                    op: PerpOp::Place(w),
                    ..
                } if w.maker == a => Some(w.order_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            a_ids,
            vec![
                compute_order_id(a, 0),
                compute_order_id(a, 1),
                compute_order_id(a, 2)
            ],
        );
        // And the final committed nonce for A = base + 3 (matches serial's three +1s).
        assert_eq!(storage::load_user_nonce(&mut fctx, a).unwrap(), 3);
        assert_eq!(storage::load_user_nonce(&mut fctx, b).unwrap(), 1);
    }

    /// 3d-7 fix: a NON-OWNER cancel of someone's sole-best order. It reverts ("not owner") touching
    /// nothing, so it runs in parallel and must NOT set the contagion floor (a griefer cancelling
    /// another account's sole-best order would otherwise force the whole tail serial). Exercises the
    /// owner-check branch; the cancel is Reverted and the delta stays byte-identical to serial.
    #[test]
    fn transact_block_parallel_non_owner_sole_best_cancel_matches_serial() {
        let a = user_addr(1); // owner of the sole-best ask
        let b = user_addr(2); // griefer: cancels a's order (not the owner)
        let sa = oid(90);
        let ops = vec![
            mk_place(a, sa, 1, 100 * TICK, 0, 0, 0), // SELL@100, sole best ask
            PerpOp::Cancel(mk_cancel(b, sa, 0, 1)),  // b cancels a's order → not owner → reverts
        ];

        let wkey = storage::keys::price_basis_window_key(MID);
        // Serial reference: the non-owner cancel reverts (no delta) and run_serial_op unwraps, so
        // reference the resting place only (the parallel path applies + rolls back to the same state).
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial, &[a, b]);
        run_serial_op(&mut serial, &ops[0]);
        let mut serial_delta = serial.journal_mut().take_perp_delta();
        serial_delta.remove(&wkey);

        let book = Arc::new(SharedPerpBook::new());
        seed(&mut make_slot(book.clone()), &[a, b]);
        let results = transact_block_parallel(test_pool(), &book, &ops, make_slot).unwrap();
        let mut parallel_delta = book.take_delta();
        parallel_delta.remove(&wkey);

        assert_eq!(results[0], OpResult::Place(PlaceOutcome::Executed));
        assert_eq!(
            results[1],
            OpResult::Cancel(CancelOutcome::Reverted),
            "a non-owner cancel must revert"
        );
        assert_eq!(serial_delta, parallel_delta);
    }

    /// A taker TIF (IOC) now runs INLINE under the held BBO ticket (step 4 — the deferred-barrier
    /// downgrade is gone): it completion-waits (trivially, ticket 0), then matches. It must NOT return
    /// `Downgrade` (that path no longer exists). With ticket 0 the inline `wait_below(.., 0)` returns
    /// immediately, so a single-op batch never hangs.
    #[test]
    fn parallel_place_taker_runs_inline_never_downgrades() {
        let book = Arc::new(SharedPerpBook::new());
        let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        ctx.journal_mut().set_perp_shared(book.clone());
        let gate = AccountGate::new();
        let book_lock = BookSideLock::new();
        let bbo = BboTicketLock::new();
        let completion = MarketCompletion::new();
        let dirty = Mutex::new(HashSet::new());
        let placed_tickets = HashMap::new();
        let best_mirror = Mutex::new(HashMap::new());
        let env = BatchEnv {
            market: 1,
            account_gate: &gate,
            book_lock: &book_lock,
            bbo: &bbo,
            completion: &completion,
            dirty: &dirty,
            placed_tickets: &placed_tickets,
            best_mirror: &best_mirror,
            prof: None,
        };

        let work = PlaceWork {
            maker: address!("1111111111111111111111111111111111111111"),
            order_id: [1u8; 32],
            market_id: 1,
            side: 0, // Buy
            price: 100,
            qty: 1,
            order_type: 0, // Limit
            tif: 1,        // IOC → taker → inline
            client_order_id: [0u8; 16],
            rank: 0,
            ticket: 0,
        };

        let out = parallel_place(&mut ctx, &env, &work).unwrap();
        assert_ne!(
            out,
            PlaceOutcome::Downgrade,
            "takers run inline now and never downgrade"
        );
    }

    const TICK: u64 = 1_000_000_000;
    const QTY: u64 = 1_000_000;
    const WALLET: u64 = 1_000_000_000;
    const MID: u64 = 1;

    fn user_addr(i: u64) -> Address {
        let mut b = [0u8; 20];
        b[12..20].copy_from_slice(&i.to_be_bytes());
        Address::from(b)
    }

    fn test_market() -> Market {
        Market {
            market_id: MID,
            base_decimals: 8,
            price_decimals: 9,
            tick_size: TICK,
            step_size: QTY,
            min_quantity: QTY,
            max_quantity: QTY * 1_000,
            max_price: 100_000 * TICK,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
        }
    }

    fn fund<CTX: ContextTr>(ctx: &mut CTX, user: Address, amt: u64) {
        let mut acc = storage::load_account(ctx, user).unwrap();
        acc.credit_perp(amt).unwrap();
        storage::save_account(ctx, user, acc).unwrap();
    }

    fn seed<CTX: ContextTr>(ctx: &mut CTX, users: &[Address]) {
        storage::save_market(ctx, &test_market()).unwrap();
        for &u in users {
            fund(ctx, u, WALLET);
        }
    }

    fn mk_work(
        maker: Address,
        order_id: [u8; 32],
        price: u64,
        rank: u64,
        ticket: u64,
    ) -> PlaceWork {
        PlaceWork {
            maker,
            order_id,
            market_id: MID,
            side: 0, // Buy
            price,
            qty: QTY,
            order_type: 0, // Limit
            tif: 0,        // GTC
            client_order_id: [0u8; 16],
            rank,
            ticket,
        }
    }

    fn make_slot(bk: Arc<SharedPerpBook>) -> TestCtx {
        let mut c: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        c.journal_mut().set_perp_shared(bk);
        c
    }

    /// Concurrency stress: N non-mover places at the SAME price from N DISTINCT accounts (so the
    /// AccountGate does NOT serialize them) must all land in the level FIFO — no lost append. A
    /// mover at ticket 0 establishes the best so the rest are non-movers (ReleaseTicket → run
    /// outside the BBO ticket). Repeated to shake out the load-modify-store race on the level blob.
    #[test]
    fn parallel_place_same_level_distinct_accounts_no_lost_append() {
        const N: u64 = 8;
        for _round in 0..40 {
            let mover = user_addr(100);
            let makers: Vec<Address> = (1..=N).map(user_addr).collect();

            let mut items = vec![mk_work(mover, [200u8; 32], 100 * TICK, 0, 0)];
            for (i, &m) in makers.iter().enumerate() {
                let mut oid = [0u8; 32];
                oid[0] = (i + 1) as u8;
                // All at 99 (< best 100) → non-movers; rank 0 for each (distinct accounts).
                items.push(mk_work(m, oid, 99 * TICK, 0, (i + 1) as u64));
            }

            let book = Arc::new(SharedPerpBook::new());
            let mut seed_users = vec![mover];
            seed_users.extend(makers.iter().copied());
            seed(&mut make_slot(book.clone()), &seed_users);
            let gate = AccountGate::new();
            let book_lock = BookSideLock::new();
            let bbo = BboTicketLock::new();
            let results = run_place_batch(&book, &gate, &book_lock, &bbo, &items, make_slot);
            for r in &results {
                assert_eq!(*r.as_ref().unwrap(), PlaceOutcome::Executed);
            }

            let mut ctx = make_slot(book.clone());
            let level = storage::load_bid_level(&mut ctx, MID, 99 * TICK).unwrap();
            assert_eq!(
                level.len() as u64,
                N,
                "lost append: level 99 has {} of {} orders (round {_round})",
                level.len(),
                N
            );
        }
    }

    /// THE step-3c gate: a batch of non-crossing places run in parallel + the barrier-sort produces
    /// a book delta byte-identical to the same ops run serially in txn_id order. Crucially includes
    /// TWO non-movers at the SAME non-best price (B@99, C@99): their FIFO append order is racy in the
    /// parallel phase and is corrected to ticket (txn_id) order by `finalize_place_batch_ordering`.
    /// Also: a mover (A@100), same-account ordering (a's two orders A@100 rank 0, C@99 rank 1), and
    /// three makers.
    #[test]
    fn parallel_place_batch_matches_serial_delta() {
        let a = user_addr(1);
        let b = user_addr(2);
        // A@100 (a, mover), B@99 (b), C@99 (a's 2nd) — B and C share level 99.
        let items = vec![
            mk_work(a, [1u8; 32], 100 * TICK, 0, 0),
            mk_work(b, [2u8; 32], 99 * TICK, 0, 1),
            mk_work(a, [3u8; 32], 99 * TICK, 1, 2),
        ];

        // Serial reference: place_order_core in txn_id (ticket) order against a PerpSection ctx.
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial, &[a, b]);
        for it in &items {
            place_order_core(
                it.maker,
                it.order_id,
                it.market_id,
                it.side,
                it.price,
                it.qty,
                it.order_type,
                it.tif,
                it.client_order_id,
                &mut serial,
            )
            .unwrap();
        }
        let serial_delta = serial.journal_mut().take_perp_delta();

        // Parallel: same ops via the batch runner against one shared book, then the barrier-sort.
        let book = Arc::new(SharedPerpBook::new());
        seed(&mut make_slot(book.clone()), &[a, b]);
        let gate = AccountGate::new();
        let book_lock = BookSideLock::new();
        let bbo = BboTicketLock::new();
        let results = run_place_batch(&book, &gate, &book_lock, &bbo, &items, make_slot);
        for r in &results {
            assert_eq!(*r.as_ref().unwrap(), PlaceOutcome::Executed);
        }
        finalize_place_batch_ordering(&book, &items, &results, make_slot).unwrap();
        let parallel_delta = book.take_delta();

        // Batch-level gate: compare the BOOK delta. price_basis_window is a BLOCK-end artifact — the
        // parallel batch skips the in-body (order-dependent) sample (perp_is_parallel) and a bare
        // batch runner does no block-end sample, so it is absent here while the serial ref still wrote
        // it in-body. The order-independent block-end sample is covered by the block-driver test.
        let (mut s, mut p) = (serial_delta, parallel_delta);
        let wkey = storage::keys::price_basis_window_key(MID);
        s.remove(&wkey);
        p.remove(&wkey);
        assert_eq!(s, p);
    }

    // ── cancel driver ─────────────────────────────────────────────────────────────

    fn oid(n: u8) -> [u8; 32] {
        let mut b = [0u8; 32];
        b[0] = n;
        b
    }

    /// Rest a GTC buy limit at `price` (setup helper for the cancel tests).
    fn rest<CTX: ContextTr>(ctx: &mut CTX, maker: Address, order_id: [u8; 32], price: u64) {
        place_order_core(maker, order_id, MID, 0, price, QTY, 0, 0, [0u8; 16], ctx).unwrap();
    }

    fn mk_cancel(canceller: Address, order_id: [u8; 32], rank: u64, ticket: u64) -> CancelWork {
        CancelWork {
            canceller,
            order_id,
            rank,
            ticket,
        }
    }

    /// THE cancel step-3c gate: a batch of parallel-eligible cancels (a below-best cancel that empties
    /// a non-best level + two at-best cancels that do NOT empty the best level) run in parallel
    /// produces a book delta byte-identical to the same cancels run serially in txn_id order.
    /// Book: best bid level 100 = [X(a), Y(b), Z(c)], level 99 = [W(a)]. Cancel X, Y, W (Z stays).
    /// X is at-best non-emptying; Y is at-best non-emptying (required = {X}); W is below-best.
    #[test]
    fn parallel_cancel_batch_matches_serial_delta() {
        let a = user_addr(1);
        let b = user_addr(2);
        let c = user_addr(3);
        let (x, y, z, w) = (oid(10), oid(11), oid(12), oid(13));

        // Serial reference: setup places + cancels in one overlay, one net delta.
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial, &[a, b, c]);
        rest(&mut serial, a, x, 100 * TICK);
        rest(&mut serial, b, y, 100 * TICK);
        rest(&mut serial, c, z, 100 * TICK);
        rest(&mut serial, a, w, 99 * TICK);
        cancel_order_core(a, x, &mut serial).unwrap();
        cancel_order_core(b, y, &mut serial).unwrap();
        cancel_order_core(a, w, &mut serial).unwrap();
        let serial_delta = serial.journal_mut().take_perp_delta();

        // Parallel: identical setup placed serially into the shared book, then the cancels in a batch.
        let book = Arc::new(SharedPerpBook::new());
        {
            let mut s = make_slot(book.clone());
            seed(&mut s, &[a, b, c]);
            rest(&mut s, a, x, 100 * TICK);
            rest(&mut s, b, y, 100 * TICK);
            rest(&mut s, c, z, 100 * TICK);
            rest(&mut s, a, w, 99 * TICK);
        }
        let cancels = vec![
            mk_cancel(a, x, 0, 0),
            mk_cancel(b, y, 0, 1),
            mk_cancel(a, w, 1, 2),
        ];
        let gate = AccountGate::new();
        let book_lock = BookSideLock::new();
        let bbo = BboTicketLock::new();
        let results = run_cancel_batch(&book, &gate, &book_lock, &bbo, &cancels, make_slot);
        for r in &results {
            assert_eq!(*r.as_ref().unwrap(), CancelOutcome::Executed);
        }
        let parallel_delta = book.take_delta();

        // See the place batch test: price_basis_window is a block-end artifact, carved out of this
        // batch-level comparison (covered by the block-driver test).
        let (mut s, mut p) = (serial_delta, parallel_delta);
        let wkey = storage::keys::price_basis_window_key(MID);
        s.remove(&wkey);
        p.remove(&wkey);
        assert_eq!(s, p);
    }

    /// An at-best OWNER cancel that is the SOLE order at the best level now runs INLINE under the held
    /// BBO ticket (step 4 — the deferred-barrier downgrade is gone): it removes the order + refreshes
    /// best in place. Proven byte-identical to a SERIAL cancel of the same order (outcome + book delta).
    #[test]
    fn parallel_cancel_sole_best_order_runs_inline_matches_serial() {
        let a = user_addr(1);
        let x = oid(10);

        // Reference: place X, then cancel X SERIALLY (the engine's own entrypoint).
        let ref_book = Arc::new(SharedPerpBook::new());
        {
            let mut s = make_slot(ref_book.clone());
            seed(&mut s, &[a]);
            rest(&mut s, a, x, 100 * TICK);
            cancel_order_core(a, x, &mut s).unwrap();
        }
        let ref_delta = ref_book.take_delta();

        // Subject: place X, then cancel X via the parallel batch (an inline at-best owner cancel).
        let book = Arc::new(SharedPerpBook::new());
        {
            let mut s = make_slot(book.clone());
            seed(&mut s, &[a]);
            rest(&mut s, a, x, 100 * TICK);
        }
        let cancels = vec![mk_cancel(a, x, 0, 0)];
        let gate = AccountGate::new();
        let book_lock = BookSideLock::new();
        let bbo = BboTicketLock::new();
        let results = run_cancel_batch(&book, &gate, &book_lock, &bbo, &cancels, make_slot);
        assert_eq!(results[0].as_ref().unwrap(), &CancelOutcome::Executed);

        let subject_delta = book.take_delta();
        assert_eq!(
            ref_delta, subject_delta,
            "an inline at-best owner cancel must produce the same book delta as a serial cancel"
        );
    }

    // ── unified block driver (step 3d) ────────────────────────────────────────────

    /// Run one block op via its normal SERIAL entrypoint (the differential reference).
    fn run_serial_op<CTX: ContextTr>(ctx: &mut CTX, op: &PerpOp) {
        match op {
            PerpOp::Place(w) => {
                place_order_core(
                    w.maker,
                    w.order_id,
                    w.market_id,
                    w.side,
                    w.price,
                    w.qty,
                    w.order_type,
                    w.tif,
                    w.client_order_id,
                    ctx,
                )
                .unwrap();
            }
            PerpOp::Cancel(w) => {
                cancel_order_core(w.canceller, w.order_id, ctx).unwrap();
            }
        }
    }

    /// Step-4b log capture (the SERVER-found bug, commit 0310403aa): the parallel path must emit the
    /// SAME per-tx EVM logs (OrderPlaced / Trade / OrderRested / PositionChanged) as serial — the
    /// matching runs in throwaway driver contexts, so `transact_block_parallel_logged` drains each op's
    /// logs and the node re-emits them on replay. Before the capture fix these were dropped, so
    /// parallelized blocks emitted ZERO perp events → the event-driven verifier's MISSING/RESTED
    /// cascade. Asserts the captured per-op logs equal a serial run's per-op logs.
    #[test]
    fn transact_block_parallel_logged_captures_same_logs_as_serial() {
        use primitives::Log;
        let a = user_addr(1);
        let b = user_addr(2);
        let ops = vec![
            mk_place(a, oid(70), 1, 100 * TICK, 0, 0, 0), // SELL@100 → rests (OrderPlaced)
            mk_place(b, oid(71), 0, 100 * TICK, 0, 0, 1), // BUY@100 taker → fills A (OrderPlaced+Trade+…)
        ];

        // Serial reference: each op's emitted logs (run_serial_op emits into the journal; drain per op).
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial, &[a, b]);
        let serial_logs: Vec<Vec<Log>> = ops
            .iter()
            .map(|op| {
                run_serial_op(&mut serial, op);
                serial.journal_mut().take_logs()
            })
            .collect();
        assert!(
            serial_logs.iter().any(|l| !l.is_empty()),
            "scenario must emit perp logs or the test has no teeth"
        );

        // Parallel: the per-op logs the driver captured from its throwaway matching contexts.
        let book = Arc::new(SharedPerpBook::new());
        seed(&mut make_slot(book.clone()), &[a, b]);
        let replays =
            transact_block_parallel_logged(test_pool(), &book, &ops, make_slot).unwrap();
        let parallel_logs: Vec<Vec<Log>> = replays.iter().map(|r| r.logs.clone()).collect();

        assert_eq!(
            parallel_logs, serial_logs,
            "parallel per-op logs must equal serial (the node replays them → receipts match)"
        );
    }

    /// A cold-read store standing in for reth's committed off-trie `canonical_perp`: `perp_storage` is
    /// served from a seeded map (a PRIOR block's `take_perp_delta`); everything else is empty. Lets the
    /// driver tests exercise CROSS-BLOCK cold-read consumption, which `make_slot`'s empty InMemoryDB
    /// cannot (its cold-read is always empty, so every prior test only matched THIS-block book entries).
    #[derive(Clone, Default)]
    struct ColdPerpDb {
        perp: context::journaled_state::PerpDelta,
    }
    impl database::Database for ColdPerpDb {
        type Error = core::convert::Infallible;
        fn basic(
            &mut self,
            _: primitives::Address,
        ) -> Result<Option<state::AccountInfo>, Self::Error> {
            Ok(None)
        }
        fn code_by_hash(&mut self, _: primitives::B256) -> Result<bytecode::Bytecode, Self::Error> {
            Ok(bytecode::Bytecode::default())
        }
        fn storage(
            &mut self,
            _: primitives::Address,
            _: primitives::StorageKey,
        ) -> Result<primitives::StorageValue, Self::Error> {
            Ok(primitives::StorageValue::ZERO)
        }
        fn block_hash(&mut self, _: u64) -> Result<primitives::B256, Self::Error> {
            Ok(primitives::B256::ZERO)
        }
        fn perp_storage(&mut self, key: primitives::B256) -> Result<std::vec::Vec<u8>, Self::Error> {
            Ok(self.perp.get(&key).cloned().unwrap_or_default())
        }
    }
    type ColdCtx = Context<BlockEnv, TxEnv, CfgEnv, ColdPerpDb, Journal<ColdPerpDb>, ()>;

    /// THE step-4b cold-read gate (the residual server bug after the log-capture fix): a taker crossing
    /// a queue of makers that rested in a PRIOR block — so they live in the cold-read committed store,
    /// NOT the this-block shared-book overlay — must consume them in FIFO time-priority, byte-identical
    /// to serial. In production matchingPair/realisticMix failed with pure `TRADE_MISMATCH` (wrong
    /// maker) while in-block matching + place/cancel passed, isolating the bug to cold-read maker
    /// consumption — the exact gap no prior driver test covered. Scenario mixes a this-block rest (m3)
    /// with the cold-read prior queue (m1, m2) and three BUY takers that consume m1, m2, m3 in order.
    #[test]
    fn transact_block_parallel_cold_read_queue_taker_fifo_matches_serial() {
        let p = 100 * TICK;
        let (m1, m2, m3) = (user_addr(1), user_addr(2), user_addr(3));
        let (t1, t2, t3) = (user_addr(11), user_addr(12), user_addr(13));
        let all = [m1, m2, m3, t1, t2, t3];

        // Prior block: m1, m2 rest SELL@P (FIFO) into the committed cold-read store; all accounts funded.
        let mut prior: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut prior, &all);
        place_order_core(m1, oid(1), MID, 1, p, QTY, 0, 0, [0u8; 16], &mut prior).unwrap();
        place_order_core(m2, oid(2), MID, 1, p, QTY, 0, 0, [0u8; 16], &mut prior).unwrap();
        let committed = prior.journal_mut().take_perp_delta();

        // This block (txn_id order): m3 rests SELL@P (joins the ask), then three BUY@P takers must
        // consume the FIFO-oldest first → m1, m2, m3.
        let ops = vec![
            mk_place(m3, oid(3), 1, p, 0, 0, 0),  // SELL@P → rests (ask = [m1, m2, m3])
            mk_place(t1, oid(11), 0, p, 0, 0, 1), // BUY@P taker → fills m1
            mk_place(t2, oid(12), 0, p, 0, 0, 2), // BUY@P taker → fills m2
            mk_place(t3, oid(13), 0, p, 0, 0, 3), // BUY@P taker → fills m3
        ];

        // Serial reference over the cold-read committed store.
        let mut serial: ColdCtx =
            Context::new(ColdPerpDb { perp: committed.clone() }, SpecId::CANCUN);
        for op in &ops {
            run_serial_op(&mut serial, op);
        }
        let serial_delta = serial.journal_mut().take_perp_delta();

        // Parallel block driver: a FRESH book over the SAME cold-read store (mirrors the node — prior
        // makers are NOT in the this-block book overlay, only the cold-read).
        let committed2 = committed.clone();
        let make_cold_slot = move |bk: Arc<SharedPerpBook>| -> ColdCtx {
            let mut c: ColdCtx =
                Context::new(ColdPerpDb { perp: committed2.clone() }, SpecId::CANCUN);
            c.journal_mut().set_perp_shared(bk);
            c
        };
        let book = Arc::new(SharedPerpBook::new());
        let results = transact_block_parallel(test_pool(), &book, &ops, make_cold_slot).unwrap();
        let parallel_delta = book.take_delta();

        for (i, r) in results.iter().enumerate() {
            assert_eq!(*r, OpResult::Place(PlaceOutcome::Executed), "op {i} should execute");
        }
        // Carve out the in-body-vs-block-end mid sample artifact (covered by the mixed-block test); the
        // maker-fill state is what this gate proves.
        let (mut s, mut pd) = (serial_delta, parallel_delta);
        let wkey = storage::keys::price_basis_window_key(MID);
        s.remove(&wkey);
        pd.remove(&wkey);
        assert_eq!(s, pd, "cold-read taker consumption must be FIFO-identical to serial");
    }

    /// THE step-4b residual bug, caught by the node PERP_PARALLEL_AUDIT: a block that is JUST a cancel
    /// of an order resting from a PRIOR block (so the order + its level live in the cold-read committed
    /// store, NOT the this-block book overlay) must produce the SAME committed state as serial. The
    /// audit found single-cancel blocks with state_diff=true, logs_diff=false, diverging_keys=1 — the
    /// parallel cancel path leaves the book in a different state than serial for a cold-read order,
    /// which a later block's match then consumes wrong (the downstream TRADE_MISMATCH). Covers: a sole
    /// order at the best level (cancel empties it → BBO moves), and one of several at the same level.
    #[test]
    fn cold_read_lone_cancel_matches_serial() {
        let p = 100 * TICK;
        let (m1, m2) = (user_addr(1), user_addr(2));

        // scenario 0: sole SELL@P at best (cancel empties the level → BBO moves)
        // scenario 1: two SELLs@P, cancel the FIRST (FIFO-oldest); scenario 2: cancel the SECOND
        for scenario in 0..3 {
            let mut prior: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
            seed(&mut prior, &[m1, m2]);
            place_order_core(m1, oid(1), MID, 1, p, QTY, 0, 0, [0u8; 16], &mut prior).unwrap();
            if scenario >= 1 {
                place_order_core(m2, oid(2), MID, 1, p, QTY, 0, 0, [0u8; 16], &mut prior).unwrap();
            }
            let committed = prior.journal_mut().take_perp_delta();

            // Block 2: a LONE cancel of a cold-read order.
            let (canceller, target) = if scenario == 2 { (m2, oid(2)) } else { (m1, oid(1)) };
            let ops = vec![PerpOp::Cancel(mk_cancel(canceller, target, 0, 0))];

            // Serial reference (same block-end sample as the parallel driver → window matches).
            let c_ser = committed.clone();
            let make_ser = move |bk: Arc<SharedPerpBook>| -> ColdCtx {
                let mut c: ColdCtx = Context::new(ColdPerpDb { perp: c_ser.clone() }, SpecId::CANCUN);
                c.journal_mut().set_perp_shared(bk);
                c
            };
            let book_s = Arc::new(SharedPerpBook::new());
            transact_block_serial(&book_s, &ops, make_ser).unwrap();
            let serial_delta = book_s.take_delta();

            // Parallel driver over the SAME cold-read.
            let c_par = committed.clone();
            let make_par = move |bk: Arc<SharedPerpBook>| -> ColdCtx {
                let mut c: ColdCtx = Context::new(ColdPerpDb { perp: c_par.clone() }, SpecId::CANCUN);
                c.journal_mut().set_perp_shared(bk);
                c
            };
            let book_p = Arc::new(SharedPerpBook::new());
            transact_block_parallel(test_pool(), &book_p, &ops, make_par).unwrap();
            let parallel_delta = book_p.take_delta();

            assert_committed_eq(&parallel_delta, &serial_delta, scenario);
        }
    }

    /// Direct answer to "is a block with ZERO matches still FIFO-sorted before commit?" — YES. The
    /// segmented driver runs `finalize_place_batch_ordering` UNCONDITIONALLY per segment, BEFORE the
    /// no-downgrade `break`, so a no-match block (one segment, all rests) still has every touched
    /// level's this-block inserts re-sorted by ticket before `take_delta`. Here: 8 DISTINCT accounts
    /// (so the AccountGate does not serialize them → racy parallel appends) rest SELL@P with NO bids to
    /// cross → zero matches. The committed book delta must still equal serial (a FIFO level). If the
    /// sort were tied to "a match happened", this block would commit a racy level. 20 rounds shake the
    /// append race.
    #[test]
    fn no_match_block_still_fifo_sorts_before_commit() {
        let p = 100 * TICK;
        let makers: Vec<Address> = (1..=8).map(user_addr).collect();
        let block: Vec<PerpOp> = makers
            .iter()
            .enumerate()
            .map(|(i, &m)| mk_place(m, oid((i + 1) as u8), 1 /* SELL */, p, 0, 0, 0))
            .collect();

        for _round in 0..20 {
            // Serial reference.
            let mut s: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
            seed(&mut s, &makers);
            for op in &block {
                run_serial_op(&mut s, op);
            }
            let mut ser = s.journal_mut().take_perp_delta();

            // Parallel driver — this block has ZERO matches (all same-side rests, no opposite book).
            let book = Arc::new(SharedPerpBook::new());
            seed(&mut make_slot(book.clone()), &makers);
            let results = transact_block_parallel(test_pool(), &book, &block, make_slot).unwrap();
            for r in &results {
                assert_eq!(*r, OpResult::Place(PlaceOutcome::Executed));
            }
            let mut par = book.take_delta();

            let wkey = storage::keys::price_basis_window_key(MID);
            ser.remove(&wkey);
            par.remove(&wkey);
            assert_eq!(
                ser, par,
                "a NO-MATCH block must FIFO-sort its touched levels before commit (round {_round})"
            );
        }
    }

    /// `transact_block_serial` is the diff target for the node's PERP_PARALLEL_AUDIT, so it must
    /// produce the SAME book delta as the parallel driver for a correct scenario (a crossing taker).
    /// Both do the block-end mid sample, so the price-basis window matches too — no carve-out needed.
    #[test]
    fn transact_block_serial_matches_parallel_delta() {
        let a = user_addr(1);
        let b = user_addr(2);
        let ops = vec![
            mk_place(a, oid(70), 1, 100 * TICK, 0, 0, 0), // SELL@100 → rests
            mk_place(b, oid(71), 0, 100 * TICK, 0, 0, 1), // BUY@100 taker → fills A
        ];

        let book_s = Arc::new(SharedPerpBook::new());
        seed(&mut make_slot(book_s.clone()), &[a, b]);
        transact_block_serial(&book_s, &ops, make_slot).unwrap();
        let ds = book_s.take_delta();

        let book_p = Arc::new(SharedPerpBook::new());
        seed(&mut make_slot(book_p.clone()), &[a, b]);
        transact_block_parallel(test_pool(), &book_p, &ops, make_slot).unwrap();
        let dp = book_p.take_delta();

        assert_eq!(ds, dp, "transact_block_serial must equal the parallel delta (audit ground truth)");
    }

    /// Merge a block's perp delta into a committed cold-read store (mirrors reth's `merge_perp_delta`
    /// into `canonical_perp`): every key is overwritten with the block's net blob; an empty blob is the
    /// delete convention, read back as `vec![]`.
    fn merge_committed(committed: &mut context::journaled_state::PerpDelta, delta: context::journaled_state::PerpDelta) {
        for (k, v) in delta {
            committed.insert(k, v);
        }
    }

    /// Compare two committed stores treating empty == absent (the delete convention) and carving out the
    /// block-end mid-sample window; panics with the FIRST diverging key (focused, not a whole-map dump).
    fn assert_committed_eq(
        par: &context::journaled_state::PerpDelta,
        ser: &context::journaled_state::PerpDelta,
        block: usize,
    ) {
        let wkey = storage::keys::price_basis_window_key(MID);
        let mut diverging = 0usize;
        let mut first: Option<(primitives::B256, std::vec::Vec<u8>, std::vec::Vec<u8>)> = None;
        let keys: std::collections::BTreeSet<primitives::B256> =
            par.keys().chain(ser.keys()).copied().filter(|k| *k != wkey).collect();
        for k in keys {
            let pe = par.get(&k).map(|v| v.as_slice()).unwrap_or(&[]);
            let se = ser.get(&k).map(|v| v.as_slice()).unwrap_or(&[]);
            if pe != se {
                diverging += 1;
                if first.is_none() {
                    first = Some((k, pe.to_vec(), se.to_vec()));
                }
            }
        }
        if let Some((k, pe, se)) = first {
            panic!(
                "block {block}: {diverging} diverging committed key(s). first key={k:?}\n  parallel={pe:02x?}\n  serial  ={se:02x?}"
            );
        }
    }

    /// THE step-4b matchingPair regression (the residual production bug): a multi-block churn at a
    /// SINGLE price with parity sides (even acct → BUY, odd → SELL — exactly matchingPair, so no
    /// self-trade), the committed store carried forward across blocks (cold-read). The parallel driver's
    /// cumulative committed state must equal serial's after EVERY block. This is the faithful repro of
    /// the production workload that failed with pure TRADE_MISMATCH; queues build + drain across blocks,
    /// exercising cross-block cold-read maker consumption under churn.
    #[test]
    fn transact_block_parallel_matchingpair_churn_multiblock_matches_serial() {
        const N_ACCTS: u64 = 16;
        const OPS_PER_BLOCK: usize = 14; // high SAME-level contention (the race surface)
        const BLOCKS: usize = 25;
        const REPS: usize = 10; // re-run the churn to widen the race timing window
        let p = 100 * TICK;
        let accts: Vec<Address> = (1..=N_ACCTS).map(user_addr).collect();

        for rep in 0..REPS {
            // Initial committed store: market + funded accounts (huge balance so margin never
            // bottlenecks the churn and masks a divergence).
            let mut init: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
            storage::save_market(&mut init, &test_market()).unwrap();
            for &a in &accts {
                fund(&mut init, a, 1_000_000_000_000_000u64);
            }
            let initial = init.journal_mut().take_perp_delta();
            let mut committed_par = initial.clone();
            let mut committed_ser = initial;

            // Deterministic LCG (seed varies per rep → different tx orders); thread timing varies the
            // race independently of the seed.
            let mut rng: u64 = 0x1234_5678_9abc_def0u64.wrapping_add((rep as u64).wrapping_mul(0x9E37_79B9));
            let mut next = || {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                rng >> 33
            };
            let mut oid_ctr: u64 = 1;

            for block in 0..BLOCKS {
                // This block's ops (generated once, run on BOTH tracks). Side = acct parity (matchingPair:
                // even acct → BUY, odd → SELL — so no self-trade); many DISTINCT accounts at one price →
                // concurrent same-level appends.
                let mut ops = Vec::with_capacity(OPS_PER_BLOCK);
                for _ in 0..OPS_PER_BLOCK {
                    let idx = (next() % N_ACCTS) as usize;
                    let side = ((idx + 1) % 2) as u8; // user_addr(idx+1): odd→SELL(1), even→BUY(0)
                    let mut oidb = [0u8; 32];
                    oidb[0..8].copy_from_slice(&oid_ctr.to_be_bytes());
                    oid_ctr += 1;
                    ops.push(mk_place(accts[idx], oidb, side, p, 0, 0, 0));
                }

                // Serial track over its committed cold-read store.
                let mut s: ColdCtx =
                    Context::new(ColdPerpDb { perp: committed_ser.clone() }, SpecId::CANCUN);
                for op in &ops {
                    run_serial_op(&mut s, op);
                }
                merge_committed(&mut committed_ser, s.journal_mut().take_perp_delta());

                // Parallel track: fresh book over its committed cold-read store.
                let cp = committed_par.clone();
                let make_cold = move |bk: Arc<SharedPerpBook>| -> ColdCtx {
                    let mut c: ColdCtx =
                        Context::new(ColdPerpDb { perp: cp.clone() }, SpecId::CANCUN);
                    c.journal_mut().set_perp_shared(bk);
                    c
                };
                let book = Arc::new(SharedPerpBook::new());
                transact_block_parallel(test_pool(), &book, &ops, make_cold).unwrap();
                merge_committed(&mut committed_par, book.take_delta());

                assert_committed_eq(&committed_par, &committed_ser, rep * BLOCKS + block);
            }
        }
    }

    /// THE step-3d gate: a MIXED block (places + a cancel of a SAME-BLOCK-placed order) run through
    /// `transact_block_parallel` produces a book delta byte-identical to the same ops run serially in
    /// txn_id order — INCLUDING price_basis_window. Exercises the unified ticket domain, the shared
    /// PriceCompletion across place+cancel (the at-best cancel of X waits for the lower-ticket places
    /// at price 100 — incl. X's own place, resolved via same-block linkage), and the block-end mid
    /// sample. Scenario has exactly ONE best-change (X sets best_bid 0→100; no asks; later ops don't
    /// move best), so serial's in-body first-change mid (=100) equals the parallel barrier's block-end
    /// final mid (=100) → the window matches byte-for-byte with no carve-out.
    #[test]
    fn transact_block_parallel_matches_serial_delta() {
        let a = user_addr(1);
        let b = user_addr(2);
        let c = user_addr(3);
        let (x, y, z) = (oid(10), oid(11), oid(12));

        // ops in txn_id order; ticket = index, rank = per-maker count.
        // 0: a places X@100 (mover → best_bid 100)
        // 1: b places Y@99  (non-mover, below best)
        // 2: c places Z@100 (non-mover, joins level 100)
        // 3: a cancels X     (at-best 100; Z remains → non-emptying → parallel remove)
        let ops = vec![
            PerpOp::Place(mk_work(a, x, 100 * TICK, 0, 0)),
            PerpOp::Place(mk_work(b, y, 99 * TICK, 0, 1)),
            PerpOp::Place(mk_work(c, z, 100 * TICK, 0, 2)),
            PerpOp::Cancel(mk_cancel(a, x, 1, 3)),
        ];

        // Serial reference.
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial, &[a, b, c]);
        for op in &ops {
            run_serial_op(&mut serial, op);
        }
        let serial_delta = serial.journal_mut().take_perp_delta();

        // Parallel block driver.
        let book = Arc::new(SharedPerpBook::new());
        seed(&mut make_slot(book.clone()), &[a, b, c]);
        let results = transact_block_parallel(test_pool(), &book, &ops, make_slot).unwrap();
        let parallel_delta = book.take_delta();

        assert_eq!(results[0], OpResult::Place(PlaceOutcome::Executed));
        assert_eq!(results[1], OpResult::Place(PlaceOutcome::Executed));
        assert_eq!(results[2], OpResult::Place(PlaceOutcome::Executed));
        assert_eq!(results[3], OpResult::Cancel(CancelOutcome::Executed));
        assert_eq!(serial_delta, parallel_delta);
    }

    /// The serial error policy mirrored by the parallel path: Ok→Committed, a `Fatal` PROPAGATES
    /// (block abort), any other error → per-tx `Reverted`. (Real DB Fatals can't be triggered against
    /// InMemoryDB, so the policy is exercised here directly; the driver propagates it via `?`.)
    #[test]
    fn dispose_body_propagates_fatal_reverts_other() {
        let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);

        let cp = ctx.journal_mut().checkpoint();
        assert!(matches!(
            dispose_body(&mut ctx, cp, Ok::<(), PrecompileError>(())).unwrap(),
            BodyDisposition::Committed
        ));

        let cp = ctx.journal_mut().checkpoint();
        assert!(matches!(
            dispose_body(
                &mut ctx,
                cp,
                Err::<(), _>(PrecompileError::Other("user revert".into()))
            )
            .unwrap(),
            BodyDisposition::Reverted
        ));

        let cp = ctx.journal_mut().checkpoint();
        assert!(
            dispose_body(
                &mut ctx,
                cp,
                Err::<(), _>(PrecompileError::Fatal("storage bug".into()))
            )
            .is_err(),
            "a Fatal must propagate (block abort), not become a revert"
        );
    }

    fn mk_place(
        maker: Address,
        order_id: [u8; 32],
        side: u8,
        price: u64,
        tif: u8,
        rank: u64,
        ticket: u64,
    ) -> PerpOp {
        PerpOp::Place(PlaceWork {
            maker,
            order_id,
            market_id: MID,
            side,
            price,
            qty: QTY,
            order_type: 0,
            tif,
            client_order_id: [0u8; 16],
            rank,
            ticket,
        })
    }

    /// THE step-3d-3 gate: a crossing TAKER downgrade + TAKER CONTAGION + the serial barrier re-run,
    /// byte-identical to serial. Block: ticket 0 rests a sell S@100 (ask mover, parallel); ticket 1 is
    /// a buy taker T@100 (crosses best_ask → downgrade, sets the contagion floor to 1); ticket 2 is a
    /// sell S2@100 — a non-crossing rest that, WITHOUT contagion, would land in the parallel phase and
    /// be wrongly consumable by the barrier-deferred taker (it doesn't exist yet in serial order). The
    /// floor forces S2 to the barrier; the barrier re-runs T then S2 in ticket order, so T matches only
    /// S (serial-equivalent). One best-change family keeps mid(0,100)=100 identical in both paths.
    #[test]
    fn transact_block_parallel_taker_contagion_matches_serial() {
        let a = user_addr(1);
        let b = user_addr(2);
        let c = user_addr(3);
        let (s, t, s2) = (oid(20), oid(21), oid(22));
        let ops = vec![
            mk_place(a, s, 1, 100 * TICK, 0, 0, 0), // sell S@100 (ask mover, parallel)
            mk_place(b, t, 0, 100 * TICK, 0, 0, 1), // buy taker T@100 (crosses → downgrade, floor=1)
            mk_place(c, s2, 1, 100 * TICK, 0, 0, 2), // sell S2@100 (forced to barrier by contagion)
        ];

        // Serial reference.
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial, &[a, b, c]);
        for op in &ops {
            run_serial_op(&mut serial, op);
        }
        let serial_delta = serial.journal_mut().take_perp_delta();

        // Parallel block driver.
        let book = Arc::new(SharedPerpBook::new());
        seed(&mut make_slot(book.clone()), &[a, b, c]);
        let results = transact_block_parallel(test_pool(), &book, &ops, make_slot).unwrap();
        let parallel_delta = book.take_delta();

        // The barrier must resolve every downgrade (none left as Downgrade).
        for r in &results {
            assert!(
                !matches!(
                    r,
                    OpResult::Place(PlaceOutcome::Downgrade)
                        | OpResult::Cancel(CancelOutcome::Downgrade)
                ),
                "barrier must re-run all downgrades, got {r:?}"
            );
        }
        assert_eq!(serial_delta, parallel_delta);
    }

    /// DEEP segmentation (the step-4 re-dispatch loop): a block with MULTIPLE interspersed takers, so
    /// the driver runs ≥3 segments and the post-taker tail RE-PARALLELIZES instead of collapsing onto
    /// one end-of-block barrier. Each taker downgrades + sets its segment's contagion floor; the floor
    /// runs serially in place against the FIFO-sorted book, then the strictly-higher tail re-dispatches
    /// against the post-taker book. Must stay byte-identical to serial across every segment boundary.
    ///
    /// Layout (all market BTC, qty QTY so the crossing limits fully fill — no taker remainder rests):
    ///   seg 1: t0 a SELL@100 (ask mover, parallel) · t1 b BUY@100 (crosses → floor; t2.. contagion)
    ///   seg 2: t2 c SELL@100 (mover again — a's ask was consumed; re-parallelized) · t3 d BUY@100 (floor)
    ///   seg 3: t4 e SELL@100 (mover) · t5 f SELL@101 (non-mover) — no taker → loop ends
    /// (Multiple best-changes → the known D3-b in-body-vs-block-end mid difference; carve the window
    /// out, the correctness lives in the position / order / level / counterparty keys.)
    #[test]
    fn transact_block_parallel_deep_segmentation_matches_serial() {
        let (a, b, c, d, e, f) = (
            user_addr(1),
            user_addr(2),
            user_addr(3),
            user_addr(4),
            user_addr(5),
            user_addr(6),
        );
        let ops = vec![
            mk_place(a, oid(70), 1, 100 * TICK, 0, 0, 0), // SELL@100 ask mover (parallel, seg 1)
            mk_place(b, oid(71), 0, 100 * TICK, 0, 0, 1), // BUY@100 taker (crosses → floor seg 1)
            mk_place(c, oid(72), 1, 100 * TICK, 0, 0, 2), // SELL@100 mover again (re-parallelized, seg 2)
            mk_place(d, oid(73), 0, 100 * TICK, 0, 0, 3), // BUY@100 taker (crosses → floor seg 2)
            mk_place(e, oid(74), 1, 100 * TICK, 0, 0, 4), // SELL@100 mover (re-parallelized, seg 3)
            mk_place(f, oid(75), 1, 101 * TICK, 0, 0, 5), // SELL@101 non-mover (parallel, seg 3)
        ];
        let users = [a, b, c, d, e, f];

        // Serial reference.
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial, &users);
        for op in &ops {
            run_serial_op(&mut serial, op);
        }
        let wkey = storage::keys::price_basis_window_key(MID);
        let mut serial_delta = serial.journal_mut().take_perp_delta();
        serial_delta.remove(&wkey);

        // Parallel block driver — exercises 3 segments + 2 serial floor ops.
        let book = Arc::new(SharedPerpBook::new());
        seed(&mut make_slot(book.clone()), &users);
        let results = transact_block_parallel(test_pool(), &book, &ops, make_slot).unwrap();
        let mut parallel_delta = book.take_delta();
        parallel_delta.remove(&wkey);

        for r in &results {
            assert!(
                !matches!(
                    r,
                    OpResult::Place(PlaceOutcome::Downgrade)
                        | OpResult::Cancel(CancelOutcome::Downgrade)
                ),
                "every downgrade must be resolved by a segment's serial floor, got {r:?}"
            );
        }
        assert!(
            results
                .iter()
                .all(|r| matches!(r, OpResult::Place(PlaceOutcome::Executed))),
            "both takers should fill and every rest should land: {results:?}"
        );
        assert_eq!(serial_delta, parallel_delta);
    }

    /// Regression for the adversarial-review MEDIUM finding: an UNRESOLVED cancel (order neither in the
    /// book nor placed this block → resolved = None) sitting ABOVE a contagion floor. The `None` branch
    /// of parallel_cancel was the ONLY op path that skipped the floor check, so it would return
    /// Executed/Reverted above the floor — breaking the driver's contiguous-downgrade-suffix invariant
    /// (a debug_assert trip in this build; a needless re-dispatch in release). With the fix it downgrades
    /// like every other above-floor op. A None cancel is a no-op revert either way, so the delta stays
    /// byte-identical to serial. (The taker's best-change → the known D3-b mid difference; carve it out.)
    #[test]
    fn transact_block_parallel_unresolved_cancel_above_floor_matches_serial() {
        let a = user_addr(1);
        let b = user_addr(2);
        let c = user_addr(3);
        let ghost = oid(81); // never placed → the cancel resolves to None
        let ops = vec![
            mk_place(a, oid(80), 1, 100 * TICK, 0, 0, 0), // SELL@100 ask mover (parallel, seg 1)
            mk_place(b, oid(82), 0, 100 * TICK, 0, 0, 1), // BUY@100 taker (crosses → floor=1)
            PerpOp::Cancel(mk_cancel(c, ghost, 0, 2)),    // cancel a non-existent order (None, above floor)
        ];

        let wkey = storage::keys::price_basis_window_key(MID);
        // Serial reference: the ghost cancel reverts (order not found) → contributes NO delta, and
        // run_serial_op unwraps (can't take a reverting op), so the faithful reference is the first two
        // ops (the revert is a no-op the parallel path applies and rolls back to the same state).
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial, &[a, b, c]);
        for op in &ops[..2] {
            run_serial_op(&mut serial, op);
        }
        let mut serial_delta = serial.journal_mut().take_perp_delta();
        serial_delta.remove(&wkey);

        let book = Arc::new(SharedPerpBook::new());
        seed(&mut make_slot(book.clone()), &[a, b, c]);
        let results = transact_block_parallel(test_pool(), &book, &ops, make_slot).unwrap();
        let mut parallel_delta = book.take_delta();
        parallel_delta.remove(&wkey);

        // The unresolved cancel reverts (order not found) — not lost, not errored, not left Downgrade.
        assert_eq!(results[2], OpResult::Cancel(CancelOutcome::Reverted));
        for r in &results {
            assert!(
                !matches!(
                    r,
                    OpResult::Place(PlaceOutcome::Downgrade)
                        | OpResult::Cancel(CancelOutcome::Downgrade)
                ),
                "every downgrade must be resolved by a segment's serial floor, got {r:?}"
            );
        }
        assert_eq!(serial_delta, parallel_delta);
    }

    /// Regression for the price-time-priority bug: TWO distinct-account makers rest at the SAME
    /// NON-BEST price in the parallel phase (both non-movers → genuinely racy append order; a mover
    /// would rest deterministically under the BBO ticket), then a downgraded taker at the LAST ticket
    /// consumes that level at the barrier. The taker must fill the LOWER-ticket maker (FIFO), so the
    /// level MUST be ticket-sorted BEFORE the barrier match (phase 3a finalize precedes phase 3b
    /// match). Looped to shake out the racy append; without the fix the taker sometimes fills the
    /// wrong maker → a divergent counterparty/position delta.
    #[test]
    fn transact_block_parallel_taker_matches_fifo_not_racy_order() {
        const N: u64 = 8;
        let x = user_addr(20); // ask@99 mover (rests deterministically under the ticket)
        let d = user_addr(21); // buy taker, qty 2 → clears 99 then the FRONT maker at 100
        let makers: Vec<Address> = (1..=N).map(user_addr).collect(); // ask@100 non-movers (race)

        // t0 SELL x@99 (mover); t1..tN SELL makers@100 (non-movers, racy append among the N);
        // t(N+1) BUY taker d@100 qty2 (crosses → downgrade; consumes 99 then the FIFO-FRONT 100 maker).
        let mut ops = vec![mk_place(x, oid(50), 1, 99 * TICK, 0, 0, 0)];
        for (i, &m) in makers.iter().enumerate() {
            let mut o = [0u8; 32];
            o[0] = 60 + i as u8;
            ops.push(mk_place(m, o, 1, 100 * TICK, 0, 0, (i + 1) as u64));
        }
        ops.push(PerpOp::Place(PlaceWork {
            maker: d,
            order_id: oid(90),
            market_id: MID,
            side: 0, // Buy
            price: 100 * TICK,
            qty: 2 * QTY,
            order_type: 0,
            tif: 0,
            client_order_id: [0u8; 16],
            rank: 0,
            ticket: N + 1,
        }));
        let mut all_users = vec![x, d];
        all_users.extend(makers.iter().copied());

        // This scenario has multiple best-changes (99 then taker→100), so serial's in-body
        // first-change mid (99) differs from the parallel block-end mid (100) — the known D3-b
        // difference. Carve the window out; the FIFO-maker correctness lives in the position / order /
        // level keys.
        let wkey = storage::keys::price_basis_window_key(MID);
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        seed(&mut serial, &all_users);
        for op in &ops {
            run_serial_op(&mut serial, op);
        }
        let mut serial_delta = serial.journal_mut().take_perp_delta();
        serial_delta.remove(&wkey);

        for _round in 0..40 {
            let book = Arc::new(SharedPerpBook::new());
            seed(&mut make_slot(book.clone()), &all_users);
            let results = transact_block_parallel(test_pool(), &book, &ops, make_slot).unwrap();
            for r in &results {
                assert!(
                    !matches!(
                        r,
                        OpResult::Place(PlaceOutcome::Downgrade)
                            | OpResult::Cancel(CancelOutcome::Downgrade)
                    ),
                    "barrier must re-run all downgrades"
                );
            }
            let mut parallel_delta = book.take_delta();
            parallel_delta.remove(&wkey);
            assert_eq!(
                serial_delta, parallel_delta,
                "taker filled the wrong maker (level not FIFO-sorted before the barrier match), round {_round}"
            );
        }
    }

    /// Regression for the BBO-serve-cursor wedge: a cancel of an UNLOADABLE order (resolved == None)
    /// at a LOWER ticket must still consume its BBO ticket, or a higher-ticket cancel's `bbo.run`
    /// blocks forever and the whole batch hangs. Run under a watchdog so a regression fails loudly
    /// (timeout) instead of hanging the test binary.
    #[test]
    fn parallel_cancel_unloadable_lower_ticket_does_not_wedge_bbo() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let a = user_addr(1);
            let b = user_addr(2);
            let c = user_addr(3);
            let w = oid(13);
            let bogus = oid(99); // never placed → resolved == None

            let book = Arc::new(SharedPerpBook::new());
            {
                let mut s = make_slot(book.clone());
                seed(&mut s, &[a, b, c]);
                rest(&mut s, c, oid(12), 100 * TICK); // best bid 100 (NOT cancelled)
                rest(&mut s, b, w, 99 * TICK); // below best
            }
            let cancels = vec![
                mk_cancel(a, bogus, 0, 0), // unloadable → None, ticket 0 (MUST pass the ticket)
                mk_cancel(b, w, 0, 1),     // real below-best cancel, ticket 1
            ];
            let gate = AccountGate::new();
            let book_lock = BookSideLock::new();
            let bbo = BboTicketLock::new();
            let results = run_cancel_batch(&book, &gate, &book_lock, &bbo, &cancels, make_slot);
            let _ = tx.send(results);
        });

        let results = rx.recv_timeout(Duration::from_secs(10)).expect(
            "parallel_cancel wedged the BBO serve cursor (deadlock): a None-resolved lower ticket \
             did not pass its BBO ticket",
        );
        worker.join().unwrap();
        assert_eq!(results[0].as_ref().unwrap(), &CancelOutcome::Reverted); // bogus order
        assert_eq!(results[1].as_ref().unwrap(), &CancelOutcome::Executed); // real below-best
    }

    /// Regression for the same-maker deferred-margin divergence (design C): one maker M, equity tuned
    /// so it can fund ONE order but not two. Block = M cancels its sole best order O (emptying →
    /// moves the BBO) + M places P. In serial, the cancel releases O's margin first, so P fits. WITHOUT
    /// the fix the emptying cancel does not set the contagion floor, so P runs in the parallel phase
    /// while O's margin is still reserved → P reverts (insufficient margin) → diverges. WITH design C
    /// the emptying cancel sets the floor → P is forced to the barrier → runs AFTER the cancel
    /// releases O's margin → P fits → parallel == serial. The window coincides (final mid == setup
    /// mid == 100), so this compares the FULL delta.
    #[test]
    fn transact_block_parallel_same_maker_emptying_cancel_then_place_matches_serial() {
        let m = user_addr(1);
        let (o_id, p_id) = (oid(40), oid(41));

        // Calibrate: d_o = wallet drop placing O (sole buy@100); d_p = wallet drop placing P (2nd buy).
        let (d_o, d_p) = {
            let mut c: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
            storage::save_market(&mut c, &test_market()).unwrap();
            fund(&mut c, m, WALLET);
            let w0 = storage::load_account(&mut c, m).unwrap().perp_wallet_balance;
            rest(&mut c, m, o_id, 100 * TICK);
            let w1 = storage::load_account(&mut c, m).unwrap().perp_wallet_balance;
            rest(&mut c, m, p_id, 100 * TICK);
            let w2 = storage::load_account(&mut c, m).unwrap().perp_wallet_balance;
            ((w0 - w1) as u64, (w1 - w2) as u64)
        };
        // O alone fits; O + P does not (so a phase-2 P, with O's margin still held, reverts).
        let funding = d_o + d_p - 1;

        // ops: cancel O (rank 0, ticket 0, emptying) then place P (rank 1, ticket 1, buy@100).
        let ops = vec![
            PerpOp::Cancel(mk_cancel(m, o_id, 0, 0)),
            mk_place(m, p_id, 0, 100 * TICK, 0, 1, 1),
        ];

        // Serial reference: fund + set up O resting, then run the block in txn_id order.
        let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        storage::save_market(&mut serial, &test_market()).unwrap();
        fund(&mut serial, m, funding);
        rest(&mut serial, m, o_id, 100 * TICK);
        for op in &ops {
            run_serial_op(&mut serial, op);
        }
        let serial_delta = serial.journal_mut().take_perp_delta();

        // Parallel: identical setup in the shared book, then the block driver.
        let book = Arc::new(SharedPerpBook::new());
        {
            let mut s = make_slot(book.clone());
            storage::save_market(&mut s, &test_market()).unwrap();
            fund(&mut s, m, funding);
            rest(&mut s, m, o_id, 100 * TICK);
        }
        let results = transact_block_parallel(test_pool(), &book, &ops, make_slot).unwrap();
        let parallel_delta = book.take_delta();

        // Both run at the barrier in txn_id order: cancel O then place P, both Executed.
        assert_eq!(results[0], OpResult::Cancel(CancelOutcome::Executed));
        assert_eq!(results[1], OpResult::Place(PlaceOutcome::Executed));
        assert_eq!(serial_delta, parallel_delta);
    }

    /// Regression for the held-ticket / AccountGate deadlock (design-C review): the driver must OWN
    /// rank/ticket assignment so a maker's rank is monotone in ticket. Here the caller supplies
    /// INVERTED ranks (the index-0 mover gets rank 1, the index-1 op rank 0). Without driver
    /// normalization the index-0 mover would hold the BBO serve cursor while waiting on the AccountGate
    /// for rank 0 — owned by the higher-ticket op blocked behind the pinned cursor → block-wide hang.
    /// With `normalize_schedule` the ranks become 0,1 and it completes. Watchdog'd so a regression
    /// fails loudly.
    #[test]
    fn transact_block_parallel_normalizes_inverted_ranks_no_deadlock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let m = user_addr(1);
            let (a, b) = (oid(50), oid(51));
            let mk_inv = |order_id, price, rank, ticket| {
                PerpOp::Place(PlaceWork {
                    maker: m,
                    order_id,
                    market_id: MID,
                    side: 0,
                    price,
                    qty: QTY,
                    order_type: 0,
                    tif: 0,
                    client_order_id: [0u8; 16],
                    rank,
                    ticket,
                })
            };
            // INVERTED: index-0 mover @100 gets rank 1; index-1 non-mover @99 gets rank 0.
            let ops = vec![mk_inv(a, 100 * TICK, 1, 0), mk_inv(b, 99 * TICK, 0, 1)];

            let mut serial: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
            seed(&mut serial, &[m]);
            for op in &ops {
                run_serial_op(&mut serial, op);
            }
            let serial_delta = serial.journal_mut().take_perp_delta();

            let book = Arc::new(SharedPerpBook::new());
            seed(&mut make_slot(book.clone()), &[m]);
            let results = transact_block_parallel(test_pool(), &book, &ops, make_slot).unwrap();
            let parallel_delta = book.take_delta();
            let _ = tx.send((results, serial_delta, parallel_delta));
        });

        let (results, serial_delta, parallel_delta) = rx.recv_timeout(Duration::from_secs(10)).expect(
            "transact_block_parallel deadlocked on rank/ticket-inverted input (driver did not normalize)",
        );
        worker.join().unwrap();
        assert_eq!(results[0], OpResult::Place(PlaceOutcome::Executed));
        assert_eq!(results[1], OpResult::Place(PlaceOutcome::Executed));
        assert_eq!(serial_delta, parallel_delta);
    }
}
