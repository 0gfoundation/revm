//! Sim-only sequential stage profiler (feature `bench-util`; zero-cost no-op otherwise).
//!
//! `mark(stage)` attributes the wall time elapsed since the previous `mark`/`reset` to `stage`.
//! A thread-local carries the clock across the whole call chain, so stages can be marked in
//! different functions of one op and still sum to the op's total. Single-threaded sim only.

// Stage ids (cancel path 0..=6; place path 7..=13).
pub const S_LOAD_ORDER: usize = 0; // decode/load order + auth + market
pub const S_BOOK_DETACH: usize = 1; // detach from price-level FIFO
pub const S_LOAD_POS: usize = 2; // load position + account + order price
pub const S_LIST_MUT: usize = 3; // order-list remove + cover-tree maintenance
pub const S_RESV: usize = 4; // flip-aware reservation recompute
pub const S_APPLY_SAVE: usize = 5; // apply release effect + save position/account
pub const S_DELETE_EVT: usize = 6; // delete order record + emit event

pub const P_LOAD: usize = 7; // place: load pos/account/BBO + build new entry
pub const P_MATCH: usize = 8; // place: matching engine
pub const P_RESV: usize = 9; // place: reservation validate
pub const P_APPLY: usize = 10; // place: wallet debit + commit aggregates
pub const P_LIST_MUT: usize = 11; // place: list insert + tree maintenance
pub const P_BOOK: usize = 12; // place: price-level insert + best-quote update
pub const P_ORDER_SAVE: usize = 13; // place: save order + events

pub const N: usize = 14;

pub const STAGE_NAMES: [&str; N] = [
    "load_order", "book_detach", "load_pos", "list_mut", "reservation", "apply_save", "delete_evt",
    "p_load", "p_match", "p_resv", "p_apply", "p_list_mut", "p_book", "p_order_save",
];

#[cfg(feature = "bench-util")]
mod imp {
    use super::N;
    use std::cell::Cell;
    use std::time::Instant;

    thread_local! {
        static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
        static ON: Cell<bool> = const { Cell::new(false) };
        static TREE: Cell<bool> = const { Cell::new(true) };
        static MERGE: Cell<bool> = const { Cell::new(true) };
        static ACC: [Cell<u64>; N] = Default::default();
    }

    #[inline]
    fn on() -> bool {
        ON.with(|c| c.get())
    }
    pub fn set_on(v: bool) {
        ON.with(|c| c.set(v));
    }
    /// Runtime switch: whether the cancel reservation uses the cover tree (true) or the linear fold
    /// (false) — lets the profiler compare both from one binary (PERP_NOTREE=1 → linear).
    pub fn set_tree(v: bool) {
        TREE.with(|c| c.set(v));
    }
    /// Runtime switch for the merged-write optimizations (#C1/C2/C3): true = one position write +
    /// one account write carrying the allocation delta; false = the original save_position (with its
    /// re-read + registry hooks + adjust_total_perp_collateral) followed by save_account. Lets the
    /// A/B run from ONE binary so instrumentation overhead is identical on both sides.
    pub fn set_merge(v: bool) {
        MERGE.with(|c| c.set(v));
    }
    #[inline]
    pub fn merge_enabled() -> bool {
        MERGE.with(|c| c.get())
    }
    #[inline]
    pub fn tree_enabled() -> bool {
        TREE.with(|c| c.get())
    }
    #[inline]
    pub fn reset() {
        if on() {
            LAST.with(|c| c.set(Some(Instant::now())));
        }
    }
    #[inline]
    pub fn mark(stage: usize) {
        if !on() {
            return;
        }
        let now = Instant::now();
        LAST.with(|l| {
            if let Some(t) = l.get() {
                ACC.with(|a| a[stage].set(a[stage].get() + now.duration_since(t).as_nanos() as u64));
            }
            l.set(Some(now));
        });
    }
    pub fn clear() {
        ACC.with(|a| {
            for c in a.iter() {
                c.set(0);
            }
        });
        LAST.with(|c| c.set(None));
    }
    pub fn snapshot() -> [u64; N] {
        ACC.with(|a| std::array::from_fn(|i| a[i].get()))
    }
}

#[cfg(not(feature = "bench-util"))]
mod imp {
    use super::N;
    #[inline(always)]
    pub fn set_on(_: bool) {}
    #[inline(always)]
    pub fn set_tree(_: bool) {}
    #[inline(always)]
    pub fn set_merge(_: bool) {}
    #[inline(always)]
    pub fn merge_enabled() -> bool {
        true
    }
    #[inline(always)]
    pub fn tree_enabled() -> bool {
        true
    }
    #[inline(always)]
    pub fn reset() {}
    #[inline(always)]
    pub fn mark(_: usize) {}
    #[inline(always)]
    pub fn clear() {}
    #[inline(always)]
    pub fn snapshot() -> [u64; N] {
        [0; N]
    }
}

pub use imp::*;
