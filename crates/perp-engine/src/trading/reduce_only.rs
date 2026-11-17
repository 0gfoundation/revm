//! reduce-only: the one invariant, and the one predicate that enforces it.
//!
//! # The invariant
//!
//! **A reduce-only order may never produce an opening fill.** Everything in this module exists to
//! keep that true; the fill path carries only a `debug_assert` watchdog, not a clamp.
//!
//! # The predicate
//!
//! Walk the user's same-side resting orders from the touch outward and accumulate **every**
//! quantity — normal orders *and* reduce-only orders, because both consume the position when they
//! fill. At each reduce-only order the running total must fit inside `|position|`:
//!
//! ```text
//! for every reduce-only order R:
//!     Σ{quantities nearer the touch than R}  +  qty(R)   ≤   |position|
//! ```
//!
//! ## Why the nearer REDUCE-ONLY orders count too
//!
//! The obvious reading — "count the normal orders ahead of me" — is not enough, and the gap is
//! reachable without a single normal order in the book:
//!
//! ```text
//! |P| = 10, no normal orders anywhere
//!   admit R₁ 10u   -> nothing ahead, 10 ≤ 10  ✓
//!   admit R₂ 10u   -> nothing ahead, 10 ≤ 10  ✓   ← blind to R₁
//!   a sweep: R₂ closes 10 -> flat; R₁ then OPENS 10 the other way
//! ```
//!
//! Counting both kinds is what makes the condition subsume the two separate things Binance's spec
//! writes down (`room = |P| − Σ reduce-only` and `ahead = Σ normal orders in front`): at the
//! FARTHEST reduce-only order the accumulated reduce-only term is the whole `Σ`, so `room ≥ 0`
//! falls out; with no reduce-only order ahead it degenerates to `ahead + qty ≤ |P|`.
//!
//! ## It collapses to ONE check
//!
//! The running total is monotone non-decreasing in distance from the touch, so the condition is
//! tightest at the farthest reduce-only order and checking that one suffices:
//!
//! ```text
//! Σ{normal quantities nearer the touch than the FARTHEST reduce-only order}
//!   +  Σ{all reduce-only quantities}                                          ≤  |position|
//! ```
//!
//! One ordered pass, one comparison — see [`prefix_terms`].
//!
//! # Relationship to Binance, and the deviation we are taking
//!
//! `misc/binance-margin-verified-model.md` §1.6 (R16–R20) writes admission as two predicates and
//! leaves open whether they share one budget. Three readings survive the evidence, and only the
//! prefix condition above is self-consistent — the other two admit configurations where a
//! reduce-only order provably opens unless eviction can run MID-WALK, which is the one thing five
//! rounds never observed (all five are taker-side). This module implements the prefix condition,
//! which is a strict subset of both looser readings: it only ever rejects MORE, never less.
//!
//! ⚠️ **That is a real, visible deviation, not a rounding difference.** A maker with a ladder of
//! same-side normal orders who then wants a reduce-only order behind them is refused here and (on
//! the looser readings) accepted by Binance. §1.6's three-way probe settles which reading is
//! Binance's; the design note records the decision to take the safe one meanwhile.
//!
//! # Silent truncation
//!
//! An over-sized request is **accepted and quietly shrunk** to what fits, with no error — measured
//! five times across three rounds, two margin modes and three symbols, for both limit and market
//! orders. So `quantity` is a REQUEST, not a promise: the accepted size is what
//! `OrderPlaced.quantity` carries. (`OrderRested` is not a substitute — it only fires when a
//! remainder rests, so an IOC or a fully-filled order produces none. R17 A13's truncation was an
//! IOC.)

use primitives::Address;

use crate::errors::perp_err;
use crate::host::PerpHost;
use crate::storage;
use crate::types::{OrderEntry, PerpPosition, Side};
use crate::PerpError;

/// Distance from the touch, as a sortable key: smaller = nearer = fills sooner.
///
/// A sell is nearer the touch the LOWER its price; a buy the HIGHER. Folding both into one
/// monotone key is what lets a single comparator serve the admission prefix, the "orders ahead of
/// me" term, and the farthest-first eviction order — three things that would otherwise each carry
/// their own side-conditional and could drift apart. (An engine that hard-codes "highest price
/// first" gets the short side backwards; §1.6 flags exactly that mistake.)
#[inline]
pub fn distance_from_touch(side: Side, price: u64) -> u64 {
    match side {
        Side::Sell => price,
        Side::Buy => u64::MAX - price,
    }
}

