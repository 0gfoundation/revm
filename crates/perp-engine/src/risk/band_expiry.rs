//! Out-of-band resting-order expiry (band GC), one leg of the deferred sweeps.

use super::*;

// ── Out-of-band resting-order expiry (band GC) ─────────────────────────────────

/// Max orders EXPIRED per `updateIndexPrice` by [`run_out_of_band_expiry_sweep`].
///
/// **Unit: orders, not levels.** Every unit of cost here is per order — one book detach, one
/// per-user entry removal, one aggregate write, one order delete, one log — and a single price level
/// can hold an unbounded number of them, so capping levels would cap nothing.
///
/// **Why a cap at all.** This runs on EVERY oracle update, i.e. every block, inside the transaction
/// that also moves the mark, settles funding and runs the liquidation sweep. Users control the
/// book's depth, and the per-market open-order cap (`MAX_OPEN_ORDERS_PER_MARKET`) bounds one
/// account but not the number of accounts, so the out-of-band prefix is unbounded. If
/// draining it could exceed the block gas limit, `updateIndexPrice` would stop landing — and with it
/// the mark, funding and every liquidation. Cheap housekeeping must never be able to take the price
/// feed down; this is the same reasoning that caps the neighbouring sweep at
/// [`MAX_SWEEP_ACTIONS_PER_UPDATE`] and ADL at [`ADL_BUDGET_PER_UPDATE`].
///
/// **Why 64.** It is [`crate::batch::MAX_BATCH_PLACE`] — the number of orders the gas table already
/// accepts one transaction may touch (and each of those may sweep the book, so 64 expiries is
/// strictly less work than one accepted `batchPlaceOrders`). It is also small next to what this
/// selector's budget already contains: 64 cancel-shaped removals are far cheaper than 50
/// liquidations, so the GC never becomes the dominant term in `updateIndexPrice`.
///
/// **When it is hit:** the sweep STOPS and the rest of the prefix keeps resting, unchanged. It is
/// not lost work — the sweep re-runs on the next oracle update (every block), and every unit of
/// budget permanently removes one order, so a stranded prefix of N orders is fully drained in
/// ⌈N/64⌉ updates. Meanwhile the fill-time band in `match_order` is still the backstop: whatever
/// the GC has not reached yet is unmatchable exactly as before, so lagging is safe by construction
/// and never a correctness question — only a latency one.
pub(super) const MAX_BAND_EXPIRIES_PER_UPDATE: u32 = crate::batch::MAX_BATCH_PLACE as u32;

