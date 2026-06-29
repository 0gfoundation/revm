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
        cancel_order_core, decode_cancel_order, decode_place_order, encode_place_order_id,
        place_order_core, verify_cancel_order_signed, verify_place_order_signed, CancelParams,
        PlaceParams,
    },
};
use crate::PrecompileError;
use alloy_sol_types::SolCall;
use context::journal::perp_pool::PerpPool;
use context::journal::perp_sched::{AccountGate, BboTicketLock, BookSideLock, PriceCompletion};
use context::journal::shared_perp::SharedPerpBook;
use context::journaled_state::JournalCheckpoint;
use context::{ContextTr, JournalTr};
use primitives::{Address, Log};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

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
#[derive(Debug, Clone)]
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
        book_lock.run(work.market_id, work.side, || {
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

/// Execute one parallel place (the lock-body). Classify under the BBO ticket; a mover / PostOnly
/// cross executes under the held ticket (the account rank is waited at the place — Q3), a non-mover
/// releases the ticket then executes under the account gate (rule 4.iii), a crossing matcher / taker
/// is marked `Downgrade` for the serial barrier (step 3d does the re-run).
pub fn parallel_place<CTX: ContextTr>(
    ctx: &mut CTX,
    account_gate: &AccountGate,
    book_lock: &BookSideLock,
    bbo: &BboTicketLock,
    price_completion: &PriceCompletion,
    min_downgrade: &AtomicU64,
    work: &PlaceWork,
) -> Result<PlaceOutcome, PrecompileError> {
    // Publish this place's completion at its (limit) price on EVERY exit, so a higher-txn_id at-best
    // cancel of a same-block order at this price (mixed batch, step 3d) sees the rest before deciding
    // emptiness. Fires on Ok / `?` / panic, mirroring the cancel path. A place's price is always
    // `work.price` regardless of outcome (rested / reverted / downgraded).
    let _mark_done = MarkDoneOnDrop {
        price_completion,
        market: work.market_id,
        price: work.price,
        ticket: work.ticket,
    };
    enum UnderTicket {
        Done(Result<PlaceOutcome, PrecompileError>),
        Release,
        Downgrade,
    }
    let under =
        bbo.run(work.ticket, || -> Result<UnderTicket, PrecompileError> {
            let mut plan = match (
                Side::from_u8(work.side),
                OrderType::from_u8(work.order_type),
                TimeInForce::from_u8(work.tif),
            ) {
                (Some(side), Some(order_type), Some(tif)) => {
                    let best_bid = storage::load_best_bid(ctx, work.market_id)?;
                    let best_ask = storage::load_best_ask(ctx, work.market_id)?;
                    classify_place(side, order_type, tif, work.price, best_bid, best_ask)
                }
                // Unparseable order fields → let the body reject it under the ticket.
                _ => LockPlan::RejectInBody,
            };
            // Taker contagion: a lower-txn_id taker downgrade in this market forces THIS op to the barrier
            // too (a barrier taker re-running against the post-parallel book must not match liquidity that
            // higher-txn_id parallel rests added — in serial those don't exist yet). Classification runs
            // under the BBO ticket (txn_id order), so the first taker's ticket is the deterministic floor.
            if min_downgrade.load(Ordering::SeqCst) < work.ticket {
                plan = LockPlan::DowngradeToBarrier;
            }
            // A taker downgrade (crossing / Market / IOC / FOK — NOT a PostOnly self-reject) lowers the
            // floor for higher tickets. fetch_min is a no-op for a forced (higher-ticket) downgrade.
            if matches!(plan, LockPlan::DowngradeToBarrier) {
                min_downgrade.fetch_min(work.ticket, Ordering::SeqCst);
            }
            match plan {
                LockPlan::DowngradeToBarrier => Ok(UnderTicket::Downgrade),
                // A mover (or a PostOnly-cross self-reject) executes while still holding the BBO ticket so
                // its best-update is serialized; the body itself takes the book-side lock.
                LockPlan::HoldTicket | LockPlan::RejectInBody => Ok(UnderTicket::Done(
                    gated_execute(ctx, account_gate, book_lock, work),
                )),
                LockPlan::ReleaseTicket => Ok(UnderTicket::Release),
            }
        })?;
    match under {
        UnderTicket::Done(r) => r,
        UnderTicket::Downgrade => {
            // Deferred to the barrier, but still CONSUME this maker's account rank (empty body) — a
            // higher-rank same-account parallel op waits on the AccountGate and would hang otherwise.
            // The actual effect runs at the serial barrier (in txn_id order). Mirrors the BBO-ticket
            // pass-through for None-resolved cancels.
            account_gate.run(work.maker, work.rank, || ());
            Ok(PlaceOutcome::Downgrade)
        }
        // Ticket released (non-mover); rest under the account gate + book-side lock, in parallel.
        UnderTicket::Release => gated_execute(ctx, account_gate, book_lock, work),
    }
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
    // Place-only batch: no at-best cancel waits on these, but places still mark_done (harmlessly) so
    // the same primitive serves the unified mixed batch (step 3d) unchanged. A fresh per-batch
    // contagion floor (no taker means no forcing → unchanged behavior for non-crossing batches).
    let price_completion = PriceCompletion::new();
    let min_downgrade = AtomicU64::new(u64::MAX);
    let (make_ctx, price_completion, min_downgrade) =
        (&make_ctx, &price_completion, &min_downgrade);
    thread::scope(|s| {
        let handles: Vec<_> = items
            .iter()
            .map(|item| {
                let book = book.clone();
                s.spawn(move || {
                    let mut ctx = make_ctx(book);
                    parallel_place(
                        &mut ctx,
                        account_gate,
                        book_lock,
                        bbo,
                        price_completion,
                        min_downgrade,
                        item,
                    )
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
#[derive(Debug, Clone)]
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

/// Resolved + scheduled cancel produced by the pre-scan: the order's `(market, side, price)` plus the
/// tickets of all lower-txn_id ops at the SAME `(market, price)` this cancel must wait for before it
/// can decide emptiness. `resolved == None` means the order was not loadable (→ the body will revert).
#[derive(Debug, Clone)]
struct CancelPlanItem {
    work: CancelWork,
    resolved: Option<(u64, u8, u64)>,
    required: Vec<u64>,
}

fn load_level<CTX: ContextTr>(
    ctx: &mut CTX,
    market: u64,
    side: Side,
    price: u64,
) -> Result<Vec<[u8; 32]>, PrecompileError> {
    match side {
        Side::Buy => storage::load_bid_level(ctx, market, price),
        Side::Sell => storage::load_ask_level(ctx, market, price),
    }
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
) -> Result<CancelOutcome, PrecompileError> {
    account_gate.run(work.canceller, work.rank, || {
        book_lock.run(market, side, || {
            let cp = ctx.journal_mut().checkpoint();
            let r = cancel_order_core(work.canceller, work.order_id, ctx);
            Ok(match dispose_body(ctx, cp, r)? {
                BodyDisposition::Committed => CancelOutcome::Executed,
                BodyDisposition::Reverted => CancelOutcome::Reverted,
            })
        })
    })
}

/// RAII guard that publishes a cancel's completion at its price on EVERY exit — `Ok`, an early `?`
/// return (storage error / a future `Fatal`), or a panic. A best-price cancel's [`PriceCompletion`]
/// wait only resolves once each lower-txn_id op at its price is marked done; if a marker could skip
/// `mark_done` on an error path the waiter would hang forever (block-wide), so this mirrors the
/// advance-on-drop discipline of `AccountGate`/`BboTicketLock`.
struct MarkDoneOnDrop<'a> {
    price_completion: &'a PriceCompletion,
    market: u64,
    price: u64,
    ticket: u64,
}

impl Drop for MarkDoneOnDrop<'_> {
    fn drop(&mut self) {
        self.price_completion
            .mark_done(self.market, self.price, self.ticket);
    }
}

/// Execute one parallel cancel. Classifies (and, for at-best, decides emptiness) WHILE HOLDING the
/// BBO ticket: below-best → remove in parallel; at-best → wait for all lower-txn_id same-price ops
/// ([`PriceCompletion::wait_for`]) so the level is settled, then if removing it would EMPTY the best
/// level (→ moves the BBO) it sets the contagion floor like a taker and downgrades to the serial
/// barrier, else it releases the ticket and removes in parallel. Holding the ticket through the
/// decision guarantees the floor (when set) is seen by every higher-txn_id op, so the deferred tail —
/// including the same maker's later margin-touching ops — re-runs serial-equivalently at the barrier
/// (no same-maker op observes the cancel's not-yet-released margin). Every op marks its price done so
/// higher same-price cancels' waits resolve.
fn parallel_cancel<CTX: ContextTr>(
    ctx: &mut CTX,
    account_gate: &AccountGate,
    book_lock: &BookSideLock,
    bbo: &BboTicketLock,
    price_completion: &PriceCompletion,
    min_downgrade: &AtomicU64,
    plan: &CancelPlanItem,
) -> Result<CancelOutcome, PrecompileError> {
    let (market, side_u8, price) = match plan.resolved {
        Some(t) => t,
        // Order not loadable → the body reverts (missing order) without touching any level, so it
        // needs no book lock and no price-completion bookkeeping (no waiter can name a None op's
        // ticket — `plan_cancel_batch` only adds Some-resolved tickets to a `required` set). It MUST
        // still consume its BBO ticket, or the serve cursor wedges and every higher ticket hangs.
        None => {
            // Honor the contagion floor here too: if a lower-txn_id op downgraded, this op is ABOVE
            // the floor and must defer as well. Without this it is the ONLY path that can `Executed`/
            // `Reverted` above the floor, breaking the driver's contiguous-downgrade-suffix invariant
            // (a debug_assert trip + a needless re-dispatch). Read the floor UNDER the BBO ticket so a
            // lower ticket's floor is visible; on a downgrade CONSUME the account rank (empty body)
            // like every other deferral. Value-neutral — a None cancel is a no-op revert whether it
            // runs here or re-runs at the barrier — but the driver relies on the invariant.
            let forced = bbo.run(plan.work.ticket, || {
                min_downgrade.load(Ordering::SeqCst) < plan.work.ticket
            });
            if forced {
                account_gate.run(plan.work.canceller, plan.work.rank, || ());
                return Ok(CancelOutcome::Downgrade);
            }
            return account_gate.run(plan.work.canceller, plan.work.rank, || {
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

    // Publish this op's completion at its price on EVERY exit (incl. the `?` paths below), so higher
    // same-price cancels' waits always resolve. Fires on drop AFTER the body's book write has landed.
    let _mark_done = MarkDoneOnDrop {
        price_completion,
        market,
        price,
        ticket: plan.work.ticket,
    };

    // 1. Classify + (for at-best) decide emptiness, all WHILE HOLDING the BBO ticket. Holding it
    //    means no higher-txn_id op classifies until this decision is made, so when an emptying cancel
    //    sets the contagion floor, every higher op is guaranteed to see it (→ forced to the barrier).
    //    The taker-contagion floor check comes first; then below-best (parallel) vs at-best. For
    //    at-best we wait for all lower-txn_id same-price ops to settle the level (a lower-txn_id
    //    downgrade would have set the floor and forced THIS op above, so reaching here means every
    //    lower same-price op executed — its effect is applied, not deferred), then check emptiness.
    enum TicketDecision {
        Downgrade,
        RunParallel,
    }
    let decision = bbo.run(
        plan.work.ticket,
        || -> Result<TicketDecision, PrecompileError> {
            // Forced by a lower-txn_id taker / emptying cancel (every downgrade sets the floor).
            if min_downgrade.load(Ordering::SeqCst) < plan.work.ticket {
                return Ok(TicketDecision::Downgrade);
            }
            let best_bid = storage::load_best_bid(ctx, market)?;
            let best_ask = storage::load_best_ask(ctx, market)?;
            if !cancel_at_best(side, price, best_bid, best_ask) {
                // Below best → cannot move the BBO → remove in parallel.
                return Ok(TicketDecision::RunParallel);
            }
            // At best: still holding the ticket, wait for lower-txn_id same-price ops, then decide.
            price_completion.wait_for(market, price, &plan.required);
            let level = book_lock.run(market, side_u8, || load_level(ctx, market, side, price))?;
            let present = level.iter().any(|id| id == &plan.work.order_id);
            let others_remain = level.iter().any(|id| id != &plan.work.order_id);
            if present && !others_remain {
                // Sole order at the best level → removing it MOVES the BBO. But only an OWNER's cancel
                // actually removes it: a non-owner cancel reverts ("not owner") at the body, touching
                // nothing. So only an owner-cancel is treated like a taker — set the contagion floor
                // (forces every higher-txn_id op to the barrier, so the same maker's later
                // margin-touching ops run AFTER this cancel's deferred margin release) and downgrade;
                // the serial barrier re-runs the removal in txn_id order. A NON-owner sole-best cancel
                // does NOT set the floor (else a griefer cancelling someone's sole-best order would
                // needlessly force the whole tail serial); it runs in parallel and reverts harmlessly,
                // byte-identical to serial. (3d-7 fix — owner read locally, no pre-scan plumbing.)
                let is_owner = storage::load_order(ctx, &plan.work.order_id)?
                    .map(|o| o.owner == plan.work.canceller.0 .0)
                    .unwrap_or(false);
                if is_owner {
                    min_downgrade.fetch_min(plan.work.ticket, Ordering::SeqCst);
                    Ok(TicketDecision::Downgrade)
                } else {
                    Ok(TicketDecision::RunParallel)
                }
            } else {
                // Others remain (or the order is already gone) → BBO unchanged → remove in parallel.
                Ok(TicketDecision::RunParallel)
            }
        },
    )?;

    let outcome = match decision {
        // Forced or emptying → deferred to the barrier; leave the book untouched. Still CONSUME the
        // account rank (empty body) for the dense consume-once contract.
        TicketDecision::Downgrade => {
            account_gate.run(plan.work.canceller, plan.work.rank, || ());
            CancelOutcome::Downgrade
        }
        // Below best, or at-best non-emptying → ticket released; remove under the account gate +
        // book-side lock, in parallel. `remove_from_book_after_cancel` detaches without emptying the
        // top level → no best refresh → safe.
        TicketDecision::RunParallel => {
            gated_cancel(ctx, account_gate, book_lock, &plan.work, market, side_u8)?
        }
    };

    // `_mark_done` fires here on drop (or earlier on any `?`/panic), publishing this op at its price.
    Ok(outcome)
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
        .map(|(i, w)| {
            let required = match resolved[i] {
                Some((m, _s, p)) => items
                    .iter()
                    .zip(&resolved)
                    .filter(|(w2, r2)| {
                        w2.ticket < w.ticket
                            && matches!(r2, Some((m2, _, p2)) if *m2 == m && *p2 == p)
                    })
                    .map(|(w2, _)| w2.ticket)
                    .collect(),
                None => Vec::new(),
            };
            CancelPlanItem {
                work: w.clone(),
                resolved: resolved[i],
                required,
            }
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
    let price_completion = PriceCompletion::new();
    let min_downgrade = AtomicU64::new(u64::MAX);
    let (make_ctx, plans, price_completion, min_downgrade) =
        (&make_ctx, &plans, &price_completion, &min_downgrade);
    thread::scope(|s| {
        let handles: Vec<_> = plans
            .iter()
            .map(|plan| {
                let book = book.clone();
                s.spawn(move || {
                    let mut ctx = make_ctx(book);
                    parallel_cancel(
                        &mut ctx,
                        account_gate,
                        book_lock,
                        bbo,
                        price_completion,
                        min_downgrade,
                        plan,
                    )
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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

    // A non-fatal decode/verify error → Reject (replayed as a revert); a Fatal propagates.
    let to_reject = |e: PrecompileError| -> Result<PerpTxClass, PrecompileError> {
        match e {
            PrecompileError::Fatal(f) => Err(PrecompileError::Fatal(f)),
            other => Ok(PerpTxClass::Reject {
                output: encode_revert_string(&other.to_string()).to_vec(),
            }),
        }
    };

    if selector == placeOrderCall::SELECTOR {
        match decode_place_order(input_bytes, caller, context) {
            Ok(p) => Ok(place_trade(p)),
            Err(e) => to_reject(e),
        }
    } else if selector == placeOrderSignedCall::SELECTOR {
        match verify_place_order_signed(input_bytes, context) {
            Ok(p) => Ok(place_trade(p)),
            Err(e) => to_reject(e),
        }
    } else if selector == cancelOrderCall::SELECTOR {
        match decode_cancel_order(input_bytes, caller) {
            Ok(c) => Ok(cancel_trade(c)),
            Err(e) => to_reject(e),
        }
    } else if selector == cancelOrderSignedCall::SELECTOR {
        match verify_cancel_order_signed(input_bytes, context) {
            Ok(c) => Ok(cancel_trade(c)),
            Err(e) => to_reject(e),
        }
    } else {
        Ok(PerpTxClass::NotTrading)
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

    let ticket = |op: &PerpOp| match op {
        PerpOp::Place(w) => w.ticket,
        PerpOp::Cancel(w) => w.ticket,
    };

    ops.iter()
        .enumerate()
        .map(|(i, op)| match op {
            PerpOp::Place(w) => PreparedOp::Place(w.clone()),
            PerpOp::Cancel(w) => {
                let required = match resolved[i] {
                    Some((m, _s, p)) => ops
                        .iter()
                        .zip(&resolved)
                        .filter(|(o2, r2)| {
                            ticket(o2) < w.ticket
                                && matches!(r2, Some((m2, _, p2)) if *m2 == m && *p2 == p)
                        })
                        .map(|(o2, _)| ticket(o2))
                        .collect(),
                    None => Vec::new(),
                };
                PreparedOp::Cancel(CancelPlanItem {
                    work: w.clone(),
                    resolved: resolved[i],
                    required,
                })
            }
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
    let mut final_results: Vec<OpReplay> = Vec::with_capacity(ops.len());
    let mut markets_touched = std::collections::BTreeSet::new();
    let mut start = 0usize;

    while start < ops.len() {
        // Re-normalize THIS segment → dense segment-local tickets + per-maker ranks (a fresh gate
        // domain each segment), then pre-scan it.
        let seg_ops = normalize_schedule(&ops[start..]);
        let prepared = plan_block(book, &seg_ops, &make_ctx);
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

        // Fresh per-segment gates + contagion floor.
        let account_gate = Arc::new(AccountGate::new());
        let book_lock = Arc::new(BookSideLock::new());
        let bbo = Arc::new(BboTicketLock::new());
        let price_completion = Arc::new(PriceCompletion::new());
        let min_downgrade = Arc::new(AtomicU64::new(u64::MAX));

        // Phase 2: parallel batch on the persistent FIFO pool (deadlock-free: strict FIFO + the
        // scheme's wait-edges-point-lower invariant — see PerpPool docs). Jobs are 'static (Arc'd
        // gates/book + cloned make_ctx); each builds its own thread-local slot ctx.
        let seg_results: Vec<OpReplay> = {
            let tasks: Vec<_> = prepared
                .iter()
                .map(|p| {
                    let p = p.clone();
                    let book = book.clone();
                    let make_ctx = make_ctx.clone();
                    let ag = account_gate.clone();
                    let bl = book_lock.clone();
                    let bbo = bbo.clone();
                    let pc = price_completion.clone();
                    let md = min_downgrade.clone();
                    move || -> Result<OpReplay, PrecompileError> {
                        let mut ctx = make_ctx(book);
                        let result = match &p {
                            PreparedOp::Place(w) => OpResult::Place(parallel_place(
                                &mut ctx, &ag, &bl, &bbo, &pc, &md, w,
                            )?),
                            PreparedOp::Cancel(plan) => OpResult::Cancel(parallel_cancel(
                                &mut ctx, &ag, &bl, &bbo, &pc, &md, plan,
                            )?),
                        };
                        // Drain THIS op's emitted logs from its (fresh, single-op) slot ctx — empty for
                        // a downgraded op (body deferred to the barrier) or a reverted op (logs rolled
                        // back). Re-emitted verbatim on replay so the canonical receipts match serial.
                        let logs = ctx.journal_mut().take_logs();
                        Ok(OpReplay { result, logs })
                    }
                })
                .collect();
            pool.run_batch(tasks)
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?
        };

        // Phase 3a: FIFO-sort THIS segment's parallel rests before any serial op reads a level. Keeps
        // prior level order + appends this segment's rests sorted by (segment-local = global-within-
        // segment) ticket, so the level stays globally FIFO across segments.
        {
            let mut place_items: Vec<PlaceWork> = Vec::new();
            let mut place_results: Vec<Result<PlaceOutcome, PrecompileError>> = Vec::new();
            for (op, r) in seg_ops.iter().zip(&seg_results) {
                if let (PerpOp::Place(w), OpResult::Place(o)) = (op, &r.result) {
                    place_items.push(w.clone());
                    place_results.push(Ok(*o));
                }
            }
            finalize_place_batch_ordering(book, &place_items, &place_results, &make_ctx)?;
        }

        // The contagion floor downgrades a CONTIGUOUS suffix (every op classifies under the BBO ticket
        // after the floor was set, so it sees the floor and downgrades); everything before it executed.
        let first_dg = seg_results.iter().position(|r| {
            matches!(
                r.result,
                OpResult::Place(PlaceOutcome::Downgrade) | OpResult::Cancel(CancelOutcome::Downgrade)
            )
        });

        let Some(dg) = first_dg else {
            // No serial op this segment → the whole remaining tail executed in parallel. Done.
            final_results.extend(seg_results);
            break;
        };
        debug_assert!(
            seg_results[dg..].iter().all(|r| matches!(
                r.result,
                OpResult::Place(PlaceOutcome::Downgrade) | OpResult::Cancel(CancelOutcome::Downgrade)
            )),
            "the contagion floor must downgrade a contiguous suffix"
        );

        // Record the parallel-executed prefix, then run the floor op SERIALLY in place against the
        // FIFO-sorted book.
        final_results.extend(seg_results.into_iter().take(dg));
        final_results.push(run_barrier_op(&seg_ops[dg], book, &make_ctx)?);

        // Re-dispatch the strictly-higher tail (all downgraded → never executed → safe to re-run).
        start += dg + 1;
    }

    // Block-end order-independent mid sample (D3-b) for every market the block touched.
    let mut barrier_ctx = make_ctx(book.clone());
    for m in markets_touched {
        crate::perp_dex::risk::finalize_block_mid_sample(&mut barrier_ctx, m)?;
    }

    Ok(final_results)
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

/// Run one downgraded op (a segment's contagion floor) SERIALLY against the shared book on a FRESH ctx
/// — full matching body for a place, removal for a cancel — disposed via [`dispose_body`] (commit on
/// Ok, propagate Fatal). A fresh ctx per call sidesteps any cross-segment read-cache staleness. The
/// ctx's `perp_is_parallel` stays true so in-body mid sampling is skipped; the driver's block-end
/// sample records the deterministic final mid.
fn run_barrier_op<CTX, F>(
    op: &PerpOp,
    book: &Arc<SharedPerpBook>,
    make_ctx: &F,
) -> Result<OpReplay, PrecompileError>
where
    CTX: ContextTr,
    F: Fn(Arc<SharedPerpBook>) -> CTX,
{
    let mut ctx = make_ctx(book.clone());
    let result = match op {
        PerpOp::Place(w) => {
            let cp = ctx.journal_mut().checkpoint();
            let res = place_order_core(
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
            OpResult::Place(match dispose_body(&mut ctx, cp, res)? {
                BodyDisposition::Committed => PlaceOutcome::Executed,
                BodyDisposition::Reverted => PlaceOutcome::Reverted,
            })
        }
        PerpOp::Cancel(w) => {
            let cp = ctx.journal_mut().checkpoint();
            let res = cancel_order_core(w.canceller, w.order_id, &mut ctx);
            OpResult::Cancel(match dispose_body(&mut ctx, cp, res)? {
                BodyDisposition::Committed => CancelOutcome::Executed,
                BodyDisposition::Reverted => CancelOutcome::Reverted,
            })
        }
    };
    // Drain the floor op's emitted logs (the taker's Trade/OrderRested/PositionChanged, etc.) from its
    // fresh ctx before it drops — re-emitted on replay so the canonical receipt carries them.
    let logs = ctx.journal_mut().take_logs();
    Ok(OpReplay { result, logs })
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

    /// A taker TIF (IOC) is downgraded to the barrier without touching the book — and without
    /// needing a seeded market/account (classify decides before the body runs).
    #[test]
    fn parallel_place_taker_tif_downgrades_without_book_writes() {
        let book = Arc::new(SharedPerpBook::new());
        let mut ctx: TestCtx = Context::new(InMemoryDB::default(), SpecId::CANCUN);
        ctx.journal_mut().set_perp_shared(book.clone());
        let gate = AccountGate::new();
        let book_lock = BookSideLock::new();
        let bbo = BboTicketLock::new();
        let pc = PriceCompletion::new();
        let md = AtomicU64::new(u64::MAX);

        let work = PlaceWork {
            maker: address!("1111111111111111111111111111111111111111"),
            order_id: [1u8; 32],
            market_id: 1,
            side: 0, // Buy
            price: 100,
            qty: 1,
            order_type: 0, // Limit
            tif: 1,        // IOC → taker → downgrade
            client_order_id: [0u8; 16],
            rank: 0,
            ticket: 0,
        };

        let out = parallel_place(&mut ctx, &gate, &book_lock, &bbo, &pc, &md, &work).unwrap();
        assert_eq!(out, PlaceOutcome::Downgrade);
        assert!(
            book.take_delta().is_empty(),
            "a downgrade must write nothing to the book"
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

    /// An at-best cancel that is the SOLE order at the best level → removing it would move the BBO →
    /// `Downgrade` (deferred to the serial barrier), writing NOTHING in the parallel phase. Proven by
    /// comparing the book delta with vs without the cancel batch: identical → the cancel wrote nothing.
    #[test]
    fn parallel_cancel_sole_best_order_downgrades() {
        let a = user_addr(1);
        let x = oid(10);

        // Reference book: setup place only.
        let ref_book = Arc::new(SharedPerpBook::new());
        {
            let mut s = make_slot(ref_book.clone());
            seed(&mut s, &[a]);
            rest(&mut s, a, x, 100 * TICK);
        }
        let ref_delta = ref_book.take_delta();

        // Subject book: same setup, then a cancel batch that must downgrade (X is the sole best order).
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
        assert_eq!(results[0].as_ref().unwrap(), &CancelOutcome::Downgrade);

        let subject_delta = book.take_delta();
        assert_eq!(
            ref_delta, subject_delta,
            "a downgraded cancel must not write to the book in the parallel phase"
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