/// The two terms of the prefix condition over one `(user, market, side)` entry list.
///
/// Returns `(normals_ahead, sigma_reduce_only)`:
///
/// * `sigma_reduce_only` — Σ remaining quantity over every reduce-only entry in the list;
/// * `normals_ahead` — Σ remaining quantity over the NORMAL entries at or nearer the touch than
///   the farthest reduce-only entry. Zero when the list holds no reduce-only entry, which is the
///   overwhelmingly common case and the reason the caller's fast path can skip this walk entirely.
///
/// `cut` is the distance of the farthest reduce-only entry, optionally widened by a hypothetical
/// new entry at `extra` — that is how admission prices an order that would itself become the
/// farthest.
///
/// ⚠️ A normal entry at EXACTLY `cut` (same price as the farthest reduce-only order) is counted as
/// ahead. Whether Binance counts same-price as ahead, and whether it breaks the tie by time, is
/// unmeasured (every R20 rung used strictly distinct prices). Counting it is the conservative
/// choice — it can only reject more — and it is OUR choice, not an observed behaviour.
pub fn prefix_terms<'a, I>(entries: I, side: Side, extra: Option<u64>) -> (u64, u64)
where
    I: IntoIterator<Item = &'a OrderEntry> + Clone,
{
    let mut sigma_ro: u64 = 0;
    let mut cut: Option<u64> = extra.map(|p| distance_from_touch(side, p));
    for e in entries.clone() {
        if e.reduce_only {
            sigma_ro = sigma_ro.saturating_add(e.amount);
            let d = distance_from_touch(side, e.price);
            cut = Some(cut.map_or(d, |c| c.max(d)));
        }
    }
    let Some(cut) = cut else {
        return (0, 0);
    };
    let normals_ahead = entries
        .into_iter()
        .filter(|e| !e.reduce_only && distance_from_touch(side, e.price) <= cut)
        .fold(0u64, |acc, e| acc.saturating_add(e.amount));
    (normals_ahead, sigma_ro)
}

/// How much room the prefix condition leaves for a NEW reduce-only order at `price`.
///
/// `|position| − normals_ahead − Σ reduce-only`, saturating at zero. Saturating rather than
/// checked on purpose: between a fill that shrinks the position and the eviction that restores the
/// condition, the terms can legitimately exceed `|position|` for an instant, and the honest answer
/// then is "no room", not an invariant panic.
fn capacity_for_new<'a, I>(pos: &PerpPosition, entries: I, side: Side, price: u64) -> u64
where
    I: IntoIterator<Item = &'a OrderEntry> + Clone,
{
    let (normals_ahead, sigma_ro) = prefix_terms(entries, side, Some(price));
    pos.amount
        .unsigned_abs()
        .saturating_sub(normals_ahead)
        .saturating_sub(sigma_ro)
}

/// The side a reduce-only order must be on to reduce `pos`: the opposite of the position's sign.
/// `None` for a flat position — nothing to reduce, so no side qualifies.
#[inline]
pub fn closing_side(pos: &PerpPosition) -> Option<Side> {
    match pos.amount {
        n if n > 0 => Some(Side::Sell),
        n if n < 0 => Some(Side::Buy),
        _ => None,
    }
}

/// Where a new order sits in the owner's own fill queue — the only thing the prefix condition
/// needs to know about it, and a type rather than a `u64` so the market-order case cannot be
/// spelled wrong.
///
/// It used to be a bare `price`, and a market order's price is IGNORED by `placeOrder` (callers
/// send 0). On the BUY side `distance_from_touch(Buy, 0)` is `u64::MAX` — the FARTHEST possible —
/// so a market reduce-only buy was ranked behind every resting buy the owner had and refused. See
/// [`crate::types::OrderKind::has_resting_queue_position`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuePosition {
    /// A GTC or PostOnly order: its limit price IS its place in the queue.
    Resting { price: u64 },
    /// A market, IOC or FOK order: fills immediately, so nothing the owner has resting can precede
    /// it, and it never joins the resting set itself.
    Immediate,
}