/// Expire the resting orders that a mark move has stranded OUT OF BAND **at the touch**, run
/// synchronously at the tail of [`run_update_index_price`] once the new mark is persisted.
///
/// # Which orders, and why only these
///
/// `match_order` applies a fill-time band centred on `market.mark_price`. On the NEAR side of the
/// walk it `continue`s past out-of-band levels; on the far side it `break`s. There is no
/// placement-time band, so a mark move can leave a level sitting AT THE TOUCH, unmatchable,
/// indefinitely. That is not merely dead weight — the level IS `best_bid`/`best_ask`, and three
/// things read the best with no knowledge of the band:
///
/// 1. `ensure_post_only_does_not_cross` rejects a legitimate opposite-side PostOnly order as "would
///    match" against a level that can never fill;
/// 2. `record_mid_price_sample_for_best_quote_change` feeds the best into the price-basis window →
///    `price2` → the mark median, so an unmatchable quote pollutes the mark itself;
/// 3. every match walk re-skips the whole prefix.
///
/// So the scope is exactly the **contiguous near-side prefix**, and nothing else:
///
/// * asks are ASCENDING ⇒ the harmful prefix is the levels **below `mark_lower`** at the FRONT;
/// * bids are DESCENDING ⇒ it is the levels **above `mark_upper`** at the FRONT.
///
/// ⚠️ The FAR ends — asks above `mark_upper`, bids below `mark_lower` — are ordinary deep passive
/// orders that the fill-time band deliberately lets rest ("lets harmless deep passive orders rest").
/// They are not at the touch, they do not distort the crossing test (a PostOnly buy is compared
/// against `best_ask`, so an `best_ask` ABOVE the band can only make it more permissive, never
/// less), and expiring them would mass-kill legitimate liquidity. Both walks therefore `take_while`
/// and stop at the FIRST in-band level.
///
/// A market with the band disabled (`price_band_bps >= 10_000`, e.g. the benches' and the golden
/// scenario's `1_000_000`) GCs nothing for free: `mark_band_bounds` returns `lower = 0` (no ask is
/// below it) and an enormous `upper` (no bid is above it). Likewise `mark == 0` (band inactive)
/// returns `(u128::MAX, 0)`.
///
/// # Placement: BEFORE the liquidation sweep
///
/// Chosen, not incidental. The sweep closes positions through the book with `match_order` centred on
/// this same new mark, so every level this function removes was already unreachable to it — the GC
/// cannot reduce what the sweep can absorb, and cannot flip a book-absorbed close into an
/// insurance-fund or ADL one. What the GC *does* change is the BBO, and running FIRST is what makes
/// that harmless in the direction that matters: nothing in `run_update_index_price` before this
/// point touches the book, so the `best_bid`/`best_ask` cache is provably LIVE here, which is
/// precisely the precondition [`crate::trading::remove_from_book_after_cancel`] documents. Running
/// after the sweep would hand this function a cache that had just been rewritten by an arbitrary
/// number of matches and mid-match auto-cancels.
///
/// That variant choice earns something beyond correctness: its invariant check ("a removal beyond
/// the cached best is impossible") is a live assertion of this function's entire scoping claim. We
/// only ever drain the front of a side, so each emptied level's price EQUALS the cached best; if a
/// scoping bug ever made it remove an interior level, the guard fires instead of silently corrupting
/// the BBO cache.
///
/// # Cost shape
///
/// Removal goes through the single door, `execute_order_cancellation`, so book removal + entry
/// removal + aggregate update stay together, and the aggregates shrink by each entry's FROZEN
/// `assuming_price` via `release_open_order_margin` — never by the limit price.
///
/// Per-order cost is O(1) writes, NOT O(k) blob rewrites, so expiring k orders of one user is not
/// O(k²): `mutate_buy_orders`/`mutate_sell_orders` edit the resident `VecDeque` IN PLACE and
/// `save_position_reservation_only` sets the aggregates on the resident position struct, with
/// serialization deferred to one write per key at block end. No per-user grouping is needed to get
/// that, which is why the loop is free to run in the deterministic order the event stream wants:
/// price order (ascending asks / descending bids), then FIFO within a level. No hash-map iteration
/// anywhere.
pub(super) fn run_out_of_band_expiry_sweep<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &Market,
) -> Result<bool, PerpError> {
    let (mark_upper, mark_lower) =
        crate::math::mark_band_bounds(market.mark_price, market.price_band_bps);
    let mut budget = MAX_BAND_EXPIRIES_PER_UPDATE;

    // Asks ASCEND, so the stranded prefix is the front: everything strictly below the lower edge.
    // Collect the doomed prices FIRST — the removals below mutate the price index in place, so the
    // walk cannot borrow it.
    let doomed_asks: Vec<u64> = storage::load_ask_prices_ref(context, market_id)?
        .iter()
        .copied()
        .take_while(|p| (*p as u128) < mark_lower)
        // Bounded by the BUDGET, not by the prefix length. Without it a mark gap that strands
        // 10_000 levels walks and allocates all 10_000 on EVERY update, forever, to remove 64.
        //
        // ⚠️ This is only EXACT — never under-using the budget — because a price in the index
        // always has at least one LIVE order: `detach_order_from_level` reports `emptied` and
        // `remove_from_book_after_cancel` drops the price on that signal, so a level whose live
        // count reached 0 is gone from the index entirely (its stale ids go with the blob,
        // `decr_level_count` at storage.rs:2251). Each collected level therefore expires >= 1
        // order and consumes >= 1 budget, so `budget` levels is a true upper bound on how many can
        // be useful. If that invariant were ever relaxed — a price left in the index with zero live
        // orders — this `take` would start leaving budget unspent on a stale-heavy prefix.
        .take(budget as usize)
        .collect();
    for price in doomed_asks {
        if budget == 0 {
            break;
        }
        expire_level(context, market_id, Side::Sell, price, market, &mut budget)?;
    }
    // Remember whether the ASK side exhausted the budget: the bid collect below reads the budget
    // that is left, so a bid prefix that exists but was never collected would otherwise look like
    // "no bid work" at the end.
    let asks_exhausted_budget = budget == 0;

    // Bids DESCEND (the index is ascending, walked in reverse), so the stranded prefix is the top:
    // everything strictly above the upper edge.
    let doomed_bids: Vec<u64> = storage::load_bid_prices_ref(context, market_id)?
        .iter()
        .rev()
        .copied()
        .take_while(|p| (*p as u128) > mark_upper)
        // Same bound, and it reads the budget REMAINING after the ask side — so when the asks
        // consumed everything this collects nothing at all rather than materialising a bid prefix
        // the loop below would immediately `break` out of.
        .take(budget as usize)
        .collect();
    for price in doomed_bids {
        if budget == 0 {
            break;
        }
        expire_level(context, market_id, Side::Buy, price, market, &mut budget)?;
    }

    // Budget gone ⇒ at least one side was cut short. Conservative in the same direction as the
    // liquidation sweep: a budget spent exactly as the last stranded order went sets this, and the
    // next drain finds nothing and clears the bit.
    //
    // ⚠️ `asks_exhausted_budget` is why this is not just `budget == 0` evaluated once at the end:
    // the ask side can consume everything, leaving the bid prefix uncollected AND the bid loop
    // never entered. Both roads lead to `budget == 0` here, which is the point — but the variable
    // records that the bid side was never even LOOKED at, which is the starvation shape worth
    // keeping visible in the code rather than only in a comment.
    Ok(budget == 0 || asks_exhausted_budget)
}

