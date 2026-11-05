use super::*;

// ── Resting in book ───────────────────────────────────────────────────────────

/// Place a limit order that did not (fully) match into the order book and lock margin.
///
/// The margin model uses `max(buy_reserved, sell_reserved)` so only the dominant side
/// actually locks capital.  Adding an order on the weaker side only increases the wallet
/// deduction when it surpasses the other side's reservation.
/// #A oracle (debug builds only): assert the maintained per-side aggregates on `pos` equal a fresh
/// recompute from the actual order lists. A divergence means a mutation site failed to keep the
/// totals in sync — caught loudly in tests / the correctness gate, compiled out in release.
///
/// The fold is `math::sum_side_totals`, which values each entry at its FROZEN
/// `assuming_price` — the same basis every maintenance site adds and subtracts. Folding at the LIMIT
/// price here would fire spuriously on any sell that was marked up at placement.
#[inline]
pub(super) fn debug_assert_totals(
    buy: impl Iterator<Item = OrderEntry>,
    sell: impl Iterator<Item = OrderEntry>,
    pos: &crate::types::PerpPosition,
    base_decimals: u32,
    price_decimals: u32,
) {
    #[cfg(debug_assertions)]
    {
        let (bq, bn) = crate::math::sum_side_totals(buy, base_decimals, price_decimals)
            .expect("sum buy totals");
        let (sq, sn) = crate::math::sum_side_totals(sell, base_decimals, price_decimals)
            .expect("sum sell totals");
        debug_assert_eq!(
            (bq, bn, sq, sn),
            (
                pos.total_buy_qty,
                pos.total_buy_notional,
                pos.total_sell_qty,
                pos.total_sell_notional
            ),
            "#A maintained reservation totals diverged from the order lists"
        );
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = (buy, sell, pos, base_decimals, price_decimals);
    }
}

/// One of PHASE 2's four aggregate adds — the mechanical restatement of a sum PHASE 1 already
/// checked, on the same two integers.
///
/// `perp_invariant_err` and not `perp_err`: this runs BEHIND the write barrier, so reaching it at
/// all is the write-then-error shape commit-only #23 forbids. It is out of reach by construction —
/// PHASE 1 performed the identical `checked_add` on the identical values (the `debug_assert_eq!`
/// against the basis is what holds "identical" to account), so an overflow here means the basis
/// diverged from the flushed state, which is a BUG and not a user's arithmetic.
#[inline]
fn add_decided_aggregate(current: u64, addend: u64, field: &str) -> Result<u64, PerpError> {
    current.checked_add(addend).ok_or_else(|| {
        perp_invariant_err(format!(
            "rest apply: total {field} overflowed a sum the decision phase had already checked"
        ))
    })
}

/// Consuming a Deque-or-slice as an OrderEntry iterator for the reservation calls / oracle.
#[inline]
pub(super) fn entries_iter(
    v: &std::collections::VecDeque<OrderEntry>,
) -> impl Iterator<Item = OrderEntry> + Clone + '_ {
    v.iter().copied()
}

/// Whether `rest_in_book` actually rested the order, or refused it as a too-good new best.
///
/// Both refusals are decided in PHASE 1, above the write barrier, so both are clean whole-order
/// rejects for every caller ([`raise_rest_refusal`]). Returning them rather than raising them in
/// place is still the right shape: it keeps the two refusal points inside the arms from having to
/// know whether a barrier is pending, and it keeps the funnel single.
///
/// ⚠️ A new variant must be decided in PHASE 1 too. Returning one from PHASE 2 would put a reject
/// behind the barrier and re-open the val0 leak.
#[must_use]
pub(super) enum RestOutcome {
    Rested,
    RefusedTooGoodBest,
    /// The rest's marginal `ooIM` (plus, for a cross order, its Open Loss) does not fit in the
    /// available balance.
    ///
    /// This used to be raised in place (two `return Err`s, one per side arm) while a crossing GTC's
    /// fills were already flushed, which was the val0 leak at block 1,098,719. It was then made a
    /// remainder EXPIRY, which stopped the leak but broke the order-lifecycle contract (an
    /// `OrderCancelled` with no `OrderRested` before it) and diverged from Binance, which rejects
    /// the whole order (`-2019`). Now the barrier sits below the decision, so it is simply a reject.
    RefusedUnaffordable {
        /// The order's position is CROSSED in force: refused with the `-2019` string.
        cross: bool,
    },
    /// The order can open, its account holds cross exposure, and `E − OpenLoss < Σ_C MM` — J1 at
    /// rest (R2). Raised with the taker J1's string (`CROSS_MAINTENANCE_REJECT`, `-2019`).
    RefusedUnderCrossMaintenance,
}

pub(super) const TOO_GOOD_BEST_REJECT: &str =
    "placeOrder: a new best quote must be inside the price band";
/// The rest-margin refusal. Also the reject the pre-walk early-out raises, deliberately: it is the
/// same cause, so a client cannot tell "refused before the walk" from "refused after it".
pub(super) const INSUFFICIENT_MARGIN_REJECT: &str =
    "placeOrder: insufficient perp wallet for margin";