impl QueuePosition {
    /// The queue position of an order of `kind` at `price`. Derived from the ORDER KIND, never
    /// from the price alone: a market / IOC / FOK order has no queue position, and a market
    /// order's price is not even set.
    ///
    /// The one derivation for both callers of [`closing_capacity`] — `placeOrder` admission and a
    /// fired conditional order — so the two cannot rank the same order differently.
    #[inline]
    pub fn for_kind(kind: crate::types::OrderKind, price: u64) -> Self {
        if kind.has_resting_queue_position() {
            QueuePosition::Resting { price }
        } else {
            QueuePosition::Immediate
        }
    }
}

/// How much a reduce-only order on the position's CLOSING side may close — the one capacity rule,
/// shared by [`admit`] and by a fired conditional order (`algo::fire`). The caller has already
/// established that `side` is the closing side of `pos`.
///
/// # An immediate order is bounded by the POSITION and by nothing else
///
/// It fills before every resting order, so no `normals_ahead` term applies. `Σ reduce-only` does
/// not apply either: those orders fill LATER, and the post-fill eviction pass ([`restore`]) trims
/// them against whatever position is left — Binance `er=4`, a reduce-only market order crowding
/// out the resting reduce-only orders behind it. Subtracting them here would refuse a close on
/// account of orders that cannot precede it — the same class of defect as the priority bug, just
/// milder.
///
/// It also skips the list load entirely, which is the cheap path for the common close.
///
/// # Why the trigger path shares it rather than keeping its own copy
///
/// `fire` used to compute this itself, with the prefix condition for EVERY fired order, priced at
/// the mark for a market one. A stop-loss next to a full-size resting reduce-only take-profit
/// therefore found no room and expired instead of closing: long 10 with a reduce-only TP sell 10
/// resting, a closePosition stop closed NOTHING; with the TP at 5 it closed only 5 — the standard
/// TP + SL bracket was broken. One function for both callers is what keeps the two from drifting
/// apart again.
pub(crate) fn closing_capacity<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    side: Side,
    pos: &PerpPosition,
    queue: QueuePosition,
) -> Result<u64, PerpError> {
    Ok(match queue {
        QueuePosition::Immediate => pos.amount.unsigned_abs(),
        QueuePosition::Resting { price } => {
            let entries = side_entries_ref(context, user, market_id, side)?;
            capacity_for_new(pos, entries.iter(), side, price)
        }
    })
}

/// Admits a reduce-only order, returning the quantity that is actually accepted.
///
/// Rejects a flat position and a same-side (would-open) request outright; otherwise TRUNCATES the
/// request to the prefix condition's capacity, floored to `step_size`, and rejects only when that
/// leaves nothing.
///
/// ⚠️ Truncating to a `step_size` multiple is OUR choice — the measured truncations all happened to
/// land on the grid, so Binance's behaviour off-grid is unobserved. Flooring keeps the book on-tick
/// and can only shrink the result, so it cannot break the invariant.
pub fn admit<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    side: Side,
    queue: QueuePosition,
    request: u64,
    step_size: u64,
) -> Result<u64, PerpError> {
    let pos = storage::load_position_ref(context, user, market_id)?;

    let Some(closing) = closing_side(&pos) else {
        return Err(perp_err(
            "placeOrder: reduce-only requires an open position",
        ));
    };
    if side != closing {
        return Err(perp_err(
            "placeOrder: reduce-only order is on the position's opening side",
        ));
    }

    // An immediate order is bounded by the POSITION and by nothing else; a resting one by the
    // prefix condition. See `closing_capacity`, which a fired conditional order shares.
    let capacity = closing_capacity(context, user, market_id, side, &pos, queue)?;

    let mut accepted = request.min(capacity);
    if step_size > 0 {
        accepted -= accepted % step_size;
    }
    if accepted == 0 {
        let (normals_ahead, sigma_ro) = match queue {
            QueuePosition::Immediate => (0, 0),
            QueuePosition::Resting { price } => {
                let entries = load_side_entries(context, user, market_id, side)?;
                prefix_terms(&entries, side, Some(price))
            }
        };
        // One message carrying all three terms rather than a separate string per cause: Binance
        // collapses every reduce-only refusal into one code with identical text anyway, and the
        // numbers are the part that is actually diagnosable from a receipt.
        return Err(perp_err(format!(
            "placeOrder: reduce-only capacity exhausted (position {}, ahead {normals_ahead}, \
             resting reduce-only {sigma_ro}, step {step_size})",
            pos.amount.unsigned_abs()
        )));
    }
    Ok(accepted)
}

