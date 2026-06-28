//! Persistent FIFO worker pool for the segmented parallel block driver (catalog #21 spike, step 4).
//!
//! The parallel place/cancel tasks BLOCK on the scheduler gates ([`super::perp_sched`]: BBO ticket,
//! AccountGate, PriceCompletion, BookSideLock). A *bounded* pool running *blocking* tasks is a classic
//! deadlock hazard: a worker can be occupied by a higher-ticket task that is blocked on a lower-ticket
//! task which is still waiting in the queue — if every worker is in that state, no worker is free to
//! run the lower task and the batch wedges. (This is also why a LIFO work-stealing pool like rayon's
//! default is unsafe here.)
//!
//! We dodge it with two structural guarantees:
//!
//! 1. **Strict FIFO, single shared queue.** Jobs are submitted in `ticket` order and pulled
//!    oldest-first by the next free worker. So the set of *running* jobs is always the lowest-ticket
//!    jobs pulled so far, and anything still *queued* has a strictly higher ticket than anything
//!    running. (A single shared queue also load-balances for free — a slow job just means its worker
//!    pulls fewer jobs — so we get the benefit of work-stealing without per-worker queues.)
//! 2. **Every gate wait edge points to a strictly LOWER ticket** (the scheme invariant, enforced by
//!    the driver's `normalize_schedule`). Combined with (1): a *running* job can only block on a lower
//!    ticket, and every lower ticket is itself running-or-done (it was pulled earlier under FIFO),
//!    never queued. So a running job never blocks on a queued job; the lowest running job blocks on
//!    nothing (its deps are complete) and always makes progress; the chain cascades.
//!
//! ⇒ **Deadlock-free at ANY worker count** (even one worker — it degenerates to serial), provided the
//! driver submits in ticket order and upholds the wait-edges-point-lower invariant.
//!
//! Jobs are `'static`: they own `Arc` clones of the shared gates + [`super::shared_perp::SharedPerpBook`]
//! rather than borrowing the driver's stack, so the pool needs no scoped-lifetime `unsafe` (unlike
//! `std::thread::scope` / `rayon::scope`). [`PerpPool::run_batch`] still blocks until every submitted
//! job finishes, so a "batch" is logically scoped even though the jobs are `'static`.

use std::boxed::Box;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::vec::Vec;

/// A type-erased unit of work. `'static` (jobs own their captures).
type Job = Box<dyn FnOnce() + Send + 'static>;

/// Shared FIFO work queue. `jobs == None` signals shutdown to the workers.
struct Queue {
    jobs: Mutex<Option<VecDeque<Job>>>,
    /// Signalled when a job is pushed or on shutdown.
    available: Condvar,
}

impl Queue {
    /// Pop the oldest job, blocking while the queue is empty. Returns `None` once shutdown is signalled
    /// and the queue has drained.
    fn pop(&self) -> Option<Job> {
        let mut guard = self.jobs.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            match guard.as_mut() {
                // Shutdown: drain whatever is left, then stop.
                None => return None,
                Some(q) => {
                    if let Some(job) = q.pop_front() {
                        return Some(job);
                    }
                }
            }
            guard = self.available.wait(guard).unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// A persistent pool of FIFO worker threads. Construct once (per node / per block driver), reuse across
/// every segment and block — only the cheap per-batch bookkeeping is rebuilt per [`run_batch`](Self::run_batch).
pub struct PerpPool {
    queue: Arc<Queue>,
    workers: Vec<JoinHandle<()>>,
}

impl core::fmt::Debug for PerpPool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PerpPool")
            .field("workers", &self.workers.len())
            .finish()
    }
}