/// [`INSUFFICIENT_MARGIN_REJECT`] for an order on a position CROSSED in force — the same cause, the
/// available balance `A` not covering the order's cost (a cross open's initial margin at the
/// fills gate; `Δ ooIM` plus its Open Loss at the rest gate; the pre-walk bound) — with Binance's
/// code, `-2019`, like the two other cross margin refusals of the place path (J1, J6). An
/// ISOLATED order keeps the uncoded string byte for byte. Batch reason `InsufficientMargin` (29),
/// like its isolated twin.
///
/// ABI CHANGE (cross P2): a new revert string, reachable only on a position CROSSED in force.
pub(crate) const CROSS_INSUFFICIENT_MARGIN_REJECT: &str =
    "placeOrder: -2019 Margin is insufficient. The available balance does not cover the order";

/// The place path's insufficient-margin refusal for an order on a position CROSSED in force
/// (`cross`) or not.
pub(super) fn insufficient_margin_reject(cross: bool) -> &'static str {
    if cross {
        CROSS_INSUFFICIENT_MARGIN_REJECT
    } else {
        INSUFFICIENT_MARGIN_REJECT
    }
}

/// The quantity of a `qty` order on `side` that can only OPEN: what is left once the position it
/// offsets has been closed — net of the same-side orders already resting against it, which are
/// first in line to close it. `0` for a pure reduce. Conservative by construction: an order that
/// shares the closing room with earlier same-side orders is treated as opening as soon as theirs
/// would fill that room.
fn opening_part_of_order(pos: &crate::types::PerpPosition, side: Side, qty: u64) -> u64 {
    let (offset, resting) = match side {
        Side::Buy if pos.amount < 0 => (pos.amount.unsigned_abs(), pos.total_buy_qty),
        Side::Sell if pos.amount > 0 => (pos.amount.unsigned_abs(), pos.total_sell_qty),
        _ => (0, 0),
    };
    qty.saturating_sub(offset.saturating_sub(resting))
}

/// Binance's **Open Loss** (F8) of a resting order on a position CROSSED in force:
/// `q_open × max(0, price − mark)` for a buy, `q_open × max(0, mark − price)` for a sell, with
/// `q_open` the order's OPENING part ([`opening_part_of_order`]) and `mark` the stored mark at
/// placement. The whole Binance order cost is `IM + Open Loss`; the IM half is the order's `ooIM`
/// (a sell's at `max(T, limit) ≥ mark`, so it carries no open loss of its own).
///
/// `0` for a position ISOLATED in force (its maker fill is held to K9 instead), for a pure reduce
/// (Binance gates no reducing order; a reduce booked at a bad price is the close-through-bankruptcy
/// risk every close carries), for an order on the good side of the mark, and at a mark of `0`.
///
/// It is the cost at PLACEMENT. A mark that later moves through a resting order's price turns it
/// into an open loss the account did not pay for — Binance's own behaviour, and not a lever: no
/// user moves the mark. The liquidation sweep (P3) is the backstop for it.
fn cross_rest_open_loss(
    market: &crate::types::Market,
    pos: &crate::types::PerpPosition,
    side: Side,
    price: u64,
    qty: u64,
) -> Result<u64, PerpError> {
    if crate::margin::margin_type::effective_margin_type(pos) != crate::types::MarginType::Crossed {
        return Ok(0);
    }
    let mark = market.mark_price;
    let loss_per_unit = match side {
        Side::Buy => price.saturating_sub(mark),
        Side::Sell => mark.saturating_sub(price),
    };
    if mark == 0 || loss_per_unit == 0 {
        return Ok(0);
    }
    let q_open = opening_part_of_order(pos, side, qty);
    calc_value(loss_per_unit, q_open, market.base_decimals, market.price_decimals)
}