/// Restores the prefix condition on one `(user, market, side)`, evicting reduce-only orders
/// FARTHEST-FROM-THE-TOUCH FIRST until it holds — and no further.
///
/// Call it after anything that can break the condition. There are exactly three such things, and
/// they are the same predicate evaluated at three moments rather than three rules:
///
/// ```text
/// after a fill / ADL          |position| shrank or flipped      ReduceOnlyPositionShrank
///                                                               ReduceOnlyWouldOpen
/// after a NORMAL order rests  the "ahead" term grew             ReduceOnlyOvertaken
/// at reduce-only admission    Σ reduce-only would grow          (refused instead — see `admit`)
/// ```
///
/// # Why farthest-first, and what that claim is worth
///
/// Evicting the farthest is the only choice that can also drop NORMAL orders out of the counted
/// prefix — everything between the old farthest and the new one stops being "ahead of a
/// reduce-only order" — so it makes progress no smaller than removing any other single entry of
/// the same size. It is also what R20 measured, using two configurations whose intersection
/// excludes both `H_largest` and `H_smart`.
///
/// ⚠️ It is NOT derived: a larger non-farthest order can reduce the total by more (that is
/// `H_largest`, which R20 excluded by observation, not by argument). Farthest-first is *consistent
/// with* the condition and *measured*; do not present it as a theorem.
///
/// # Mid-walk eviction is deliberately NOT a thing
///
/// A tempting reading of this is "re-check after every fill inside the match walk". It is not
/// needed, and the reason is the whole point of the prefix condition: admission guarantees that
/// everything nearer the touch than a reduce-only order, PLUS that order's own quantity, fits
/// inside `|position|`. Orders fill in distance order, so when the walk reaches a reduce-only
/// order the position still covers it in full. This pass only has to put the book back in shape
/// for the NEXT transaction. The `fill_opening_qty == 0` watchdog is what would catch that
/// reasoning being wrong.
///
/// # Cost
///
/// At most [`MAX_REDUCE_ONLY_ORDERS_PER_MARKET`] evictions per call — the side cannot hold more
/// reduce-only orders than that — each re-reading a side list of at most `MAX_OPEN_ORDERS_PER_MARKET`
/// (200) entries. The quadratic shape is deliberate at that size: it keeps the decision in one pure
/// function ([`next_eviction`]) that the admission gates replay.
pub fn restore<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &crate::types::Market,
    side: Side,
    reason: crate::types::CancelReason,
) -> Result<(), PerpError> {
    let market_id = market.market_id;
    loop {
        let entries = load_side_entries(context, user, market_id, side)?;
        // Fast exit, and the reason this is affordable on the fill path: no reduce-only entry on
        // this side means nothing to check. One scan of a list the caller already had warm.
        if !entries.iter().any(|e| e.reduce_only) {
            return Ok(());
        }

        let pos = storage::load_position_ref(context, user, market_id)?;
        let Some(eviction) = next_eviction(&pos, &entries, side) else {
            return Ok(());
        };
        // A side that is no longer the closing side is reported as such whatever the caller's
        // trigger was: "the position flipped or went flat" is the true cause, and it is sufficient
        // on its own — R19 ② separated it from the size condition by flipping a position while
        // `|position|` still exceeded the resting quantity, and the order died anyway.
        let reason = if eviction.side_is_closing {
            reason
        } else {
            crate::types::CancelReason::ReduceOnlyWouldOpen
        };
        let victim = eviction.entry.order_id;

        let order = storage::load_order(context, &victim)?.ok_or_else(|| {
            crate::errors::perp_invariant_err(
                "reduce-only eviction: entry has no order record",
            )
        })?;
        crate::trading::execute_order_cancellation(
            context,
            user,
            market_id,
            victim,
            order,
            market,
            crate::trading::remove_from_book_after_cancel,
            reason,
        )?;
    }
}