/// Expire the live orders at ONE out-of-band price level, in FIFO order, until `budget` runs out.
///
/// Stale ids (lazy-queue leftovers whose order record is already deleted, or terminal) cost one
/// order-map probe and **do not consume budget**. That is deliberate: charging them would let a
/// level thick with stale ids burn the whole budget without removing anything, and the next update
/// would re-scan the same prefix forever — the sweep would stop converging. Charging only real
/// expiries makes every unit of budget permanent progress. The scan itself is the same sweep the
/// match walk already performs over these queues, and it is paid at most once per level: draining
/// the level takes its live count to 0, which deletes the blob and its stale ids with it.
fn expire_level<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    price: u64,
    market: &Market,
    budget: &mut u32,
) -> Result<(), PerpError> {
    // Snapshot the FIFO ids: each removal below mutates this level's blob.
    //
    // ⚠️ NOT bounded by `budget`, deliberately — unlike the price-prefix collects in the caller.
    // Stale and terminal ids are skipped WITHOUT consuming budget (see the doc above), so the
    // number of ids this loop must walk to spend one unit of budget is unbounded by the budget
    // itself. Truncating here to `budget` would silently stop at the first `budget` ids and leave a
    // level whose front is all stale permanently un-drained — the exact non-convergence the
    // free-stale-skip rule exists to prevent.
    let ids: Vec<[u8; 32]> = match side {
        Side::Buy => storage::load_bid_level_arc(context, market_id, price)?
            .ids
            .clone(),
        Side::Sell => storage::load_ask_level_arc(context, market_id, price)?
            .ids
            .clone(),
    };

    for order_id in ids {
        if *budget == 0 {
            return Ok(());
        }
        let Some(order) = storage::load_order(context, &order_id)? else {
            continue; // stale id, already gone
        };
        if !matches!(
            order.status,
            OrderStatus::Open | OrderStatus::PartiallyFilled
        ) {
            continue; // terminal but not yet swept
        }
        let user = Address::from(order.owner);
        crate::trading::execute_order_cancellation(
            context,
            user,
            market_id,
            order_id,
            order,
            market,
            // Nothing in this call has matched yet (this runs before the liquidation sweep), so the
            // BBO cache is live — and its "no removal beyond the cached best" invariant doubles as
            // an assertion that we only ever drain the front of a side. See the doc comment above.
            crate::trading::remove_from_book_after_cancel,
            CancelReason::PriceBandExpiry,
        )?;
        *budget -= 1;
    }
    Ok(())
}