impl PerpPool {
    /// Spawn `n_workers` persistent worker threads (clamped to at least 1). The threads live until the
    /// pool is dropped.
    pub fn new(n_workers: usize) -> Self {
        let n_workers = n_workers.max(1);
        let queue = Arc::new(Queue {
            jobs: Mutex::new(Some(VecDeque::new())),
            available: Condvar::new(),
        });
        let workers = (0..n_workers)
            .map(|_| {
                let queue = queue.clone();
                std::thread::Builder::new()
                    .name("perp-pool".into())
                    .spawn(move || {
                        // A panicking job must not poison the queue mutex for the rest of the batch;
                        // `run_batch`'s completion counter would then never reach `n` and the driver
                        // would hang. We can't catch_unwind a `FnOnce` without `UnwindSafe`, so jobs
                        // themselves are responsible for not panicking (they translate errors into
                        // their result type). The worker loop just pulls and runs.
                        while let Some(job) = queue.pop() {
                            job();
                        }
                    })
                    .expect("spawn perp-pool worker")
            })
            .collect();
        Self { queue, workers }
    }

    /// Run `tasks` to completion in FIFO order, returning their results in submission order. Blocks
    /// until every task has finished (the batch "drain"/join).
    ///
    /// CONTRACT: submit in `ticket` order and uphold the wait-edges-point-lower invariant (see module
    /// docs) — otherwise the deadlock-freedom guarantee does not hold.
    pub fn run_batch<R, F>(&self, tasks: Vec<F>) -> Vec<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let n = tasks.len();
        if n == 0 {
            return Vec::new();
        }

        // Per-task result slots + a completion counter. All `Arc`, so the `'static` jobs can own clones.
        let slots: Arc<Vec<Mutex<Option<R>>>> =
            Arc::new((0..n).map(|_| Mutex::new(None)).collect());
        let done: Arc<(Mutex<usize>, Condvar)> = Arc::new((Mutex::new(0usize), Condvar::new()));

        {
            let mut guard = self.queue.jobs.lock().unwrap_or_else(|p| p.into_inner());
            let q = guard.as_mut().expect("run_batch on a shut-down pool");
            for (i, task) in tasks.into_iter().enumerate() {
                let slots = slots.clone();
                let done = done.clone();
                q.push_back(Box::new(move || {
                    let r = task();
                    *slots[i].lock().unwrap_or_else(|p| p.into_inner()) = Some(r);
                    let (count, cv) = &*done;
                    let mut c = count.lock().unwrap_or_else(|p| p.into_inner());
                    *c += 1;
                    if *c == n {
                        cv.notify_all();
                    }
                }));
            }
            // Wake up to `n` idle workers (notify_all: spurious wakes are harmless).
            self.queue.available.notify_all();
        }

        // Drain: wait until every job has incremented the counter.
        {
            let (count, cv) = &*done;
            let mut c = count.lock().unwrap_or_else(|p| p.into_inner());
            while *c < n {
                c = cv.wait(c).unwrap_or_else(|p| p.into_inner());
            }
        }

        // Collect results in submission order. Read through the `Arc` (do not `try_unwrap`: the last
        // job's closure may still be unwinding its stack — and thus holding its `slots` clone — in the
        // instant after it bumped the counter to `n`).
        slots
            .iter()
            .map(|slot| {
                slot.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                    .expect("a pool job did not record its result")
            })
            .collect()
    }
}

impl Drop for PerpPool {
    fn drop(&mut self) {
        // Signal shutdown (clear the queue handle) and wake every worker so it observes it.
        {
            let mut guard = self.queue.jobs.lock().unwrap_or_else(|p| p.into_inner());
            *guard = None;
        }
        self.queue.available.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// Run `f` on a fresh thread under a wall-clock watchdog; panic if it does not finish in time. Used
    /// to turn "the pool deadlocked / wedged" from a hung test (which blocks the whole suite) into a
    /// fast, attributable failure.
    fn within<F: FnOnce() + Send + 'static>(secs: u64, f: F) {
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let d2 = done.clone();
        let h = std::thread::spawn(move || {
            f();
            let (m, cv) = &*d2;
            *m.lock().unwrap() = true;
            cv.notify_all();
        });
        let (m, cv) = &*done;
        let mut g = m.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !*g {
            let now = Instant::now();
            if now >= deadline {
                panic!("watchdog: pool task did not finish within {secs}s (deadlock/wedge)");
            }
            let (ng, _) = cv.wait_timeout(g, deadline - now).unwrap();
            g = ng;
        }
        h.join().unwrap();
    }