/// The order [`restore`] evicts next from one side — see [`next_eviction`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Eviction {
    /// The victim: the reduce-only entry FARTHEST from the touch.
    pub(crate) entry: OrderEntry,
    /// `false` = the side is no longer the position's closing side (it flipped or went flat), which
    /// is what the eviction is then reported as, whatever triggered the pass.
    pub(crate) side_is_closing: bool,
}

/// [`restore`]'s DECISION, pure: the reduce-only entry it evicts next from `side`'s `entries`
/// against `pos`, or `None` once the prefix condition holds there.
///
/// One function for the real pass and for every place that has to know its outcome BEFORE it
/// runs. The admission gates are those places: the order that triggers a pass (a fill that shrinks,
/// flips or closes the taker's own position; a normal order that overtakes the owner's reduce-only
/// orders) is decided before the pass evicts anything, and the orders it is about to evict carry
/// no margin once they are gone — so the gate must be measured without them, which means
/// replaying exactly this decision on the gate's copies (`trading::replay_reduce_only_restore`).
/// Two copies of the rule would let the gate and the pass disagree about which orders survive.
///
/// Only `pos.amount` is read; the aggregates play no part in the prefix condition.
pub(crate) fn next_eviction<'a, I>(pos: &PerpPosition, entries: I, side: Side) -> Option<Eviction>
where
    I: IntoIterator<Item = &'a OrderEntry> + Clone,
{
    if !entries.clone().into_iter().any(|e| e.reduce_only) {
        return None;
    }
    let side_is_closing = closing_side(pos) == Some(side);
    let (normals_ahead, sigma_ro) = prefix_terms(entries.clone(), side, None);
    if side_is_closing && normals_ahead.saturating_add(sigma_ro) <= pos.amount.unsigned_abs() {
        return None;
    }
    // The farthest reduce-only entry. `min_by_key` over the REVERSED distance returns the FIRST of
    // the farthest, so equal-distance (same-price) entries are evicted in list order — which is
    // insertion order, i.e. FIFO, taking the earliest first. §1.6 records FIFO and
    // orderId-ascending as indistinguishable in the evidence; FIFO is the one that can be explained
    // to a user, and our order ids are `keccak(account ‖ nonce)`, so ascending-id would be
    // pseudorandom.
    let entry = entries
        .into_iter()
        .filter(|e| e.reduce_only)
        .min_by_key(|e| std::cmp::Reverse(distance_from_touch(side, e.price)))
        .copied()?;
    Some(Eviction {
        entry,
        side_is_closing,
    })
}

/// [`restore`] on BOTH sides.
///
/// After a fill the position may have FLIPPED, which moves the closing side — so reduce-only orders
/// that were legitimately on the old closing side are now on the opening side. Checking only the
/// current closing side would leave exactly those behind.
pub fn restore_both_sides<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &crate::types::Market,
    reason: crate::types::CancelReason,
) -> Result<(), PerpError> {
    restore(context, user, market, Side::Buy, reason)?;
    restore(context, user, market, Side::Sell, reason)
}

