//! Scheduler synchronization primitives for PARALLEL block execution (catalog #21 spike, step 2).
//!
//! Two "ticket" gates encode the ordering rules of the parallel place/cancel scheme. The public API
//! is closure form — [`AccountGate::run`] / [`BboTicketLock::run`] — which waits until it is this
//! caller's turn, runs the body, and advances the cursor on the way out **regardless of how the body
//! returns (Ok / Err / panic)**. So a rank/ticket can never be allocated without being consumed (the
//! cursor cannot wedge on a reverting transaction), and no lock is held across the body (the gates
//! cannot be in a deadlock cycle).
//!
//! - [`AccountGate`] (rule 6): serializes a single maker account's transactions in their block-order
//!   `rank`, so path-dependent margin math runs in the same order as a serial txn_id execution.
//!   Distinct accounts are independent (per-account lanes).
//! - [`BboTicketLock`] (rules 2+3): grants exclusive access to one market's BBO strictly in `ticket`
//!   order (= txn_id order within a parallel batch). Exclusivity is implicit: only the caller whose
//!   ticket equals the serve cursor runs, and the cursor does not advance until that body completes.
//!
//! ## Driver contract (step 3 MUST uphold these; the primitives fail-stop, never silently diverge)
//! - **Dense consume-once:** within a parallel batch the driver assigns ranks `0..k` per account and
//!   tickets `0..N` globally, and calls `run` exactly once per id (a txn that needs no BBO work calls
//!   `bbo.run(ticket, || ())` to pass the ticket). A missing id wedges the lane (a fail-stop hang in
//!   release; a `debug_assert` fires in dev). A reused/decreasing id is a bug caught by `debug_assert`.
//! - **One live turn/ticket per thread:** a slot must not hold an account turn (be inside `run`) and
//!   re-enter the SAME account's `run`, nor nest `BboTicketLock::run` on the same lock — that
//!   self-deadlocks (the advance only happens when the outer body returns). One txn = one account
//!   `run` wrapping at most one `bbo.run`.
//! - **Per-block lifetime:** gates are single-block-scoped; the driver constructs fresh gates each
//!   block. A reused gate would carry stale cursors into the next block's id space.
//! - **Visibility (rule 4.iii caveat):** a ticket holder's writes to BBO-relevant `SharedPerpBook`
//!   keys are published to the next ticket by the serve-cursor release/acquire on body exit; they
//!   MUST therefore complete *before* the `bbo.run` body returns. The non-crossing insert path
//!   releases the BBO (returns from `bbo.run`) and then writes ONLY level-guard-protected keys
//!   (whose visibility the level guard's own lock carries) — never a BBO key after release.
//!
//! D3 (deadlock-free order): the only *held* locks during a body are the BBO ticket (logical) and a
//! per-price-level `SharedPerpBook` DashMap guard; the discipline is **BBO -> level**. No path takes
//! a level guard then the BBO. The account `rank` is a pre-wait, not a held lock, so it is outside
//! the cycle.
//!
//! Step-2 scope: the primitives + their concurrency contracts. NO driver, NO precompile, NO body
//! state machine, NO txn_id/rank assignment (all step 3). The serial barrier for market/match
//! (rule 1) is the driver's phase boundary, not a primitive here.

use dashmap::DashMap;
use primitives::Address;
use std::collections::HashSet;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

/// Poison-tolerant lock of a cursor mutex. The protected value is a plain monotonic counter with no
/// invariant that a panic could break, so recovering a poisoned guard is sound — and it stops one
/// slot's panic from cascade-poisoning the gate for every other slot (which would escalate a single
/// transaction's failure into a block-wide abort / non-deterministic divergence).
#[inline]
fn lock_cursor(m: &Mutex<u64>) -> MutexGuard<'_, u64> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Per-maker-account ordered gate (rule 6). One lane per account; a transaction with block-order
/// `rank` waits until the account's applied count reaches `rank`, runs, then advances.
#[derive(Debug, Default)]
pub struct AccountGate {
    lanes: DashMap<Address, Arc<Lane>>,
}

#[derive(Debug)]
struct Lane {
    /// How many of this account's transactions have completed (the next allowed `rank`).
    applied: Mutex<u64>,
    cv: Condvar,
}

impl AccountGate {
    /// Creates an empty (single-block-scoped) gate.
    pub fn new() -> Self {
        Self {
            lanes: DashMap::new(),
        }
    }

    fn lane(&self, account: Address) -> Arc<Lane> {
        // `entry` holds the shard lock, so racing creators of the same account share one lane.
        self.lanes
            .entry(account)
            .or_insert_with(|| {
                Arc::new(Lane {
                    applied: Mutex::new(0),
                    cv: Condvar::new(),
                })
            })
            .clone()
    }