    #[test]
    fn run_batch_returns_results_in_submission_order() {
        let pool = PerpPool::new(4);
        within(10, move || {
            let out = pool.run_batch((0..100usize).map(|i| move || i * i).collect());
            assert_eq!(out, (0..100usize).map(|i| i * i).collect::<Vec<_>>());
        });
    }

    #[test]
    fn empty_batch_is_a_noop() {
        let pool = PerpPool::new(2);
        let out: Vec<usize> = pool.run_batch(Vec::<Box<dyn FnOnce() -> usize + Send>>::new());
        assert!(out.is_empty());
    }

    #[test]
    fn pool_is_reusable_across_batches() {
        let pool = PerpPool::new(3);
        within(10, move || {
            for round in 0..5usize {
                let out = pool.run_batch((0..20usize).map(move |i| move || i + round).collect());
                assert_eq!(out, (0..20usize).map(|i| i + round).collect::<Vec<_>>());
            }
        });
    }

    /// The core deadlock-freedom property. Build a chain of BLOCKING jobs where job `i` may not start
    /// its body until job `i-1` has finished — i.e. every wait edge points to a strictly LOWER ticket,
    /// exactly like the real gates. Run it on a pool with FAR FEWER workers than jobs. If FIFO ordering
    /// or the "running never blocks on queued" property were violated, this wedges; the watchdog turns
    /// that into a failure instead of a hang.
    #[test]
    fn blocking_chain_on_undersized_pool_does_not_deadlock() {
        let pool = PerpPool::new(2);
        within(10, move || {
            const N: usize = 50;
            // `progress` is the count of jobs that have completed their body. Job `i` waits until
            // `progress == i` (its lower-ticket predecessor is done), then bumps it to `i + 1`.
            let progress = Arc::new((Mutex::new(0usize), Condvar::new()));
            let tasks: Vec<_> = (0..N)
                .map(|i| {
                    let progress = progress.clone();
                    move || {
                        let (m, cv) = &*progress;
                        let mut g = m.lock().unwrap();
                        while *g != i {
                            g = cv.wait(g).unwrap();
                        }
                        *g = i + 1;
                        cv.notify_all();
                        i
                    }
                })
                .collect();
            let out = pool.run_batch(tasks);
            assert_eq!(out, (0..N).collect::<Vec<_>>());
            assert_eq!(*progress.0.lock().unwrap(), N);
        });
    }

    /// Even a single-worker pool must complete a blocking chain (degenerates to serial; the lowest
    /// ticket always runs first under FIFO so it never blocks on a not-yet-pulled job).
    #[test]
    fn single_worker_runs_blocking_chain_serially() {
        let pool = PerpPool::new(1);
        within(10, move || {
            const N: usize = 30;
            let progress = Arc::new((Mutex::new(0usize), Condvar::new()));
            let tasks: Vec<_> = (0..N)
                .map(|i| {
                    let progress = progress.clone();
                    move || {
                        let (m, cv) = &*progress;
                        let mut g = m.lock().unwrap();
                        while *g != i {
                            g = cv.wait(g).unwrap();
                        }
                        *g = i + 1;
                        cv.notify_all();
                        i
                    }
                })
                .collect();
            assert_eq!(pool.run_batch(tasks), (0..N).collect::<Vec<_>>());
        });
    }

    /// Jobs genuinely run concurrently (not just round-robin-serial): with `W` workers, `W` jobs that
    /// each spin until all `W` have arrived will only all arrive if they run at the same time.
    #[test]
    fn jobs_run_concurrently() {
        const W: usize = 4;
        let pool = PerpPool::new(W);
        within(10, move || {
            let arrived = Arc::new(AtomicUsize::new(0));
            let tasks: Vec<_> = (0..W)
                .map(|_| {
                    let arrived = arrived.clone();
                    move || {
                        arrived.fetch_add(1, Ordering::SeqCst);
                        // Spin until every worker is here. If they were serialized this never settles
                        // (watchdog fires); concurrent execution makes it settle immediately.
                        while arrived.load(Ordering::SeqCst) < W {
                            std::hint::spin_loop();
                        }
                        0usize
                    }
                })
                .collect();
            assert_eq!(pool.run_batch(tasks).len(), W);
        });
    }
}