/// How many reduce-only orders one account may hold RESTING in one market — counting, in the same
/// budget, its untriggered conditional orders whose child would rest (owner decision 2026-11-17).
///
/// # Why it exists
///
/// [`restore`] runs after every fill, for every maker the walk touched, and evicts that maker's
/// reduce-only orders farthest-first until the prefix condition holds. Before this cap the number it
/// could evict was bounded only by the open-order cap (`MAX_OPEN_ORDERS_PER_MARKET` = 200), so one
/// taker walk under the 64-touch cap could cancel 64 × 199 = 12,736 orders in one flat-gas call (one
/// in-band fill per maker flattens it, and every off-band reduce-only order of its own goes). With
/// this cap a touched maker loses at most 10, so one walk evicts at most 64 × 10 = 640, and a
/// liquidation walk at most `MAX_LIQUIDATION_MAKER_ACCOUNTS` × 10 = 1,280. Per eviction `restore`
/// re-reads the side list, so the pass is O(k · n) with k ≤ 10 evictions and n ≤ 200 entries —
/// no longer worth restructuring.
///
/// # What counts
///
/// `slots = (reduce-only entries resting on BOTH sides of the book)
///        + (untriggered conditionals whose child RESTS: a limit pair with GTC / PostOnly tif)`
///
/// Every conditional is reduce-only or closePosition (`algo::validate`), so its child is reduce-only.
/// A market-type conditional (STOP_MARKET / TAKE_PROFIT_MARKET / TRAILING_STOP_MARKET) or a limit one
/// with IOC / FOK tif never rests, so it does not count; neither does a placement that fills or
/// expires in full.
///
/// # Where it is enforced
///
/// * a SUBMITTED reduce-only order (`placeOrder`, `placeOrderSigned`, a batch item, any of them via
///   `asSubAccount`) — at the moment its remainder would REST (after the match is decided, before it
///   is written), so an order that fills completely, an IOC / FOK / market order, and a GTC whose
///   remainder expires (touch cap, STP) are never refused. Refused write-clean with
///   [`REDUCE_ONLY_CAP_REJECT`] when `slots >= 10`;
/// * `placeAlgoOrder` / `placeAlgoOrderSigned` of a limit conditional with GTC / PostOnly tif —
///   [`ALGO_REDUCE_ONLY_CAP_REJECT`] when `slots >= 10`;
/// * a TRIGGERED child is never refused (a stop that does nothing is worse than a slow one): it
///   needs no check, because its conditional already held its slot. `fire` takes the conditional off
///   the owner's list before placing the child, so a firing trades one counted order for at most one
///   other (a re-armed conditional returns to the list, and its child never rested). Hence
///   `slots <= 10` is an INVARIANT for every account and market, which a debug assertion in the
///   triggered path checks.
///
/// The count is read from the store BEFORE the match is written. A walk that evicts some of the
/// placer's own reduce-only orders (its fill shrinks its position) frees those slots only after this
/// check, so at the boundary such an order is refused although its rest would have fitted. That is
/// conservative and rare (the order has to cross, and shrink the position past the prefix of the
/// orders it is about to join), so it is not worth replaying the eviction for.
///
/// No new event, batch reason (`Other`, 255) or storage key: both lists are already per-account.
pub const MAX_REDUCE_ONLY_ORDERS_PER_MARKET: usize = 10;

/// A submitted reduce-only order whose remainder would rest past [`MAX_REDUCE_ONLY_ORDERS_PER_MARKET`].
/// Not in the batch reason table (reason `Other`). The number is pinned to the constant by test.
pub const REDUCE_ONLY_CAP_REJECT: &str =
    "placeOrder: too many reduce-only orders (at most 10 resting per market); cancel one first";

/// [`REDUCE_ONLY_CAP_REJECT`] for a `placeAlgoOrder` whose child would rest.
pub const ALGO_REDUCE_ONLY_CAP_REJECT: &str =
    "placeAlgoOrder: too many reduce-only orders (at most 10 resting per market); cancel one first";

/// The slots `user` holds in `market_id` under [`MAX_REDUCE_ONLY_ORDERS_PER_MARKET`], or `None`
/// when the cheap bound already shows fewer than the cap are held (the caller only needs to know
/// "under the cap").
///
/// The conditional records are loaded only when it could matter: book reduce-only entries plus the
/// WHOLE conditional list (≤ `MAX_ALGO_ORDERS_PER_MARKET` ids) below the cap means the exact count
/// is below it too. Read-only (`_ref` lists, cache-fill), so it enters nothing into the block delta.
fn reduce_only_slots_if_near_cap<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<Option<usize>, PerpError> {
    let book = side_entries_ref(context, user, market_id, Side::Buy)?
        .iter()
        .filter(|e| e.reduce_only)
        .count()
        + side_entries_ref(context, user, market_id, Side::Sell)?
            .iter()
            .filter(|e| e.reduce_only)
            .count();
    if book >= MAX_REDUCE_ONLY_ORDERS_PER_MARKET {
        return Ok(Some(book));
    }
    let algo_ids = storage::load_user_algo_orders(context, user, market_id)?;
    if book + algo_ids.len() < MAX_REDUCE_ONLY_ORDERS_PER_MARKET {
        return Ok(None);
    }
    let mut slots = book;
    for id in &algo_ids {
        let rests = storage::load_order(context, id)?.is_some_and(|o| {
            o.order_type == crate::types::OrderType::Limit
                && crate::types::OrderKind::Limit(o.tif).has_resting_queue_position()
        });
        if rests {
            slots += 1;
        }
    }
    Ok(Some(slots))
}