    /// Runs `f` as `account`'s `rank`-th transaction of the block: blocks until the account's prior
    /// `rank` transactions have completed, runs `f`, then advances the account's count — on Ok, Err,
    /// or panic (the advance is in a guard's `Drop`, which runs on unwind). Skipping a turn = call
    /// with an empty body.
    pub fn run<R>(&self, account: Address, rank: u64, f: impl FnOnce() -> R) -> R {
        let lane = self.lane(account);
        {
            let mut applied = lock_cursor(&lane.applied);
            debug_assert!(
                rank >= *applied,
                "account rank {rank} already consumed (applied={})",
                *applied
            );
            // `applied` only ever increases (one per turn), so this terminates for an in-order rank.
            while *applied != rank {
                applied = lane.cv.wait(applied).unwrap_or_else(|p| p.into_inner());
            }
        }
        // Advance on the way out (incl. panic) — see `LaneAdvance`.
        let _advance = LaneAdvance { lane };
        f()
    }
}

/// Advances an account lane's applied count on drop (so success, revert, and panic all advance).
struct LaneAdvance {
    lane: Arc<Lane>,
}

impl Drop for LaneAdvance {
    fn drop(&mut self) {
        let mut applied = lock_cursor(&self.lane.applied);
        *applied += 1;
        self.lane.cv.notify_all();
    }
}

/// Ordered exclusive lock for one market's BBO (rules 2+3). Access is granted strictly in `ticket`
/// order (the driver assigns tickets `0..N` in txn_id order within a parallel batch).
#[derive(Debug, Default)]
pub struct BboTicketLock {
    /// The ticket currently allowed to run.
    serve: Mutex<u64>,
    cv: Condvar,
}

impl BboTicketLock {
    /// Creates a lock whose first served ticket is 0.
    pub fn new() -> Self {
        Self {
            serve: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    /// Runs `f` while holding ticket `ticket`: blocks until it is this ticket's turn, runs `f`
    /// (exclusively — only the serve-cursor ticket runs, and the cursor does not advance until `f`
    /// returns), then advances the cursor — on Ok, Err, or panic. Passing a ticket without BBO work
    /// = call with an empty body.
    pub fn run<R>(&self, ticket: u64, f: impl FnOnce() -> R) -> R {
        {
            let mut serve = lock_cursor(&self.serve);
            debug_assert!(
                ticket >= *serve,
                "bbo ticket {ticket} already served (serve={})",
                *serve
            );
            while *serve != ticket {
                serve = self.cv.wait(serve).unwrap_or_else(|p| p.into_inner());
            }
        }
        // Advance on the way out (incl. panic) — see `ServeAdvance`.
        let _advance = ServeAdvance { lock: self };
        f()
    }
}

/// Advances the BBO serve cursor on drop (so success, revert, and panic all advance).
struct ServeAdvance<'a> {
    lock: &'a BboTicketLock,
}

impl Drop for ServeAdvance<'_> {
    fn drop(&mut self) {
        let mut serve = lock_cursor(&self.lock.serve);
        *serve += 1;
        self.lock.cv.notify_all();
    }
}

/// Per-(market, price) completion tracker for the parallel cancel path (Harry's optimization). Every
/// place/cancel marks `(market, price, txn_id)` done on completion; a cancel at the BEST price waits
/// (`wait_for`) for the lower-txn_id ops at its price (from the pre-scan) to all be done, so the
/// level's MEMBERSHIP is complete before it decides whether removing its order empties the best
/// level. (Below-best cancels and all places don't wait — they just `mark_done`.) Poison-tolerant.
#[derive(Debug, Default)]
pub struct PriceCompletion {
    levels: DashMap<(u64, u64), Arc<PriceLevel>>,
}

#[derive(Debug, Default)]
struct PriceLevel {
    done: Mutex<HashSet<u64>>,
    cv: Condvar,
}

impl PriceCompletion {
    /// Creates an empty (single-block-scoped) tracker.
    pub fn new() -> Self {
        Self {
            levels: DashMap::new(),
        }
    }

    fn level(&self, market: u64, price: u64) -> Arc<PriceLevel> {
        self.levels.entry((market, price)).or_default().clone()
    }

    /// Records that op `txn_id` finished touching `(market, price)`, waking waiters. Called for EVERY
    /// place/cancel at the price (incl. reverted/downgraded), so a waiter's required set always
    /// resolves (the op either rested there or decided not to — either way it is "done" w.r.t. this
    /// level's membership).
    pub fn mark_done(&self, market: u64, price: u64, txn_id: u64) {
        let level = self.level(market, price);
        let mut done = level.done.lock().unwrap_or_else(|p| p.into_inner());
        done.insert(txn_id);
        level.cv.notify_all();
    }