pub(super) fn rest_in_book<H: PerpHost>(
    context: &mut H,
    user: Address,
    order_id: &[u8; 32],
    market_id: u64,
    side: Side,
    price: u64,
    qty: u64,
    tif: TimeInForce,
    client_order_id: [u8; 16],
    // Threaded rather than re-read: the `OrderEntry` this builds is what the admission predicate and
    // all three eviction triggers walk, so `reduce_only` has to be ON the entry; `stp` rides along
    // so `getOpenOrders` can echo it off the entry, and `OrderRested` echoes both.
    flags: OrderFlags,
    market: &crate::types::Market,
    // 2b resolve-once: (best_bid, best_ask). `Some` = the caller already read the BBO (PostOnly
    // threads its do-not-cross read — no match ran, so it is still current). `None` = read it here.
    bbo: Option<(u64, u64)>,
    // ── THE DEFERRED MATCH, if any ──
    // `Some` = a crossing GTC whose match is DECIDED BUT UNWRITTEN. PHASE 1 below reads its
    // [`settlement::RestBasis`] instead of the store, decides, and only then does PHASE 1's tail
    // flush it. Dropping it (a refusal) discards the whole match, which is exactly what a clean
    // reject wants — see [`MatchOutcome`]. `None` = PostOnly, where no match ran.
    mut deferred: Option<MatchOutcome>,
    // The place path's buffered `OrderPlaced`. On the deferred path `MatchOutcome::apply` flushes it
    // at the barrier (so it precedes the match's own Trade/PositionChanged rows, unchanged); on the
    // PostOnly path the tail below flushes it, before this order's `OrderRested`. Either way it is
    // emitted only once every refusal has passed.
    pending_placed: &mut Option<PendingOrderPlaced>,
) -> Result<RestOutcome, PerpError> {
    // ══════════════════════════════════════════════════════════════════════════════════════════
    // PHASE 1 — DECIDE. Zero storage writes, here or inside `deferred`.
    // ══════════════════════════════════════════════════════════════════════════════════════════
    let (base_decimals, price_decimals) = (market.base_decimals, market.price_decimals);
    // The position + wallet the rest is admitted against. On the deferred path these are the values
    // the barrier is ABOUT to write (post-fill, post-cover, post-debit) rather than the values the
    // store still holds — that is the whole point of `RestBasis`, and it is why this decision needs
    // no second opinion from the settlement side. `None` = the taker was never touched by the match
    // (or there was no match), so the store already IS the post-flush state.
    let basis = deferred.as_mut().and_then(|d| d.rest_basis.take());
    // Resting escrows nothing, so the account is READ-ONLY on this path: the fee rate for the book
    // entry, and (in the no-basis case) the cross wallet for the derived admission gate. An `_ref`
    // read — no owned clone, and no `save_account` at the end, so nothing marks this user for an
    // `AccountBalanceChanged` snapshot (see `storage::mark_account_snapshot_dirty`: a pure placement
    // publishes nothing, by measurement).
    //
    // The FEE RATE is read from the store on BOTH paths, deliberately: it is not a settlement
    // output. Only `setUserFeeRates` moves it, the flush cannot, so the pre-barrier read is the same
    // integer the post-barrier one would give.
    let maker_fee_ppm = storage::load_account_ref(context, user)?.maker_fee_ppm;
    let (pos_basis, wallet, own_side_cancelled, own_side_list) = match basis {
        Some(b) => (b.pos, b.wallet, b.own_side_cancelled, b.own_side_list),
        None => (
            storage::load_position(context, user, market_id)?,
            storage::load_account_ref(context, user)?.perp_wallet_balance,
            false,
            // A reduce-only order overtakes nothing (see the pass at the end of PHASE 2).
            if flags.reduce_only {
                None
            } else {
                stored_side_list_with_reduce_only(context, user, market_id, side)?
            },
        ),
    };
    // ONE BBO resolve for the band gate. On the deferred path this is the PRE-barrier BBO; see
    // `may_become_best` for why that is sound, and PHASE 2 for the post-barrier re-read the two
    // cache writes use.
    let (best_bid, best_ask) = match bbo {
        Some(b) => b,
        None => {
            let hot = storage::load_market_hot(context, market_id)?;
            (hot.best_bid, hot.best_ask)
        }
    };

    // A quote that BECOMES the best must sit inside the fill-time band. Gating on "becomes the BBO"
    // rather than on "is out of band" is what makes this precise instead of blanket: an out-of-band
    // level is harmful only while it is the best — that is when it poisons the PostOnly cross check
    // (which reads the cached best with no band term) and when it feeds the mid-price sample that
    // flows into the mark median. A level sitting behind an in-band one is invisible to both, so
    // deep passive orders keep resting, which the fill-time band deliberately allows.
    //
    // This is the PLACEMENT half of a pair; the capped sweep in `run_update_index_price` is the
    // DRIFT half (a mark move can strand a book that was in band when placed). Neither subsumes
    // the other.
    //
    // It applies to EVERY resting order, not just PostOnly. A crossing GTC stops at the band and
    // rests its remainder at its own limit price, so `best_bid = 200` with the mark at 100 is
    // reachable without ever touching the PostOnly path — and a best_bid that high rejects nearly
    // every PostOnly sell. Gating PostOnly alone would leave that half open.
    //
    // ── Why the gate tests `may_become_best` and PHASE 2 recomputes `becomes_best` ──────────────
    //
    // The gate is a DECISION and must therefore be taken pre-barrier, off the BBO the store holds
    // now. The two cache writes it guards are WRITES and must therefore be taken post-barrier, off
    // the BBO the flush leaves. Those two BBOs can differ, so the flag cannot be shared any more —
    // what is shared instead is an implication: `becomes_best(post) ⟹ may_become_best(pre)`, so the
    // band is never skipped for an order that really does become the best.
    //
    // The pre-BBO is exact on our OWN side except for one narrow case:
    // * the match consumes the OPPOSITE side only, so it cannot move our side's best at all;
    // * `finalize_apply`'s cover loop, and the flush's reduce-only pass, cancel the taker's OWN
    //   same-side orders, which CAN lower a bid / raise an ask and hand the best to this very
    //   order.
    //
    // Hence the `own_side_cancelled` disjunct: when the barrier will really cancel one, treat the
    // order as possibly-best and band-check it. That over-checks (never under-checks) in a corner
    // that is already all but unreachable — a crossing order's remainder sits at or past the touch
    // it just crossed, so it is the new best on its own side anyway.
    let may_become_best = own_side_cancelled
        || match side {
            Side::Buy => best_bid == 0 || price > best_bid,
            Side::Sell => best_ask == 0 || price < best_ask,
        };
    if may_become_best {
        // ONE-SIDED, matching the sweep's scoping and the measured exchange behaviour. The harms are
        // asymmetric: a new best that is too GOOD to be true — an ask below `mark_lower`, a bid above
        // `mark_upper` — both mis-rejects the opposite side's PostOnly orders (the cross check rejects
        // on `best_ask <= price` / `best_bid >= price`, so a too-low ask or too-high bid rejects MORE)
        // and pollutes the mid-price sample. A new best on the FAR side only pollutes the sample, and
        // it is the ordinary "thin book, deep quote" case: a live probe on the reference exchange
        // placed a sell at 1.389x mark and it was accepted, so rejecting it here would diverge for no
        // gain and would break the deep passive orders the fill-time band deliberately allows.
        let (mark_upper, mark_lower) =
            crate::math::mark_band_bounds(market.mark_price, market.price_band_bps);
        let p = price as u128;
        let too_good = match side {
            Side::Sell => p < mark_lower,
            Side::Buy => p > mark_upper,
        };
        if too_good {
            // Write-clean: nothing above this point mutates, and `deferred` is dropped unflushed on
            // the way out. A clean whole-order reject.
            return Ok(RestOutcome::RefusedTooGoodBest);
        }
    }

    // ── THE FREEZE ── The one place a SHORT order's `T` is resolved, ever, and it happens HERE:
    // pre-barrier, i.e. before `save_last_traded_price`. A SHORT order's Assuming Price is
    // `max(T, limit)` with `T = max(ROUND_UP(lastTraded × 1.0015), mark)`, and it is stored on the
    // entry: from here on the order's margin term is `calc_value(assuming_price, amount)` and no
    // later read re-derives it (R12 — 90 frames, the reported value never moved; `H_live` refused by
    // 1939 quanta).
    //
    // ⚠️ THIS IS THE VAL0 BUG'S ROOT, AND ITS FIX. There used to be a second resolution of `T` (the
    // pre-check in `finalize_compute`) with `save_last_traded_price` running BETWEEN the two. A
    // crossing SELL always drags `last_traded` to at least its own limit price, so the second read
    // was always ≥ the first and the pre-check was systematically optimistic — it vouched for a rest
    // the real check then refused, after the fills had committed. With the write deferred past this
    // point there is exactly ONE evaluation, so the divergence is not merely fixed, it has nowhere
    // left to live. Do not add a second call site.
    //
    // Binance additionally ESCROWS the uplift (measured, R10) and releases it at fill, which is what
    // funds a flipping fill's new leg. We hold no escrow bucket at all, so for us the uplift
    // manifests purely as a stricter admission — which still leaves more wallet present at fill
    // time. It is NOT cosmetic: without it the `ooIM = 0` band (`Ask ≈ 2|N|`, a sell that would flip
    // the position) is free AND fillable here, while on Binance the two are mutually exclusive (a
    // sell must sit near the touch to fill, and near the touch the markup bites). See
    // `binance-flip-and-admission.md` §1.6c/§3.4.
    //
    // ⚠️ `assuming_price` is a MARGIN BASIS ONLY. The insert position, the book level, the fill price
    // and the fee all key on `price`.
    let assuming_price = match side {
        // A LONG order's Assuming Price IS its own limit price — MEASURED, no markup on the buy side
        // (R11's `bidNotional == q_B × P_b` digit-for-digit). Setting the field rather than leaving
        // it side-conditional is what lets ONE rule (`Σ calc_value(assuming_price, amount)`) serve
        // both aggregates.
        Side::Buy => price,
        Side::Sell => price.max(crate::margin::view::assuming_price_floor(
            context, market_id, market,
        )?),
    };
    let new_entry = OrderEntry {
        order_id: *order_id,
        price,
        amount: qty,
        maker_fee_ppm,
        assuming_price,
        reduce_only: flags.reduce_only,
        stp: flags.stp,
    };
    // `Bid`/`Ask` grow by this order's notional at its FROZEN Assuming Price — the same
    // per-order-floored `calc_value` term the raw-list fold would contribute, so the aggregates stay
    // exactly Binance's `bidNotional`/`askNotional`.
    let entry_notional = new_entry.margin_notional(base_decimals, price_decimals)?;

    // ── Derived-ooIM admission gate (POST-STATE form, lazy delta) ──
    //
    // commit-only #23 CLONE-FREE probe: the hypothetical "own-side list ⊕ new_entry" is never
    // materialised — the derived requirement reads only the per-side AGGREGATES, so the whole probe
    // is two `checked_add`s. Nothing is debited either: resting an order moves no money, it only
    // raises the requirement. The OTHER side still matters, through the `max()` inside `ooIM`.
    //
    // ONE arm for both sides. The buy and sell paths used to be ~165 lines of verbatim duplication
    // apiece — same probe, same gate, same four-case table, same ⚠️ notes — differing only in WHICH
    // aggregate pair moves and in the Assuming Price resolved above. An asymmetry between them would
    // have been silent (most tests drive one side), so the duplication is gone rather than mirrored.
    //
    // # Mapping this onto the doc's predicate — one algebraic step, and the step is MEASURED
    //
    // The measured admission predicate is the DELTA form,
    // 「接受 ⟺ 该单带来的 IM 增量 ≤ availableBalance」, i.e. `Δ ooIM ≤ available(before)`. What is
    // evaluated below is `available(after) ≥ 0`. R13 measured the identity that connects them —
    // `availableBalance + totalOpenOrderInitialMargin == totalCrossWalletBalance`, 73/73
    // observations, residual `0E-8`, while BOTH terms moved (`misc/binance-flip-and-admission.md`
    // §3.14) — so `available = wallet − Σ_m ooIM_m` and
    //
    //     available(after) = wallet − (Σ_{m ≠ this} ooIM_m + ooIM_this(after))
    //                      = available(before) − Δ ooIM
    //     ⇒   Δ ooIM ≤ available(before)   ⟺   available(after) ≥ 0
    //
    // That identity is exact here, not approximate: `ooIM_m` is a pure function of market `m`'s own
    // `(N_m, Bid_m, Ask_m, L_m)` (`margin_view::position_derived_margin` → `math::open_order_margin`,
    // which takes nothing else), so every `m ≠ this` term is literally the same integer in both sums
    // within one call — there is NO cross-market coupling to make the cancellation lossy. Both sides
    // also price THIS market at the same mark: the threaded `market` and the one the Σ walk re-reads
    // are the same blob, since the place path never writes `Market`.
    //
    // # The case table this is equivalent over
    //
    //   Δ ooIM │ available(after) │ delta form (was)              │ this form
    //   ───────┼──────────────────┼───────────────────────────────┼────────────────────────
    //     ≤ 0  │      ≥ 0         │ accept (`Δ ≤ 0` escape)       │ accept (1st branch)
    //     ≤ 0  │      < 0         │ accept (`Δ ≤ 0` escape)       │ accept (escape kept)
    //     > 0  │      ≥ 0         │ accept (`avail(before) ≥ Δ`)  │ accept (1st branch)
    //     > 0  │      < 0         │ REJECT                        │ REJECT
    //
    // # ⚠️ The `Δ ≤ 0` escape is the B1 INVARIANT — do not delete it
    //
    // Rows 1 and 3 are the whole reason the delta is LAZY: they are the overwhelming majority and
    // they need only `available(after)`. Row 2 is the ONLY row that needs the delta, and only its
    // SIGN. It is **reachable and it matters**: a user rests affordable orders, the mark then moves
    // against them so `Σ ooIM > wallet` and `available < 0` with no action of theirs (R8: an
    // identically-priced probe accepted, then refused 4 seconds later after the mark fell
    // `4.42 USD`), and they now want to rest a RISK-REDUCING order. Its `Δ ooIM ≤ 0`, but
    // `available(after)` is still negative. Dropping the escape would refuse precisely the order
    // that de-risks them and send them to liquidation instead. This is `derived_can_afford`'s "a
    // non-positive requirement is always affordable", restated at the one site that still has to ask.
    //
    // (At THIS site `Δ ooIM` is never negative unless the rest OVERTAKES reduce-only orders —
    // `max(|N + Bid|, |N − Ask|)` is non-decreasing in each of `Bid`/`Ask` at fixed `N`, `PIM` does
    // not move, and `round_up_div`/`saturating_sub` preserve that. An order that evicts reduce-only
    // orders lowers its own side by their terms, and can come out negative: then the escape is
    // exactly B1 — the placement as a whole lowers the requirement.)
    //
    // Cost: in the accepting case this market's `ooIM` is evaluated ONCE (inside the walk, via the
    // override) instead of three times.
    //
    // # Why the LEAN fold — `Σ ooIM` alone, not the ten-scalar roll-up
    //
    // The gate needs one number, `available(after)`, and that is all this walk produces. A previous
    // pass widened it to `margin_view::index_account_scalars` so the placement could publish an
    // `AccountBalanceChanged` off the same walk; that event is gone (a pure placement is deliberately
    // silent — R14 measurement + the official trigger sentence, see
    // `storage::mark_account_snapshot_dirty`), so the widening has no consumer again and the five
    // accumulators it folded would be discarded arithmetic on the hottest path.
    //
    // # The orders this rest OVERTAKES — measured without them
    //
    // The third reduce-only eviction trigger (the pass at the end of PHASE 2): a NORMAL order
    // resting nearer the touch grows the "ahead" term of the owner's reduce-only orders behind it,
    // and the pass evicts as many as the prefix condition needs. It runs AFTER this decision, so
    // the decision replays it here — on the post-barrier list with the new entry inserted where
    // PHASE 2 inserts it — and the gate is measured without the orders it evicts. On Binance a
    // reduce-only order carries no open-order margin and is expired once it can no longer reduce,
    // so the order whose rest kills one is not liable for it. Measured with them, the gate was
    // too strict: short 5 with a reduce-only buy 5 @ $90, a normal buy 16 @ $100 resting ahead of
    // it was priced at `Bid = $2,050` (`ooIM = $1,050` at leverage 1) where `$1,600` is what rests
    // (`ooIM = $600`).
    //
    // `pos_pre` is the basis without them; `None` = nothing evicted (the common case — no
    // reduce-only order on this side, so no list was even cloned). The order's OWN terms (its Open
    // Loss, its opening part) are measured against it; the delta runs from the true before
    // (`pos_basis`) to the true after (`pos_after`).
    let pos_pre = match own_side_list.filter(|_| !flags.reduce_only) {
        Some(mut list) => {
            let i = match side {
                Side::Buy => list.partition_point(|e| e.price > price),
                Side::Sell => list.partition_point(|e| e.price < price),
            };
            list.insert(i, new_entry);
            let mut pre = pos_basis.clone();
            let mut other = std::collections::VecDeque::new();
            let (buy, sell) = match side {
                Side::Buy => (&mut list, &mut other),
                Side::Sell => (&mut other, &mut list),
            };
            replay_reduce_only_restore(&mut pre, buy, sell, side, market)?.then_some(pre)
        }
        None => None,
    };
    let pre = pos_pre.as_ref().unwrap_or(&pos_basis);
    let mut pos_after = pre.clone();
    {
        let (qty_field, notional_field, label) = match side {
            Side::Buy => (
                &mut pos_after.total_buy_qty,
                &mut pos_after.total_buy_notional,
                "buy",
            ),
            Side::Sell => (
                &mut pos_after.total_sell_qty,
                &mut pos_after.total_sell_notional,
                "sell",
            ),
        };
        *qty_field = qty_field
            .checked_add(qty)
            .ok_or_else(|| perp_err(format!("placeOrder: total {label} qty overflow")))?;
        *notional_field = notional_field
            .checked_add(entry_notional)
            .ok_or_else(|| perp_err(format!("placeOrder: total {label} notional overflow")))?;
    }
    // ── A CROSS order's Open Loss (Binance F8: `Cost = IM + Open Loss`) ──
    //
    // `0` for an order on an ISOLATED position (its maker fill is held to K9 on the trial silo,
    // which refuses an open into insolvency) and for the part of an order that only reduces. For
    // the OPENING part of an order on a position CROSSED in force it is what the fill will book
    // against the account's equity the moment it happens, priced against the mark: nothing checks a
    // cross maker fill (owner decision Q9 / D13 — and K9 does not apply to a margin-free position),
    // so it must be paid for HERE, at placement, or a sell resting below the mark (a buy above it)
    // is admitted on its initial margin alone and fills into an open loss the account never had —
    // the counterparty, possibly the user's own sub-account, collecting it as profit. See
    // [`cross_rest_open_loss`].
    let open_loss = cross_rest_open_loss(market, pre, side, price, qty)?;
    // `pos_after` overrides storage for this market: it is not written until PHASE 2. ONE fold:
    // its `A` is the gate below (`derived_available_balance_with` is exactly this fold's `a`), its
    // `E` / `Σ_C MM` the maintenance check after it (R2) — no load beyond the walk the gate made.
    let fold = crate::margin::view::cross_fold_checked(
        context,
        user,
        Some(wallet),
        Some((market_id, &pos_after)),
        None,
    )?;
    let available_after = fold.a;
    // `A_after − OpenLoss ≥ 0`, i.e. `Δ ooIM + OpenLoss ≤ A(before)` — the measured delta form with
    // the order's whole Binance cost. The B1 escape is unchanged: an order whose cost is not
    // positive (`Δ ooIM ≤ 0` and no open loss) rests at any balance.
    if available_after < open_loss as i128 {
        let delta = crate::margin::view::derived_requirement_delta(market, &pos_basis, &pos_after)?;
        if delta > 0 || open_loss > 0 {
            // Write-clean (validate-then-apply: the probe above materialised nothing, and
            // `deferred` is dropped unflushed). A clean whole-order reject.
            let cross = crate::margin::margin_type::effective_margin_type(&pos_basis)
                == crate::types::MarginType::Crossed;
            return Ok(RestOutcome::RefusedUnaffordable { cross });
        }
    }
    // ── R2: J1 AT REST — an order that can open must find the account at cross maintenance ──────
    //
    // The gate above is `A`'s, so an account that already sits UNDER cross maintenance while
    // `A ≥ 0` (a leg whose `MM > PIM` after tier drift) could still rest an opening order and —
    // no maker fill being checked (Q9) — have it filled, which the taker's J1 refuses. So the
    // resting order is held to the liquidation predicate itself, on the same fold:
    // `E − OpenLoss ≥ Σ_C MM`. Resting moves neither `E` nor `Σ_C MM` (only `ooIM`), so these are
    // the post-rest values; the Open Loss is what the fill would book against `E` at the mark.
    //
    // WHICH orders: the ones that can OPEN (`opening_part_of_order > 0`) of an account holding
    // cross exposure (`has_cross`) — a cross order, and an isolated one too (its fill would move
    // silo cash out of the cross wallet). Never a reduce-only order (it cannot open, whatever the
    // conservative opening part says about the orders ahead of it — a fired stop included) nor a
    // pure reduce, which keep B1. An account with no cross position never reaches it, so every
    // isolated outcome is what it was.
    if fold.has_cross
        && !flags.reduce_only
        && opening_part_of_order(pre, side, qty) > 0
        && fold.e - (open_loss as i128) < fold.cross_mm
    {
        // Write-clean, like the refusals above.
        return Ok(RestOutcome::RefusedUnderCrossMaintenance);
    }

    // ══════════════════════════════════════════════════════════════════════════════════════════
    // ── THE WRITE BARRIER ── every refusal has passed; from here the placement is ACCEPTED.
    // ══════════════════════════════════════════════════════════════════════════════════════════
    let flushed = deferred.is_some();
    if let Some(d) = deferred {
        d.apply(context, side, market, pending_placed)?;
    }

    // ══════════════════════════════════════════════════════════════════════════════════════════
    // PHASE 2 — APPLY. Mechanical: it writes what PHASE 1 decided and decides nothing itself.
    // ══════════════════════════════════════════════════════════════════════════════════════════

    // Post-barrier BBO for the two CACHE writes only. Re-resolved when something was flushed,
    // because the match consumed the opposite side and a cover cancel may have moved our own.
    let (best_bid, best_ask) = if flushed {
        let hot = storage::load_market_hot(context, market_id)?;
        (hot.best_bid, hot.best_ask)
    } else {
        (best_bid, best_ask)
    };
    let becomes_best = match side {
        Side::Buy => best_bid == 0 || price > best_bid,
        Side::Sell => best_ask == 0 || price < best_ask,
    };
    debug_assert!(
        !becomes_best || may_become_best,
        "an order that BECAME the best was not band-checked — `may_become_best` must be a \
         superset of the post-barrier `becomes_best` (see the note on the band gate)"
    );

    // The store is now the state PHASE 1 measured, so the aggregates are re-derived from it rather
    // than carried over from `pos_after`: the add is the same two `checked_add`s on the same two
    // integers, and doing it here means PHASE 2 can never clobber a field the flush wrote.
    let mut pos = storage::load_position(context, user, market_id)?;
    debug_assert_eq!(
        (
            pos.total_buy_qty,
            pos.total_buy_notional,
            pos.total_sell_qty,
            pos.total_sell_notional
        ),
        (
            pos_basis.total_buy_qty,
            pos_basis.total_buy_notional,
            pos_basis.total_sell_qty,
            pos_basis.total_sell_notional
        ),
        "the rest basis diverged from what the flush actually wrote — the PHASE 1 decision was \
         taken on the wrong state (`settlement::RestBasis`)"
    );
    #[cfg(debug_assertions)]
    {
        // The aggregates ARE `Bid`/`Ask` and feed the admission gate directly, so keep the raw-list
        // fold as a live oracle on them. Both lists are read POST-barrier, against the POST-barrier
        // position, so a flush that moved one without the other fails here. (Debug-only: the two
        // list loads exist for this check alone.)
        let buy_entries = storage::load_buy_orders_ref(context, user, market_id)?;
        let sell_entries = storage::load_sell_orders_ref(context, user, market_id)?;
        debug_assert_totals(
            entries_iter(&buy_entries),
            entries_iter(&sell_entries),
            &pos,
            base_decimals,
            price_decimals,
        );
    }
    match side {
        Side::Buy => {
            pos.total_buy_qty = add_decided_aggregate(pos.total_buy_qty, qty, "buy qty")?;
            pos.total_buy_notional =
                add_decided_aggregate(pos.total_buy_notional, entry_notional, "buy notional")?;
        }
        Side::Sell => {
            pos.total_sell_qty = add_decided_aggregate(pos.total_sell_qty, qty, "sell qty")?;
            pos.total_sell_notional =
                add_decided_aggregate(pos.total_sell_notional, entry_notional, "sell notional")?;
        }
    }

    // The real insert: in-place on a warm list (zero clone), or one materialize-clone on a cold
    // first-touch (unavoidable — it IS the write of a previously-committed list). `partition_point`
    // re-derives the same index the probe would have.
    match side {
        Side::Buy => {
            storage::mutate_buy_orders(context, user, market_id, |list| {
                let i = list.partition_point(|e| e.price > price);
                list.insert(i, new_entry);
            })?;
            storage::insert_bid_price(context, market_id, price)?;
            // push_bid_order also bumps the level's live count (folded into the level blob).
            storage::push_bid_order(context, market_id, price, *order_id)?;

            // Keep best_bid cache up to date.
            if becomes_best {
                storage::save_best_bid(context, market_id, price)?;
                // best_ask from the POST-barrier resolve above — the match consumed that side.
                record_mid_price_sample_for_best_quote_change(context, market_id, price, best_ask)?;
            }
        }
        Side::Sell => {
            storage::mutate_sell_orders(context, user, market_id, |list| {
                let i = list.partition_point(|e| e.price < price);
                list.insert(i, new_entry);
            })?;
            storage::insert_ask_price(context, market_id, price)?;
            // push_ask_order also bumps the level's live count (folded into the level blob).
            storage::push_ask_order(context, market_id, price, *order_id)?;

            // Keep best_ask cache up to date.
            if becomes_best {
                storage::save_best_ask(context, market_id, price)?;
                // best_bid from the POST-barrier resolve above — the match consumed that side.
                record_mid_price_sample_for_best_quote_change(context, market_id, best_bid, price)?;
            }
        }
    }

    // Resting only touches the per-side aggregates, so this skips save_position's old-position
    // re-read + `amount` zero-crossing registry hooks (dead work here) and writes just the four
    // aggregates onto the stored position, in place.
    //
    // It is ALSO what keeps a pure placement silent on the `@account` stream: an aggregates-only
    // write does not mark the user for an `AccountBalanceChanged` snapshot, while `save_position`
    // would. Resting moves no money and moves no position state — the two things Binance's
    // `ACCOUNT_UPDATE` reports — so it publishes nothing. See the trigger table and the R14 /
    // official-doc citation on `storage::mark_account_snapshot_dirty`.
    storage::save_position_reservation_only(
        context,
        user,
        market_id,
        storage::SideAggregates::of(&pos),
    )?;

    emit_pending_order_placed(context, pending_placed);
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderRested {
            user,
            marketId: market_id,
            orderId: FixedBytes(*order_id),
            side: side as u8,
            price,
            quantity: qty,
            tif: tif as u8,
            clientOrderId: FixedBytes(client_order_id),
            // The FROZEN margin basis, straight off the entry this call just inserted — the same
            // integer `new_entry.margin_notional` was folded into `total_buy/sell_notional` with,
            // so `quantity × assumingPrice` reproduces this order's contribution to
            // `getPositionRisk`'s `bidNotional`/`askNotional` with no re-derivation. It is
            // published because it is un-derivable off-chain: for a sell it is `max(T, limit)` with
            // `T` resolved ONCE, above, and never again (see THE FREEZE).
            assumingPrice: assuming_price,
            flags: flags.encode(),
        }
        .to_log_data(),
    });

    // ── reduce-only: this order may have OVERTAKEN the owner's reduce-only orders ────────────
    //
    // The third way the prefix condition breaks, and the one that is not about the position at all:
    // a NORMAL order nearer the touch fills FIRST, so it grows the "ahead" term of every
    // reduce-only order behind it. Nothing refuses the normal order — it carries no reduce-only
    // predicate of its own, and §1.6's R20 measured Binance accepting it and killing the
    // reduce-only order instead. Normal orders are first-class; reduce-only ones are not.
    //
    // Skipped for a reduce-only order resting: `admit` already priced it against this same
    // condition, so re-running the pass here could only evict the order that just passed.
    //
    // The whole cost when the owner holds no reduce-only order on this side — the overwhelmingly
    // common case — is one scan of the list this call already had warm. The heavy part (loading the
    // position, folding the prefix) is behind that check.
    //
    // PHASE 1 replayed this pass (`pos_pre`) and decided on its outcome; this is the real one, run
    // whenever it could evict — not only when the replay did — so the prefix condition never
    // depends on the replay's plumbing. The assertion below holds the two to each other.
    if !flags.reduce_only {
        crate::trading::reduce_only::restore(
            context,
            user,
            market,
            side,
            crate::types::CancelReason::ReduceOnlyOvertaken,
        )?;
    }
    #[cfg(debug_assertions)]
    {
        // PHASE 2 wrote what PHASE 1 decided on: the rest's own terms, less exactly the reduce-only
        // orders the replay evicted.
        let settled = storage::load_position_ref(context, user, market_id)?;
        debug_assert_eq!(
            (
                settled.total_buy_qty,
                settled.total_buy_notional,
                settled.total_sell_qty,
                settled.total_sell_notional
            ),
            (
                pos_after.total_buy_qty,
                pos_after.total_buy_notional,
                pos_after.total_sell_qty,
                pos_after.total_sell_notional
            ),
            "the overtake pass evicted other orders than PHASE 1 replayed — the rest was admitted \
             on the wrong state (`replay_reduce_only_restore`)"
        );
    }

    Ok(RestOutcome::Rested)
}