/// Refuses with `reject` when `user` already holds [`MAX_REDUCE_ONLY_ORDERS_PER_MARKET`] slots in
/// `market_id`. Pure read: a refusal is a genuine write-clean reject.
pub(crate) fn ensure_reduce_only_room<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    reject: &'static str,
) -> Result<(), PerpError> {
    match reduce_only_slots_if_near_cap(context, user, market_id)? {
        Some(slots) if slots >= MAX_REDUCE_ONLY_ORDERS_PER_MARKET => Err(perp_err(reject)),
        _ => Ok(()),
    }
}

/// Debug-only watchdog for the triggered path: a fired child about to rest must find a free slot,
/// because its conditional held one and has just given it up (see
/// [`MAX_REDUCE_ONLY_ORDERS_PER_MARKET`]). A failure means some path added a reduce-only order or
/// a resting-kind conditional without [`ensure_reduce_only_room`].
#[cfg(debug_assertions)]
pub(crate) fn debug_assert_triggered_rest_has_room<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<(), PerpError> {
    let slots = reduce_only_slots_if_near_cap(context, user, market_id)?.unwrap_or(0);
    debug_assert!(
        slots < MAX_REDUCE_ONLY_ORDERS_PER_MARKET,
        "a triggered reduce-only child found {user} at the reduce-only cap ({slots}) in market \
         {market_id}: some path admitted one without the cap check"
    );
    Ok(())
}

/// Snapshot of one side's entry list. Read through the `_ref` loader: cache-fill only, never
/// dirty-marking, so evaluating the predicate enters no key into the block delta.
fn load_side_entries<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    side: Side,
) -> Result<std::collections::VecDeque<OrderEntry>, PerpError> {
    Ok((*side_entries_ref(context, user, market_id, side)?).clone())
}