    /// Blocks until every txn_id in `required` has been marked done at `(market, price)`. Empty
    /// `required` returns immediately. Poison-tolerant (a panicked marker leaves the set readable).
    pub fn wait_for(&self, market: u64, price: u64, required: &[u64]) {
        if required.is_empty() {
            return;
        }
        let level = self.level(market, price);
        let mut done = level.done.lock().unwrap_or_else(|p| p.into_inner());
        while !required.iter().all(|t| done.contains(t)) {
            done = level.cv.wait(done).unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// Per-(market, side) mutual-exclusion lock guarding ALL book mutations on one side of one market's
/// order book — the level FIFO queues AND the side's price list. Both are cross-maker shared and are
/// mutated via a load → modify → store sequence that is NOT atomic against the concurrent
/// [`super::shared_perp::SharedPerpBook`] (load returns an owned `Vec`, the store happens later), so
/// two parallel ops touching the same side can lose an append/removal. Every parallel place/cancel
/// runs its book-mutating body under this lock for its order's side.
///
/// It is the INNERMOST lock in the discipline (BBO ticket → AccountGate → BookSideLock): a leaf that
/// never blocks on anything while held, so it cannot participate in a deadlock cycle regardless of
/// the order in which slots reach it. Poison-tolerant (a panicked holder leaves the side usable).
#[derive(Debug, Default)]
pub struct BookSideLock {
    sides: DashMap<(u64, u8), Arc<Mutex<()>>>,
}

impl BookSideLock {
    /// Creates an empty (single-block-scoped) per-side lock table.
    pub fn new() -> Self {
        Self {
            sides: DashMap::new(),
        }
    }

    fn side_lock(&self, market: u64, side: u8) -> Arc<Mutex<()>> {
        self.sides.entry((market, side)).or_default().clone()
    }

    /// Runs `f` while holding the `(market, side)` book lock. Poison-tolerant: a previously poisoned
    /// side is recovered rather than propagating the panic.
    pub fn run<R>(&self, market: u64, side: u8, f: impl FnOnce() -> R) -> R {
        let lock = self.side_lock(market, side);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::UnsafeCell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Barrier, Mutex as StdMutex};
    use std::time::Duration;

    fn addr(n: u8) -> Address {
        Address::with_last_byte(n)
    }

    /// Rule 6: same-account transactions take effect in `rank` order regardless of thread schedule.
    /// Looped to shake out lost/spurious-wakeup interleavings (cheap, sub-ms gates).
    #[test]
    fn account_gate_serializes_same_account_in_rank_order() {
        const N: u64 = 8;
        let acct = addr(1);
        for _round in 0..25 {
            let gate = AccountGate::new();
            let order = StdMutex::new(Vec::<u64>::new());
            let start = Barrier::new(N as usize);
            std::thread::scope(|s| {
                for rank in (0..N).rev() {
                    let (gate, order, start) = (&gate, &order, &start);
                    s.spawn(move || {
                        start.wait();
                        gate.run(acct, rank, || order.lock().unwrap().push(rank));
                    });
                }
            });
            assert_eq!(*order.lock().unwrap(), (0..N).collect::<Vec<_>>());
        }
    }

    /// Distinct accounts must not block each other (per-account lanes, not a global lock).
    #[test]
    fn account_gate_distinct_accounts_are_independent() {
        let gate = AccountGate::new();
        let (a_in_tx, a_in_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (b_in_tx, b_in_rx) = mpsc::channel::<()>();

        std::thread::scope(|s| {
            let g = &gate;
            s.spawn(move || {
                g.run(addr(1), 0, || {
                    a_in_tx.send(()).unwrap();
                    // Hold account A's turn until released — B (a different account) must proceed.
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                });
            });
            s.spawn(move || {
                g.run(addr(2), 0, || b_in_tx.send(()).unwrap());
            });

            a_in_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            b_in_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("distinct account B blocked behind A");
            release_tx.send(()).unwrap();
        });
    }

    /// Rule 6 under contention: many accounts each with many ranks, interleaved — each account's
    /// ranks still apply in order (proves lanes don't alias / reset under churn).
    #[test]
    fn account_gate_multi_account_ordered_under_contention() {
        const M: u8 = 4;
        const K: u64 = 6;
        let gate = AccountGate::new();
        let recorded = StdMutex::new(Vec::<(u8, u64)>::new());
        let start = Barrier::new((M as usize) * (K as usize));

        std::thread::scope(|s| {
            for a in 0..M {
                for rank in (0..K).rev() {
                    let (gate, recorded, start) = (&gate, &recorded, &start);
                    s.spawn(move || {
                        start.wait();
                        gate.run(addr(a), rank, || recorded.lock().unwrap().push((a, rank)));
                    });
                }
            }
        });

        let recorded = recorded.lock().unwrap();
        for a in 0..M {
            let ranks: Vec<u64> = recorded.iter().filter(|(x, _)| *x == a).map(|(_, r)| *r).collect();
            assert_eq!(ranks, (0..K).collect::<Vec<_>>(), "account {a} out of order");
        }
    }

    /// An account turn advances even if its body PANICS (revert/panic must not wedge the lane).
    #[test]
    fn account_gate_advances_on_panic() {
        let gate = AccountGate::new();
        let acct = addr(1);
        let reached = AtomicBool::new(false);
        std::thread::scope(|s| {
            let g = &gate;
            s.spawn(move || {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    g.run(acct, 0, || panic!("body blew up"));
                }));
            });
            let (g, reached) = (&gate, &reached);
            s.spawn(move || {
                g.run(acct, 1, || reached.store(true, Ordering::SeqCst));
            });
        });
        assert!(reached.load(Ordering::SeqCst), "rank 1 wedged by a panicking rank 0");
    }

    /// The documented skip protocol: a turn consumed with an empty body still advances the lane.
    #[test]
    fn account_gate_empty_run_skips() {
        let gate = AccountGate::new();
        let acct = addr(1);
        gate.run(acct, 0, || {}); // skip rank 0
        // rank 1 must now be runnable (would hang if the skip did not advance).
        let ran = gate.run(acct, 1, || 42);
        assert_eq!(ran, 42);
    }

    /// Rules 2+3: the BBO lock is granted strictly in ticket (txn_id) order. Looped for robustness.
    #[test]
    fn bbo_ticket_lock_grants_in_ticket_order() {
        const N: u64 = 8;
        for _round in 0..25 {
            let lock = BboTicketLock::new();
            let order = StdMutex::new(Vec::<u64>::new());
            let start = Barrier::new(N as usize);
            std::thread::scope(|s| {
                for ticket in (0..N).rev() {
                    let (lock, order, start) = (&lock, &order, &start);
                    s.spawn(move || {
                        start.wait();
                        lock.run(ticket, || order.lock().unwrap().push(ticket));
                    });
                }
            });
            assert_eq!(*order.lock().unwrap(), (0..N).collect::<Vec<_>>());
        }
    }

    /// The serve cursor does NOT advance until the holder's body completes (advance == drop). A
    /// non-exclusive / advance-at-acquire impl would let ticket 1 enter while ticket 0 still holds.
    #[test]
    fn bbo_ticket_advance_is_gated_on_body_completion() {
        let lock = BboTicketLock::new();
        let entered = AtomicBool::new(false);
        let (in_tx, in_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        std::thread::scope(|s| {
            let l = &lock;
            s.spawn(move || {
                l.run(0, || {
                    in_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                });
            });
            let (l, entered) = (&lock, &entered);
            s.spawn(move || {
                l.run(1, || entered.store(true, Ordering::SeqCst));
            });

            in_rx.recv_timeout(Duration::from_secs(5)).unwrap(); // ticket 0 is inside its body
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                !entered.load(Ordering::SeqCst),
                "ticket 1 entered while ticket 0 still held"
            );
            release_tx.send(()).unwrap(); // ticket 0 completes -> advance -> ticket 1 runs
        });
        assert!(entered.load(Ordering::SeqCst));
    }

    /// A ticket advances even if its body PANICS.
    #[test]
    fn bbo_ticket_advances_on_panic() {
        let lock = BboTicketLock::new();
        let reached = AtomicBool::new(false);
        std::thread::scope(|s| {
            let l = &lock;
            s.spawn(move || {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    l.run(0, || panic!("body blew up"));
                }));
            });
            let (l, reached) = (&lock, &reached);
            s.spawn(move || {
                l.run(1, || reached.store(true, Ordering::SeqCst));
            });
        });
        assert!(reached.load(Ordering::SeqCst), "ticket 1 wedged by a panicking ticket 0");
    }

    /// Exclusivity smoke test: never two holders in the critical section at once.
    #[test]
    fn bbo_ticket_lock_is_exclusive() {
        const N: u64 = 8;
        let lock = BboTicketLock::new();
        let active = AtomicUsize::new(0);
        let max_seen = AtomicUsize::new(0);
        let start = Barrier::new(N as usize);
        std::thread::scope(|s| {
            for ticket in (0..N).rev() {
                let (lock, active, max_seen, start) = (&lock, &active, &max_seen, &start);
                s.spawn(move || {
                    start.wait();
                    lock.run(ticket, || {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        max_seen.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(1));
                        active.fetch_sub(1, Ordering::SeqCst);
                    });
                });
            }
        });
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    /// The gate's own release/acquire edge publishes a ticket's writes to the next ticket, proven on
    /// NON-atomic data (so the test depends on the gate, not on atomic fences). Access to the cell is
    /// serialized by the ticket order, making the `unsafe` sound.
    #[test]
    fn bbo_ticket_publishes_writes_to_next_ticket() {
        struct Scratch(UnsafeCell<u64>);
        // SAFETY: every access happens inside `BboTicketLock::run`, which guarantees exactly one
        // ticket runs at a time and publishes its writes to the next via the serve cursor.
        unsafe impl Sync for Scratch {}

        const N: u64 = 16;
        for _round in 0..25 {
            let lock = BboTicketLock::new();
            let scratch = Scratch(UnsafeCell::new(u64::MAX));
            let start = Barrier::new(N as usize);
            std::thread::scope(|s| {
                for ticket in (0..N).rev() {
                    let (lock, scratch, start) = (&lock, &scratch, &start);
                    s.spawn(move || {
                        start.wait();
                        lock.run(ticket, || {
                            // SAFETY: exclusive under the ticket; see the `unsafe impl Sync` note.
                            let cell = scratch.0.get();
                            if ticket > 0 {
                                let seen = unsafe { *cell };
                                assert_eq!(seen, ticket - 1, "stale read: gate did not publish");
                            }
                            unsafe { *cell = ticket };
                        });
                    });
                }
            });
        }
    }

    /// A best-price cancel's `wait_for` must block until ALL its required (lower-same-price) txn_ids
    /// are marked done — not just any subset, and unrelated txn_ids don't satisfy it.
    #[test]
    fn price_completion_waits_for_all_required() {
        let pc = PriceCompletion::new();
        let proceeded = AtomicBool::new(false);
        std::thread::scope(|s| {
            let (pc, proceeded) = (&pc, &proceeded);
            s.spawn(move || {
                pc.wait_for(1, 100, &[0, 1]);
                proceeded.store(true, Ordering::SeqCst);
            });
            // Mark 0 and an unrelated 2, but NOT 1 — the waiter must stay blocked.
            pc.mark_done(1, 100, 0);
            pc.mark_done(1, 100, 2);
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                !proceeded.load(Ordering::SeqCst),
                "wait_for returned before required txn 1 was done"
            );
            // Now complete the required set; the waiter must proceed.
            pc.mark_done(1, 100, 1);
        });
        assert!(
            proceeded.load(Ordering::SeqCst),
            "wait_for never returned after all required txns were done"
        );
    }

    /// The book-side lock serializes a non-atomic read-modify-write on the SAME (market, side): N
    /// threads each load → +1 → store a shared cell under the lock; all N increments must land (no
    /// lost update). Two distinct sides use distinct locks (so they never block each other), which we
    /// exercise by hammering both sides concurrently and checking each side's total independently.
    #[test]
    fn book_side_lock_serializes_same_side_read_modify_write() {
        const N: usize = 16;
        for _round in 0..20 {
            let lock = BookSideLock::new();
            // SAFETY: every access to a cell is performed under that side's book lock, so the
            // read-modify-write below is exclusive per side.
            struct Cell(UnsafeCell<usize>);
            unsafe impl Sync for Cell {}
            let bid = Cell(UnsafeCell::new(0));
            let ask = Cell(UnsafeCell::new(0));
            let start = Barrier::new(N * 2);
            std::thread::scope(|s| {
                let (lock, bid, ask, start) = (&lock, &bid, &ask, &start);
                for _ in 0..N {
                    s.spawn(move || {
                        start.wait();
                        lock.run(7, 0, || {
                            let c = bid.0.get();
                            let v = unsafe { *c };
                            unsafe { *c = v + 1 };
                        });
                    });
                    s.spawn(move || {
                        start.wait();
                        lock.run(7, 1, || {
                            let c = ask.0.get();
                            let v = unsafe { *c };
                            unsafe { *c = v + 1 };
                        });
                    });
                }
            });
            assert_eq!(unsafe { *bid.0.get() }, N, "lost update on bid side");
            assert_eq!(unsafe { *ask.0.get() }, N, "lost update on ask side");
        }
    }
}
