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

use crate::perp_dex::types::order::{OrderType, Side, TimeInForce};

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

/// Decide how to handle a cancel given the current BBO and whether removing this order empties its
/// price level. `empties_level` is computed by the driver (the level FIFO holds only this order).
pub fn classify_cancel(
    side: Side,
    price: u64,
    best_bid: u64,
    best_ask: u64,
    empties_level: bool,
) -> LockPlan {
    // The BBO moves only if the cancelled order is AT the best on its side AND removing it empties
    // that level (no other orders remain). Then the new best must be found by scanning the book,
    // which is only correct once all lower-txn_id inserts have landed → serial barrier (3d).
    let at_best = match side {
        Side::Buy => price == best_bid,
        Side::Sell => price == best_ask,
    };
    if at_best && empties_level {
        LockPlan::DowngradeToBarrier
    } else {
        LockPlan::ReleaseTicket
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
            classify_place(Side::Sell, OrderType::Limit, TimeInForce::Gtc, 105, 100, 110),
            LockPlan::HoldTicket
        );
        // sell at 120 > best_ask 110 → non-cross, doesn't move best_ask → release.
        assert_eq!(
            classify_place(Side::Sell, OrderType::Limit, TimeInForce::Gtc, 120, 100, 110),
            LockPlan::ReleaseTicket
        );
        // sell at 100 <= best_bid 100 → crosses → downgrade.
        assert_eq!(
            classify_place(Side::Sell, OrderType::Limit, TimeInForce::Gtc, 100, 100, 110),
            LockPlan::DowngradeToBarrier
        );
    }

    // ── place: PostOnly ────────────────────────────────────────────────────────
    #[test]
    fn postonly_cross_rejects_not_downgrades() {
        // PostOnly buy at 110 >= best_ask 110 → crosses → REJECT (body reverts), not downgrade.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::PostOnly, 110, 100, 110),
            LockPlan::RejectInBody
        );
        // PostOnly buy at 105 < 110 → non-cross, moves best_bid → hold.
        assert_eq!(
            classify_place(Side::Buy, OrderType::Limit, TimeInForce::PostOnly, 105, 100, 110),
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

    // ── cancel ──────────────────────────────────────────────────────────────────
    #[test]
    fn cancel_emptying_best_level_downgrades() {
        // buy order at best_bid 100, removing it empties the level → BBO moves → barrier.
        assert_eq!(
            classify_cancel(Side::Buy, 100, 100, 110, true),
            LockPlan::DowngradeToBarrier
        );
    }

    #[test]
    fn cancel_not_moving_best_releases() {
        // at best but level not emptied (others remain) → BBO unchanged → parallel.
        assert_eq!(
            classify_cancel(Side::Buy, 100, 100, 110, false),
            LockPlan::ReleaseTicket
        );
        // below best, even if it empties that (non-best) level → BBO unchanged → parallel.
        assert_eq!(
            classify_cancel(Side::Buy, 90, 100, 110, true),
            LockPlan::ReleaseTicket
        );
        // sell at best_ask, empties → barrier.
        assert_eq!(
            classify_cancel(Side::Sell, 110, 100, 110, true),
            LockPlan::DowngradeToBarrier
        );
    }
}