/// [`load_side_entries`] without the copy, for a caller that only reads the list.
fn side_entries_ref<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    side: Side,
) -> Result<std::sync::Arc<std::collections::VecDeque<OrderEntry>>, PerpError> {
    match side {
        Side::Buy => storage::load_buy_orders_ref(context, user, market_id),
        Side::Sell => storage::load_sell_orders_ref(context, user, market_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(price: u64, amount: u64, reduce_only: bool) -> OrderEntry {
        OrderEntry {
            order_id: [0u8; 32],
            price,
            amount,
            maker_fee_ppm: 0,
            assuming_price: price,
            reduce_only,
            stp: Default::default(),
        }
    }

    fn pos(amount: i64) -> PerpPosition {
        PerpPosition {
            amount,
            ..Default::default()
        }
    }

    fn terms(entries: &[OrderEntry], side: Side, extra: Option<u64>) -> (u64, u64) {
        prefix_terms(entries, side, extra)
    }

    fn cap(p: &PerpPosition, entries: &[OrderEntry], side: Side, price: u64) -> u64 {
        capacity_for_new(p, entries, side, price)
    }

    /// A list with no reduce-only entry constrains nothing, and — the part that matters for the hot
    /// path — the walk reports zero for BOTH terms rather than summing the normal orders.
    #[test]
    fn a_list_without_reduce_only_orders_is_unconstrained() {
        let e = [entry(100, 5, false), entry(101, 7, false)];
        assert_eq!(terms(&e, Side::Sell, None), (0, 0));
    }

    /// Distance folds both sides into one monotone key. A sell is nearer the touch the lower its
    /// price, a buy the higher — an engine that hard-codes "highest price is farthest" gets the
    /// short side backwards.
    #[test]
    fn distance_orders_each_side_from_its_own_touch() {
        assert!(distance_from_touch(Side::Sell, 100) < distance_from_touch(Side::Sell, 101));
        assert!(distance_from_touch(Side::Buy, 101) < distance_from_touch(Side::Buy, 100));
    }

    /// Only the normal orders AT OR NEARER the farthest reduce-only order count. One beyond it
    /// fills after, so it cannot consume the position first.
    #[test]
    fn normal_orders_beyond_the_farthest_reduce_only_do_not_count() {
        let e = [
            entry(100, 2, false), // nearer  -> counts
            entry(101, 3, true),  // the farthest reduce-only
            entry(102, 9, false), // beyond  -> does not count
        ];
        assert_eq!(terms(&e, Side::Sell, None), (2, 3));
    }

    /// The buy side is the mirror image, and gets it right only because `distance_from_touch`
    /// inverts the price.
    #[test]
    fn the_buy_side_counts_from_the_high_side() {
        let e = [
            entry(102, 2, false), // nearer the touch for a BUY -> counts
            entry(101, 3, true),  // farthest reduce-only
            entry(100, 9, false), // beyond -> does not count
        ];
        assert_eq!(terms(&e, Side::Buy, None), (2, 3));
    }

    /// ⚠️ OUR choice, not a measured behaviour: a normal order at exactly the farthest reduce-only
    /// order's price is counted as ahead. Same-price tie-breaking is unobserved, and counting is
    /// the direction that can only reject more.
    #[test]
    fn a_normal_order_at_the_same_price_counts_as_ahead() {
        let e = [entry(101, 4, false), entry(101, 3, true)];
        assert_eq!(terms(&e, Side::Sell, None), (4, 3));
    }

    /// `extra` widens the cut when the hypothetical new order would itself be the farthest — that
    /// is how admission prices an order placed behind existing normal orders.
    #[test]
    fn a_new_order_behind_everything_widens_the_cut() {
        let e = [entry(100, 2, false), entry(101, 3, false)];
        // No reduce-only order yet, so without `extra` there is no cut at all…
        assert_eq!(terms(&e, Side::Sell, None), (0, 0));
        // …and with a hypothetical reduce-only order at 102, both normals are ahead of it.
        assert_eq!(terms(&e, Side::Sell, Some(102)), (5, 0));
    }

    /// The reason nearer reduce-only orders have to be counted. Two full-size reduce-only orders
    /// and not a single normal order: a predicate that only looked at "normal orders ahead" admits
    /// both, and the second one to fill opens the position the other way.
    #[test]
    fn two_full_size_reduce_only_orders_cannot_both_be_admitted() {
        let p = pos(10);
        assert_eq!(cap(&p, &[], Side::Sell, 100), 10);
        let after_first = [entry(100, 10, true)];
        assert_eq!(
            cap(&p, &after_first, Side::Sell, 101),
            0,
            "the first one already commits the whole position"
        );
    }

    /// The configuration that refutes the "one merged budget" reading of §1.6: it checks only the
    /// NEW order, so `Σ reduce-only` can grow behind an older one that was admitted when the
    /// budget was still free. The prefix condition sees it because it re-derives both terms.
    #[test]
    fn a_nearer_reduce_only_order_cannot_starve_a_farther_one_retroactively() {
        let p = pos(10);
        // A normal 8u sits near the touch; a reduce-only 2u goes behind it. 8 + 2 = 10 ✓
        let mut book = vec![entry(100, 8, false)];
        assert_eq!(cap(&p, &book, Side::Sell, 101), 2);
        book.push(entry(101, 2, true));
        // Now a reduce-only order IN FRONT of the normal order: the merged-budget reading would
        // allow 8u here (10 − 2 − 0). The prefix condition allows nothing — the farthest
        // reduce-only order is still the 2u at 101, so the 8u normal is ahead of it and the whole
        // position is already committed.
        assert_eq!(cap(&p, &book, Side::Sell, 99), 0);
    }

    #[test]
    fn a_flat_position_has_no_closing_side() {
        assert_eq!(closing_side(&pos(0)), None);
        assert_eq!(closing_side(&pos(5)), Some(Side::Sell));
        assert_eq!(closing_side(&pos(-5)), Some(Side::Buy));
    }

    /// Saturating, not checked: between a fill that shrinks the position and the eviction that
    /// restores the condition the terms can exceed `|position|`, and "no room" is the honest
    /// answer there rather than a panic.
    #[test]
    fn an_over_committed_list_reports_no_capacity_instead_of_panicking() {
        let p = pos(1);
        let book = [entry(100, 50, false), entry(101, 50, true)];
        assert_eq!(cap(&p, &book, Side::Sell, 102), 0);
    }
}
