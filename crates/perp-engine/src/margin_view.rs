//! Derived margin layer — the single source of every open-order margin number in the engine.
//!
//! # What this is
//!
//! Binance USDⓈ-M stores only a small ledger (`walletBalance`, per-position `isolatedWallet`,
//! `positionAmt`, `entryPrice`) and **derives** every margin quantity on read. We now do the same:
//! the six escrow fields this module used to merely *report against* (`margin_reserved`,
//! `margin_reserved_notional`, `buy/sell_side_margin_reserved`, `buy/sell_side_reserved_notional`)
//! are GONE, nothing is debited from the wallet when an order rests, and the open-order
//! requirement is computed here, on demand, from `(N, Bid, Ask, L)`.
//!
//! So this is no longer a pure read layer: [`derived_available_balance`] is the **admission
//! basis** every money-out gate in the engine now asks. It still writes nothing — every loader it
//! calls is a `_ref` (cache-fill, never dirty-mark) reader, so it enters no key into the block
//! delta — but its ANSWER now decides accept/reject.
//!
//! ```text
//! Bid            = pos.total_buy_notional      Σ resting buys  qty × frozen assuming price
//! Ask            = pos.total_sell_notional     Σ resting sells qty × frozen assuming price
//! N              = trunc(positionAmt × mark)                            ← the only LIVE input
//! ooIM(market)   = ROUND_UP(max(|N + Bid|, |N − Ask|) / L) − ROUND_UP(|N| / L)
//! available      = perp_wallet_balance − Σ_markets ooIM(market)
//! ```
//!
//! summed over the per-user market index (the only enumerable set of a user's markets).
//! `perp_wallet_balance` is the analogue of Binance's `crossWalletBalance`: position margin has
//! been physically moved out of it, the open-order requirement has NOT.
//!
//! # `Bid` and `Ask` are FROZEN per order; `N` is not
//!
//! Each resting order's contribution is fixed when it is placed, at its **Assuming Price** — its
//! limit price for a buy, `max(max(⌈lastTraded × 1.0015⌉, mark), limit)` for a sell — and never
//! re-resolved (`types::OrderEntry::assuming_price`; MEASURED, R12,
//! `misc/binance-flip-and-admission.md` §3.13). So both aggregates are read O(1) off the position
//! blob and this layer touches no order list and no `MarketHot` at all. The read-time `T`
//! resolution and the ascending-prefix re-fold that used to live here are GONE — they implemented
//! the `H_live` hypothesis R12 refuted by 1939 quanta.
//!
//! ⚠️ **That does NOT make `ooIM` frozen.** `N` is recomputed from the current mark on every
//! evaluation, so the requirement still moves with the mark whenever the position is non-flat —
//! independently measured (R10's 15 dense snapshots; R8's 4-second verdict flip). Only at `N = 0`
//! does `ooIM` degenerate to a constant, and that is the single branch R12 measured; the claim
//! 「挂单的托管是个常数」must not be extrapolated past it.
//!
//! The staleness this buys is deliberate and is the exposure the docs name: rest a sell, let the
//! market run 5%, and its `Ask` term still reflects the old print while an equivalent NEW order
//! would cost more. 「我们自己的账必须存下单时的值,不能每次重算。」
//!
//! # Formula source
//!
//! `misc/binance-margin-verified-model.md` §1.1 (the formula set) and §2 (the rounding model),
//! plus `misc/binance-v3-account-balance-field-reference.md` §2/§4 (field-by-field, with the
//! discriminating mainnet samples). Every rounding mode below cites the evidence that settled
//! it. One behaviour of Binance's is deliberately **not** copied — see [`run_get_account_margin`]
//! (`availableBalance` is not clamped at zero).
//!
//! ⚠️ **What is NOT measured:** all ten mainnet runs behind the formula used a LONG position, so the
//! form the joint `max()` takes for a SHORT (`N < 0`) is EXTRAPOLATED — a **BLOCKING** open item per
//! §6 (「空头侧符号」) since docs commit `8d179c0`. [`crate::math::open_order_margin`] carries the
//! full note; [`position_derived_margin`] marks the one place the sign enters.

use alloy_sol_types::SolCall;
use primitives::{Address, Bytes};

use crate::host::PerpHost;
use crate::{
    errors::{perp_err, perp_invariant_err},
    interface::IPerpDex::{
        getAccountMarginCall, getAccountMarginReturn, getMarginInfoCall, getMarginInfoReturn,
    },
    math::{calc_value_i64, checked_u64_to_i64, maintenance_margin, open_order_margin},
    storage, PerpError,
};

/// Only the debug-only Bid/Ask oracle in [`compute_margin_info`] folds the raw order lists; the
/// production path reads the maintained aggregates.
#[cfg(debug_assertions)]
use crate::math::sum_side_totals;

/// Prefix a `math::` error from the shared ooIM helper with the caller that hit it, so the two
/// call sites (`getMarginInfo` and the admission-path Σ walk) stay distinguishable in a revert
/// string. Fatals propagate verbatim — only business rejects are reshaped.
fn relabel_derived(e: PerpError, who: &str) -> PerpError {
    match e {
        PerpError::Reject(m) => perp_err(format!("{who}: {m}")),
        other => other,
    }
}

/// Maximum number of market ids `getAccountMargin` will fold in one call.
///
/// The array is caller-supplied and the selector's gas is flat, so it needs a bound. 64 is the
/// same order as `MAX_BATCH_PLACE`; a client with more markets than this pages the call.
pub const MAX_MARGIN_INFO_MARKETS: usize = 64;

/// Every Binance-shaped margin quantity for one `(user, market)`, plus the two of ours the
/// caller compares them against. Field-for-field the return of `getMarginInfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarginInfo {
    // ── inputs (returned so the caller can recompute every derived field locally) ──
    /// Market mark price, in the market's `price_decimals` fixed-point units.
    pub mark_price: u64,
    /// Signed net position in base units (positive = long).
    pub position_amt: i64,
    /// Virtual quote balance; `entry = -v_quote_balance / position_amt`.
    pub v_quote_balance: i64,
    /// Position leverage, floored at 1.
    pub leverage: u64,
    /// `Bid` — Σ `qty × frozen Assuming Price` over the user's resting BUYS in this market (a long
    /// order's Assuming Price is its limit price, so there is no markup here).
    pub bid_notional: u64,
    /// `Ask` — Σ `qty × frozen Assuming Price` over resting SELLS, each order's price fixed at
    /// `max(T, limit)` when it was placed (`T` = [`crate::math::assuming_price_floor`]). Mainnet R10
    /// measured Binance's own reported `askNotional / q == limit × 1.0015` for a sell resting below
    /// `T`, and R12 measured that the value then does not move.
    pub ask_notional: u64,
    // ── derived, Binance formulas ──
    /// `N = trunc(|positionAmt| × markPrice)`.
    pub notional: u64,
    /// `positionAmt × (markPrice − entryPrice)`, truncated toward zero.
    pub unrealized_profit: i64,
    /// `isolatedWallet + unrealizedProfit` — position EQUITY at mark, not a balance.
    pub isolated_margin: i64,
    /// `ROUND_UP(N / L)`.
    pub position_initial_margin: u64,
    /// `initialMargin − positionInitialMargin`.
    pub open_order_initial_margin: u64,
    /// `ROUND_UP(max(|N + Bid|, |N − Ask|) / L)` — the joint requirement.
    pub initial_margin: u64,
    /// Maintenance margin at `N` under this market's tier table.
    pub maint_margin: u64,
    /// The position's own allocated margin — our `isolatedWallet`.
    pub position_margin: i64,
}

/// The derived open-order requirement for ONE `(market, position)` pair, computed from
/// IN-MEMORY values only — no storage access.
///
/// This is the single entry point through which every ooIM number in the engine is produced: the
/// `getMarginInfo` read path calls it on a stored position, and the admission gates call it on
/// the in-memory post-operation position they are about to write. Anything that needs an ooIM and
/// does not come through here is a second implementation and must be deleted.
///
/// `Bid`/`Ask` come from the two maintained per-side aggregates on `pos` (proven equal to the
/// resting-order fold at each entry's FROZEN Assuming Price; see [`compute_margin_info`]), `N` from
/// `pos.amount` at `market.mark_price`, and `L` from `pos.leverage` — UNCAPPED by the tier table
/// (see the "Why no tier cap" note on [`crate::math::open_order_margin`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PositionDerivedMargin {
    /// `N` — signed position notional at MARK, truncated toward zero. The only LIVE input.
    pub signed_notional: i64,
    /// `Bid` — Σ (remaining qty × frozen Assuming Price) over resting buys.
    pub bid_notional: u64,
    /// `Ask` — Σ (remaining qty × frozen Assuming Price) over resting sells.
    pub ask_notional: u64,
    /// PIM / IM / ooIM at the position's own leverage.
    pub margin: crate::math::OpenOrderMargin,
}

/// Compute [`PositionDerivedMargin`] for one `(market, position)`. Pure function, no storage.
///
/// # This is where the POSITION'S SIGN enters the derived margin — and it is the ONLY place
///
/// The sign of `pos.amount` reaches the formula through exactly one channel, the `calc_value_i64`
/// below, which preserves it (`i128` division truncates TOWARD ZERO, so a short yields exactly
/// `−trunc(|amount| × mark)` — the same magnitude as the long case). Everything downstream is
/// [`crate::math::open_order_margin`]'s three uses of it: `n + bid`, `n − ask`, and the sign-free
/// `|n|` of `PIM`.
///
/// `Bid` and `Ask` are keyed to the ORDER's side and never to the position's, so the position sign
/// does **not** enter them: `pos.total_buy_notional` is the buy fold whether the position is long,
/// short or flat, and the Assuming-Price markup is baked into resting SELLS on the same basis.
///
/// ⚠️ The short-side (`pos.amount < 0`) form of the joint `max()` is **EXTRAPOLATED**, and
/// `misc/binance-margin-verified-model.md` §6 classifies it as a **BLOCKING** open item as of docs
/// commit `8d179c0`. See the dedicated section on [`crate::math::open_order_margin`] before
/// touching anything here.
pub fn position_derived_margin(
    market: &crate::types::Market,
    pos: &crate::types::PerpPosition,
) -> Result<PositionDerivedMargin, PerpError> {
    // ── SIGN ENTRY POINT (the only one) ──
    // `pos.amount` is signed and its sign survives into `N`; see the doc comment above.
    let signed_notional = calc_value_i64(
        market.mark_price,
        pos.amount,
        market.base_decimals,
        market.price_decimals,
    )?;
    let (bid_notional, ask_notional) = (pos.total_buy_notional, pos.total_sell_notional);
    Ok(PositionDerivedMargin {
        signed_notional,
        bid_notional,
        ask_notional,
        margin: open_order_margin(
            signed_notional,
            bid_notional,
            ask_notional,
            // `pos.leverage` may be a defaulted/corrupt 0; `open_order_margin` floors it at 1.
            pos.leverage,
        )?,
    })
}

/// `ooIM` for one `(market, position)` — the requirement the position's RESTING ORDERS add, on
/// top of what the position alone already needs. The quantity every gate sums.
#[inline]
pub fn position_open_order_margin(
    market: &crate::types::Market,
    pos: &crate::types::PerpPosition,
) -> Result<u64, PerpError> {
    Ok(position_derived_margin(market, pos)?
        .margin
        .open_order_initial_margin)
}

// ── Assuming-Price resolution (PLACEMENT-TIME ONLY) ───────────────────────────────────────────

/// `T` for one market: `max(ROUND_UP(lastTraded × 1.0015), mark)`.
///
/// # Called at PLACEMENT, never on a read path
///
/// This resolves the floor a NEW sell's [`crate::types::OrderEntry::assuming_price`] is frozen
/// against — at the moment it starts resting, and at the admission gate that prices the same
/// hypothetical one statement earlier. Nothing that reports or sums an existing order's requirement
/// calls it: those read the frozen aggregates off the position blob. Re-resolving `T` for an order
/// that is already resting is exactly the `H_live` behaviour R12 refuted, so a new call site here
/// on a read path is a bug.
///
/// `lastTraded` is our analogue of Binance's `Last Price`: the doc is explicit that Last Price is
/// "市场最新成交价" — the market's latest TRADED price from `/fapi/v1/ticker/price`, not the mark,
/// not the index, and not the caller's own fill (`misc/binance-flip-and-admission.md` §1.6b, final
/// ⚠). `MarketHot::last_traded` is exactly that: written by the match loop with the last executed
/// price of every match, and already consumed under the name "contract price" as the third input to
/// the mark-price median. The index price and `PriceBasisWindow` are oracle/top-of-book series, not
/// trade prints, so neither is the right input.
///
/// Costs one `MarketHot` load (cache-fill, never dirty-marking, so it enters no key into the block
/// delta). The mark comes off the already-threaded `Market`.
pub fn assuming_price_floor<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &crate::types::Market,
) -> Result<u64, PerpError> {
    let last_traded = storage::load_market_hot(context, market_id)?.last_traded;
    crate::math::assuming_price_floor(last_traded, market.mark_price)
}

/// Compute [`MarginInfo`] for one `(user, market)`. Pure: reads only, no writes.
///
/// Errors if the market does not exist (its `base_decimals` / `price_decimals` / tier table are
/// required inputs, and fabricating a zero market would silently report zeros).
pub fn compute_margin_info<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<MarginInfo, PerpError> {
    let market = storage::load_market_ref(context, market_id)?
        .ok_or_else(|| perp_err("getMarginInfo: unknown market"))?;
    let (base_decimals, price_decimals) = (market.base_decimals, market.price_decimals);
    let pos = storage::load_position_ref(context, user, market_id)?;
    // Leverage is floored at 1 here as it is inside `open_order_margin`, so a defaulted or
    // corrupt 0 divides as 1 rather than trapping.
    let leverage = pos.leverage.max(1);

    // ── Bid / Ask ────────────────────────────────────────────────────────────────────────
    // ONE rule, both sides: `Σ (remaining qty × that order's FROZEN Assuming Price)`, read O(1) off
    // the two maintained aggregates rather than re-folding the lists. They ARE `Bid`/`Ask` by
    // construction (the same per-order-floored `calc_value(assuming_price, amount)` terms every
    // maintenance site adds and subtracts), and the property test
    // `side_aggregates_are_exactly_bid_and_ask_after_every_operation` proves it holds after every
    // transition, with the list fold as the independent ground truth.
    //
    // The side asymmetry lives ENTIRELY in the per-order `assuming_price`: a buy's is its limit
    // price (no markup), a sell's is `max(T, limit)` fixed when it was placed. So there is no
    // read-time `T`, no `MarketHot` load, and no list load on this path — R12 measured that a
    // resting order's reported contribution does not move, and re-deriving it here would be the
    // refuted `H_live`.
    //
    // Binance warns that its OWN `bidNotional`/`askNotional` are not reproducible from
    // `qty × price` (two arithmetic paths coexist server-side, 1e-5 apart) and must be read
    // from the response body. We have no such split: this IS the definition, evaluated once.
    let bid_notional = pos.total_buy_notional;
    let ask_notional = pos.total_sell_notional;
    #[cfg(debug_assertions)]
    {
        let buy_entries = storage::load_buy_orders_ref(context, user, market_id)?;
        let sell_entries = storage::load_sell_orders_ref(context, user, market_id)?;
        let (_, bid_fold) =
            sum_side_totals(buy_entries.iter().copied(), base_decimals, price_decimals)?;
        let (_, ask_fold) =
            sum_side_totals(sell_entries.iter().copied(), base_decimals, price_decimals)?;
        debug_assert_eq!(
            (bid_notional, ask_notional),
            (bid_fold, ask_fold),
            "getMarginInfo: maintained (Bid, Ask) for {user} market {market_id} diverged from the \
             resting-order fold at each entry's frozen assuming price"
        );
        // The FREEZE itself, on the buy side where it is checkable from current state: a buy's
        // Assuming Price IS its limit price, always, so any markup leaking onto the buy fold shows
        // up here. (A sell's frozen price cannot be re-derived from current state — that is the
        // point of freezing it — so the sell claim it CAN carry is the aggregate/fold agreement
        // above plus `assuming_price >= price`, both asserted by the property test.)
        for e in buy_entries.iter() {
            debug_assert_eq!(
                e.assuming_price, e.price,
                "getMarginInfo: buy entry {:?} of {user} carries a marked-up assuming price",
                e.order_id
            );
        }
    }

    // ── notional ────────────────────────────────────────────────────────────────────────
    // `N = trunc(|positionAmt| × markPrice)` — TRUNCATED (mainnet: `0.001 × 63544.85745652 =
    // 63.54485745652 → 63.54485745`; HALF_UP would give `...46`). Every field below consumes
    // this truncated value, never the raw mark.
    //
    // `calc_value_i64` divides in `i128`, and Rust integer division truncates TOWARD ZERO, so
    // for a short (`amount < 0`) it yields exactly `-trunc(|amount| × mark)`, i.e. the same
    // magnitude as the long case. That identity is what makes `notional` below (an
    // `unsigned_abs` of the signed value) equal to the doc's `trunc(|positionAmt| × markPrice)`
    // for BOTH signs, and it is the trunc-toward-zero (not floor) that the doc's negative-PnL
    // sample settles: `exact = −0.08323233884 → obs = −0.08323233`, floor would give `…34`
    // (`binance-v3-account-balance-field-reference.md` §4, 11/11 samples).
    let signed_notional = calc_value_i64(
        market.mark_price,
        pos.amount,
        base_decimals,
        price_decimals,
    )?;
    let notional = signed_notional.unsigned_abs();

    // ── unrealizedProfit ────────────────────────────────────────────────────────────────
    // Binance: `positionAmt × (markPrice − entryPrice)`. Our `v_quote_balance` is
    // `−(positionAmt × entryPrice)` accumulated exactly at fill time, so
    // `signedNotional + vQuoteBalance` is the same quantity with no second rounding step: the
    // ONLY rounding in it is the truncation already inside `signedNotional`.
    let unrealized_profit = signed_notional
        .checked_add(pos.v_quote_balance)
        .ok_or_else(|| perp_err("getMarginInfo: unrealized profit overflow"))?;

    // ── isolatedMargin ──────────────────────────────────────────────────────────────────
    // `isolatedWallet + unrealizedProfit`, where our `isolatedWallet` is `pos.margin`.
    // Market-valued EQUITY, not a balance: it can sit below `pos.margin`, and can go negative.
    let isolated_margin = pos
        .margin
        .checked_add(unrealized_profit)
        .ok_or_else(|| perp_err("getMarginInfo: isolated margin overflow"))?;

    // ── positionInitialMargin / initialMargin / openOrderInitialMargin ──────────────────
    // The formula itself lives in `math::open_order_margin` — ONE implementation, shared
    // verbatim with the admission path (`total_open_order_initial_margin` below). Everything above
    // this line is this layer's job: turning storage into the four pure inputs `(N, Bid, Ask, L)`.
    //
    //   PIM  = ROUND_UP(|N| / L)
    //   IM   = ROUND_UP( max(|N + Bid|, |N − Ask|) / L )
    //   ooIM = IM − PIM
    //
    // IM is a genuine `max()`, not "one side always wins" and not "the two sides add": mainnet
    // run2 P2/P3/P4 rule out both rivals, and P3→P4 switches the winning branch by changing only
    // the buy quantity (|N+Bid| goes from 15.31 behind to 168.39 ahead). The two branches are the
    // exposure left if every BUY fills and if every SELL fills.
    //
    // `N` is passed SIGNED. ⚠️ The doc's samples are all LONGS (ten runs, every one of them), where
    // signed == the unsigned `notional` field — so the short-side form of the joint `max()` is
    // EXTRAPOLATED, and `binance-margin-verified-model.md` §6 (「空头侧符号」) UPGRADED it from
    // 「低优先」 to a BLOCKING open item in docs commit `8d179c0`. The signed reading is the one the
    // branch SEMANTICS force ("多头暴露 / 空头暴露"): for a short, unsigned `N` would make
    // `|N − Ask|` understate the very exposure the ask branch exists to measure. It is our choice,
    // not a measurement — see the "SHORT side is EXTRAPOLATED" section on `math::open_order_margin`
    // and the characterisation tests it names.
    //
    // The `notional` field reported just above deliberately DISCARDS the sign
    // (`signed_notional.unsigned_abs()`), matching Binance's own unsigned `notional`; only the
    // joint `max()` below sees the sign.
    //
    // NOTE this deliberately mixes bases: `N` is at MARK, `Bid` at each buy's LIMIT price, `Ask` at
    // each sell's ASSUMING price. That is Binance's formula, and this layer reports Binance's
    // numbers.
    //
    // At the position's OWN leverage, UNCAPPED by the tier table: Binance derives `initialMargin`
    // from the position's `leverage` field. There is only one ooIM definition — this is the same
    // number the admission gate enforces, not a parallel "reported" one.
    let derived =
        position_derived_margin(&market, &pos).map_err(|e| relabel_derived(e, "getMarginInfo"))?;
    debug_assert_eq!(
        (
            derived.signed_notional,
            derived.bid_notional,
            derived.ask_notional
        ),
        (signed_notional, bid_notional, ask_notional),
        "position_derived_margin must consume the same (N, Bid, Ask) this function reports"
    );
    let position_initial_margin = derived.margin.position_initial_margin;
    let initial_margin = derived.margin.initial_margin;
    let open_order_initial_margin = derived.margin.open_order_initial_margin;

    // ── maintMargin ─────────────────────────────────────────────────────────────────────
    // Binance: `trunc(N × MMR − cum)` with the tier recursion
    // `cum(n+1) = cum(n) + cap(n) × (MMR(n+1) − MMR(n))`. `maintenance_margin` is the SAME
    // model in its integer-safe (slice / marginal) formulation — algebraically identical over
    // the reals, but with one floor per crossed band instead of one floor of the whole
    // subtractive expression, which removes a +1 discontinuity at 53% of tier boundaries. Ours
    // also truncates, so the rounding DIRECTION matches Binance's. See the long note on
    // `math::maintenance_margin`.
    let maint_margin = u64::try_from(maintenance_margin(
        &market.tiers,
        checked_u64_to_i64(notional, "getMarginInfo: notional")?,
    )?)
    .map_err(|_| perp_err("getMarginInfo: maintenance margin negative"))?;

    Ok(MarginInfo {
        mark_price: market.mark_price,
        position_amt: pos.amount,
        v_quote_balance: pos.v_quote_balance,
        leverage,
        bid_notional,
        ask_notional,
        notional,
        unrealized_profit,
        isolated_margin,
        position_initial_margin,
        open_order_initial_margin,
        initial_margin,
        maint_margin,
        position_margin: pos.margin,
    })
}

// ── Derived-ooIM admission basis (the live enforcement gate) ─────────────────────────────────

/// `Σ_markets ooIM` for `user`, over the per-user market index — the account-level open-order
/// requirement.
///
/// Enumerates exactly the markets the user is active in (non-zero position OR at least one
/// resting order), which is precisely the support of the sum: a market the user has left
/// contributes `N = Bid = Ask = 0` ⇒ `ooIM = 0`. That is what the Phase 0 index was built for —
/// there is no other way to enumerate a user's markets, and walking every market on the exchange
/// would be unbounded.
///
/// `override_market` substitutes an IN-MEMORY position for one market id, so a caller can price
/// the state it is ABOUT to write (the hypothetical post-placement position) or the state a match
/// is holding in its registry working copy, without touching storage. When the overridden id is
/// not yet in the index — a user entering a market — its term is added anyway, which is exactly
/// what the placement gate needs.
///
/// Pure read — every loader it reaches is a `_ref` (cache-fill, never dirty-mark) reader, so it
/// enters no key into the block delta and cannot move the commitment. Returns `u128` so the fold
/// cannot overflow before the caller compares it.
///
/// # ⚠️ DO NOT CACHE THIS, AND DO NOT INCREMENT IT — RE-WALK, EVERY TIME
///
/// See the same warning on [`derived_available_balance_with`]. The account-level aggregate is a
/// LIVE quantity, not a ledger field: R13 measured `Δtotal == ΔooIM_B` on all 30 frames of a
/// hands-off window (`misc/binance-flip-and-admission.md` §3.14), i.e. the total moved with a
/// market's mark while the user did nothing. Any scheme that keeps a previous answer and adjusts it
/// by a delta silently freezes every OTHER market's `N`.
pub fn total_open_order_initial_margin<H: PerpHost>(
    context: &mut H,
    user: Address,
    override_market: Option<(u64, &crate::types::PerpPosition)>,
) -> Result<u128, PerpError> {
    let markets = storage::load_user_markets_ref(context, user)?;
    let mut total: u128 = 0;
    let mut applied_override = false;
    for market_id in markets.iter().copied() {
        // The index is a set, so no id repeats and no term is double-counted.
        let market = storage::load_market_ref(context, market_id)?.ok_or_else(|| {
            crate::errors::perp_invariant_err(format!(
                "derived margin: user market index holds unknown market {market_id}"
            ))
        })?;
        total += match override_market {
            Some((id, over)) if id == market_id => {
                applied_override = true;
                position_open_order_margin(&market, over)?
            }
            _ => {
                // TWO loads per market — `{market, position}`. The `MarketHot` read and the sell-list
                // read the old read-time `Ask` re-fold needed are both gone: both aggregates sit in
                // the position blob, frozen (R12).
                let pos = storage::load_position_ref(context, user, market_id)?;
                position_open_order_margin(&market, &pos)?
            }
        } as u128;
    }
    if let Some((id, over)) = override_market {
        if !applied_override {
            // Entering a market: the index is only written at save time, so the id the caller is
            // pricing is legitimately absent. A market that does not exist contributes nothing
            // (the placement path rejects an unknown market long before this point).
            if let Some(market) = storage::load_market_ref(context, id)? {
                total += position_open_order_margin(&market, over)? as u128;
            }
        }
    }
    Ok(total)
}

/// The user's available balance — **the admission basis**.
///
/// ```text
/// available = perp_wallet_balance − Σ_markets ooIM
/// ```
///
/// There is no gross-up term. Phase 1 needed one (`+ Σ margin_reserved`) because the escrow had
/// ALREADY been debited from the wallet, so subtracting Σ ooIM on top would have charged the
/// open-order requirement twice. Nothing is debited any more: `perp_wallet_balance` IS the gross
/// (Binance's `crossWalletBalance`), and adding anything back here would re-open that
/// double-count from the other side.
///
/// Signed and unclamped, in `i128`: clamping at zero would hide exactly the under-coverage the
/// gate exists to detect, and a mark move alone can legitimately push it negative.
pub fn derived_available_balance<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<i128, PerpError> {
    derived_available_balance_with(context, user, None, None)
}

/// [`derived_available_balance`] with in-memory overrides for the wallet and/or one market's
/// position — the form the fill paths need, where the authoritative values live in a registry
/// working copy that has not been flushed yet.
///
/// # ⚠️ DO NOT CACHE THE RESULT, AND DO NOT DERIVE IT FROM A PREVIOUS CALL BY SUBTRACTING A DELTA
///
/// This must be RECOMPUTED on every check. The temptation is real and the gates now invite it: the
/// admission gate in `rest_in_book` evaluates this at the POST state and reads like incremental
/// arithmetic, and the identity `available(after) == available(before) − Δ ooIM` genuinely holds —
/// but **only within a single call**, where every other market's term is the same integer on both
/// sides. Across calls it does not hold at all: `ooIM_m` contains `N_m = trunc(amount_m × mark_m)`,
/// so every other market's term moves with ITS OWN mark, with no action by the user.
///
/// Measured, twice over:
/// * **R13** — `Δtotal == ΔooIM_B` on 30/30 frames of a hands-off window, and the stronger
///   `availableBalance + totalOpenOrderInitialMargin == totalCrossWalletBalance` closing to `0E-8`
///   over 73 observations *while both terms moved in opposite directions and nothing was traded*:
///   `availableBalance` is a read-time residual, not a stored balance
///   (`misc/binance-flip-and-admission.md` §3.14).
/// * **R8** — an identically-priced probe was ACCEPTED, then REFUSED 4 seconds later, on nothing but
///   the mark falling `4.42 USD` (§1.6c / §3.14's calibration; `d(ooIM)/d(mark) = −2q_L/L`). A cached
///   `available` would have accepted the second probe.
///
/// A cached or delta-adjusted value is therefore not a stale optimisation, it is a WRONG ANSWER —
/// and on the permissive side, which is the side that funds an under-margined position.
pub fn derived_available_balance_with<H: PerpHost>(
    context: &mut H,
    user: Address,
    wallet_override: Option<i64>,
    override_market: Option<(u64, &crate::types::PerpPosition)>,
) -> Result<i128, PerpError> {
    let wallet = match wallet_override {
        Some(w) => w,
        None => storage::load_account_ref(context, user)?.perp_wallet_balance,
    };
    Ok(wallet as i128 - total_open_order_initial_margin(context, user, override_market)? as i128)
}

/// The affordability test on the derived basis: `available >= requirement`, with a NON-POSITIVE
/// requirement always affordable.
///
/// That exemption is the derived-basis restatement of the `has_available_perp(0) == true`
/// invariant (B1): **a risk-reducing or zero-cost action must never be gated on a balance the
/// user does not need.** `available` is signed and can be legitimately negative — a mark move
/// alone can push it there with no action by the user — so without the exemption a user in that
/// state would be refused precisely the operations that would REDUCE their risk: a pure-reduce
/// order (whose `Δ ooIM` is ≤ 0), a close, a cancel-driven cover. It widens no hole: every
/// POSITIVE requirement is still refused unless `available` covers it in full.
#[inline]
pub fn derived_can_afford(available: i128, requirement: i128) -> bool {
    requirement <= 0 || available >= requirement
}

/// `ooIM(after) − ooIM(before)` for one market — the marginal derived requirement of an operation
/// that changes a position's resting-order aggregates, its size, or its leverage.
///
/// Signed: an order that reduces net exposure lowers ooIM, and a fill that grows the position can
/// lower it too (a resting sell against a bigger long nets further). A non-positive delta is free
/// (see [`derived_can_afford`]).
///
/// Both positions are priced at the same mark (`market` is threaded, not re-read) and every ALREADY
/// resting order contributes its frozen term to both sides identically, so the difference isolates
/// the caller's operation and cannot pick up a market move no user action caused.
pub fn derived_requirement_delta(
    market: &crate::types::Market,
    before: &crate::types::PerpPosition,
    after: &crate::types::PerpPosition,
) -> Result<i128, PerpError> {
    Ok(position_open_order_margin(market, after)? as i128
        - position_open_order_margin(market, before)? as i128)
}

/// `getMarginInfo(address user, uint64 marketId) returns (...)` — see the ABI doc comment in
/// [`crate::interface`] for the field-by-field contract.
pub fn run_get_margin_info<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getMarginInfoCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarginInfo: invalid calldata"))?;

    let info = compute_margin_info(context, args.user, args.marketId)?;

    Ok(Bytes::from(getMarginInfoCall::abi_encode_returns(
        &getMarginInfoReturn {
            markPrice: info.mark_price,
            positionAmt: info.position_amt,
            vQuoteBalance: info.v_quote_balance,
            leverage: info.leverage,
            bidNotional: info.bid_notional,
            askNotional: info.ask_notional,
            notional: info.notional,
            unrealizedProfit: info.unrealized_profit,
            isolatedMargin: info.isolated_margin,
            positionInitialMargin: info.position_initial_margin,
            openOrderInitialMargin: info.open_order_initial_margin,
            initialMargin: info.initial_margin,
            maintMargin: info.maint_margin,
            positionMargin: info.position_margin,
        },
    )))
}

// ── Account-level Σ walkers (ONE implementation, two ABI entry points) ───────────────────────
//
// `getAccount` and `getAccountMargin` report the same account-level scalars and differ in exactly
// one thing: where the market set comes from (the per-user index vs the caller's `uint64[]`).
// Everything below the market set — the per-market fold, the six Σ accumulators, the narrowing,
// and the four balance identities — lives here, once. Two copies of margin arithmetic that can
// drift is the failure this layer exists to remove, so there is no second Σ anywhere: an entry
// point that wants account-level numbers calls [`account_margin_scalars`] or it is a bug.

/// Where a market set came from. This selects the ERROR SHAPE for an id that names no market, and
/// **nothing else** — the arithmetic is bit-identical either way, which is the whole point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketSetSource {
    /// The per-user market index (`umkt`) — engine-maintained, therefore TRUSTED and already a
    /// set. An id in it that names no market is corrupt state, not a caller mistake, and is raised
    /// as an `[INVARIANT]` reject — the same guard [`total_open_order_initial_margin`] applies
    /// while walking this same set.
    UserIndex,
    /// A caller-supplied `uint64[]`. An unknown id is an ordinary business reject, labelled with
    /// the offending id so the caller can tell WHICH entry failed. Duplicates are folded once.
    CallerList,
}

/// The six Σ accumulators, in accumulator width so the fold cannot overflow before it is narrowed.
///
/// `total_position_margin` is `Σ isolatedWallet` — the money physically sitting in the position
/// silos. It is the term that separates Binance's `totalWalletBalance` (gross) from its
/// `totalCrossWalletBalance` (our stored `perp_wallet_balance`), and it has to be WALKED: the
/// former `total_perp_collateral` ("TC") aggregate that used to carry it was deliberately deleted
/// (see the note at the end of `types::UserAccount`) because it was derivable state maintained on
/// the hottest write paths. Nothing stores it now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccountMarginTotals {
    /// Σ `initialMargin`.
    pub total_initial_margin: u128,
    /// Σ `positionInitialMargin`.
    pub total_position_initial_margin: u128,
    /// Σ `openOrderInitialMargin` — the same quantity [`total_open_order_initial_margin`] folds
    /// for the admission gate, over the same per-market helper.
    pub total_open_order_initial_margin: u128,
    /// Σ `maintMargin`.
    pub total_maint_margin: u128,
    /// Σ `unrealizedProfit`.
    pub total_unrealized_profit: i128,
    /// Σ `positionMargin` (`isolatedWallet`) — the silos.
    pub total_position_margin: i128,
}

/// Every account-level scalar either view reports, narrowed to the ABI's widths with the balance
/// identities applied. Computed ONCE by [`account_margin_scalars`], so the two entry points can
/// only differ in which of these fields they encode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccountMarginScalars {
    /// Binance `totalWalletBalance` — the GROSS wallet: `totalCrossWalletBalance + Σ isolatedWallet`.
    /// Complete only when the market set is the whole index (see [`MarketSetSource`]).
    pub total_wallet_balance: i64,
    /// Binance `totalCrossWalletBalance` — our stored `perp_wallet_balance`, verbatim. Signed and
    /// unclamped.
    pub total_cross_wallet_balance: i64,
    /// Binance `totalMarginBalance` = `totalWalletBalance + totalUnrealizedProfit` — total account
    /// EQUITY. Note this is GROSS-based, so it is NOT `total_cross_wallet_balance + upnl`.
    pub total_margin_balance: i64,
    /// `totalCrossWalletBalance + totalUnrealizedProfit`. **Not a Binance field** — it is
    /// [`Self::total_margin_balance`] minus `Σ isolatedWallet`, and it is what a caller working
    /// from a PARTIAL market list can honestly be given (a "total wallet balance" over a partial
    /// list would be a total that is not total).
    pub cross_margin_balance: i64,
    /// Binance `totalUnrealizedProfit` = Σ `unrealizedProfit`.
    pub total_unrealized_profit: i64,
    /// Binance `totalInitialMargin` (== the next two, summed).
    pub total_initial_margin: u64,
    /// Binance `totalPositionInitialMargin`.
    pub total_position_initial_margin: u64,
    /// Binance `totalOpenOrderInitialMargin`.
    pub total_open_order_initial_margin: u64,
    /// Binance `totalMaintMargin`.
    pub total_maint_margin: u64,
    /// Binance `availableBalance` = `totalCrossWalletBalance − totalOpenOrderInitialMargin`, and
    /// deliberately **NOT clamped at zero** (Binance's is; see [`run_get_account_margin`]).
    /// Cross-based, not gross: the silos are already out of the cross wallet.
    pub available_balance: i64,
}

/// Fold [`compute_margin_info`] over a market set. **The only account-level Σ in the engine.**
///
/// Deterministic: the ids are consumed in iteration order with no map iteration anywhere, and a
/// repeated id is folded ONCE (a duplicate would double-count every total). The index is already a
/// set, so the dedup is a no-op there and exists for the caller-list path.
///
/// Pure read — every loader it reaches is a `_ref` (cache-fill, never dirty-mark) reader, so it
/// enters no key into the block delta and cannot move the commitment.
pub fn fold_account_margin<H: PerpHost, I: IntoIterator<Item = u64>>(
    context: &mut H,
    user: Address,
    market_ids: I,
    source: MarketSetSource,
    who: &str,
) -> Result<AccountMarginTotals, PerpError> {
    let market_ids = market_ids.into_iter();
    let mut totals = AccountMarginTotals::default();
    let (lower, _) = market_ids.size_hint();
    let mut seen: Vec<u64> = Vec::with_capacity(lower);
    for market_id in market_ids {
        // O(n^2) over a set bounded by MAX_MARGIN_INFO_MARKETS (64) / MAX_USER_MARKETS (16).
        if seen.contains(&market_id) {
            continue;
        }
        seen.push(market_id);
        if source == MarketSetSource::UserIndex
            && storage::load_market_ref(context, market_id)?.is_none()
        {
            // Checked here rather than by reshaping `compute_margin_info`'s reject, so that its
            // OTHER rejects (the overflow guards) stay ordinary rejects on this path too. Same
            // guard, same wording as `total_open_order_initial_margin`.
            return Err(perp_invariant_err(format!(
                "{who}: user market index holds unknown market {market_id}"
            )));
        }
        let info = compute_margin_info(context, user, market_id)
            .map_err(|e| relabel_market(e, who, market_id))?;
        totals.total_initial_margin += info.initial_margin as u128;
        totals.total_position_initial_margin += info.position_initial_margin as u128;
        totals.total_open_order_initial_margin += info.open_order_initial_margin as u128;
        totals.total_maint_margin += info.maint_margin as u128;
        totals.total_unrealized_profit += info.unrealized_profit as i128;
        totals.total_position_margin += info.position_margin as i128;
    }
    Ok(totals)
}

/// [`fold_account_margin`] plus the stored cross wallet, narrowed to the ABI's widths with the
/// four balance identities applied. **The single source of every account-level scalar.**
///
/// ```text
/// totalCrossWalletBalance = perp_wallet_balance                                  (stored)
/// totalWalletBalance      = totalCrossWalletBalance + Σ isolatedWallet           (Binance gross)
/// totalMarginBalance      = totalWalletBalance      + totalUnrealizedProfit      (equity)
/// crossMarginBalance      = totalCrossWalletBalance + totalUnrealizedProfit
/// availableBalance        = totalCrossWalletBalance − totalOpenOrderInitialMargin
/// ```
///
/// Each narrowing is the point where a pathological state surfaces as a clean revert instead of
/// wrapping.
pub fn account_margin_scalars<H: PerpHost, I: IntoIterator<Item = u64>>(
    context: &mut H,
    user: Address,
    market_ids: I,
    source: MarketSetSource,
    who: &str,
) -> Result<AccountMarginScalars, PerpError> {
    let totals = fold_account_margin(context, user, market_ids, source, who)?;
    // Signed and unclamped: this is the CROSS wallet exactly as stored. A negative value is a
    // settled receivable (see `types::UserAccount::perp_wallet_balance`), and hiding it behind a
    // `uint64` floor is precisely the blind spot the old `availablePerpBalance` had.
    let cross = storage::load_account_ref(context, user)?.perp_wallet_balance;

    let narrow_u64 = |v: u128, what: &str| {
        u64::try_from(v).map_err(|_| perp_err(format!("{who}: {what} exceeds u64")))
    };
    let narrow_i64 = |v: i128, what: &str| {
        i64::try_from(v).map_err(|_| perp_err(format!("{who}: {what} exceeds i64")))
    };

    let total_unrealized_profit =
        narrow_i64(totals.total_unrealized_profit, "total unrealized profit")?;
    let total_open_order_initial_margin = narrow_u64(
        totals.total_open_order_initial_margin,
        "total open-order initial margin",
    )?;
    let total_wallet_balance = narrow_i64(
        cross as i128 + totals.total_position_margin,
        "total wallet balance",
    )?;

    Ok(AccountMarginScalars {
        total_wallet_balance,
        total_cross_wallet_balance: cross,
        total_margin_balance: narrow_i64(
            total_wallet_balance as i128 + total_unrealized_profit as i128,
            "total margin balance",
        )?,
        cross_margin_balance: narrow_i64(
            cross as i128 + total_unrealized_profit as i128,
            "cross margin balance",
        )?,
        total_unrealized_profit,
        total_initial_margin: narrow_u64(totals.total_initial_margin, "total initial margin")?,
        total_position_initial_margin: narrow_u64(
            totals.total_position_initial_margin,
            "total position initial margin",
        )?,
        total_open_order_initial_margin,
        total_maint_margin: narrow_u64(totals.total_maint_margin, "total maintenance margin")?,
        available_balance: narrow_i64(
            cross as i128 - total_open_order_initial_margin as i128,
            "available balance",
        )?,
    })
}

/// `getAccountMargin(address user, uint64[] marketIds) returns (...)`.
///
/// A thin shell over [`account_margin_scalars`] with [`MarketSetSource::CallerList`] — the
/// arithmetic is shared verbatim with `getAccount`, which passes the per-user index instead. This
/// function contains no margin math of its own.
///
/// # Why the wallet field is called `totalCrossWalletBalance` and not `walletBalance`
///
/// It used to be called `walletBalance`, and that was a NAMING BUG: Binance's `walletBalance` is
/// the GROSS wallet, ours is the cross wallet, and the two differ by `Σ isolatedWallet` — a full
/// position's margin. Anyone comparing the two same-named fields was off by exactly that.
/// Binance keeps THREE nested balances and derives the innermost on read:
///
/// ```text
/// walletBalance                                                (gross)
/// crossWalletBalance = walletBalance      − Σ isolatedWallet   (net of positions)
/// availableBalance   = crossWalletBalance − Σ ooIM             (net of open orders)
/// ```
///
/// Only the outer two are ledger state there; `Σ ooIM` is never debited from anything. We keep
/// exactly ONE stored balance and it is the MIDDLE one: `perp_wallet_balance` has had the
/// position leg physically taken out of it (margin moves to `pos.margin` at open) and the
/// open-order leg NOT — the escrow that used to debit it was deleted. So
///
/// ```text
/// our totalCrossWalletBalance == Binance totalCrossWalletBalance
/// our availableBalance        == Binance availableBalance  == cross − Σ ooIM
/// Binance totalWalletBalance  == our cross + Σ positionMargin   ← `getAccount` returns this
/// ```
///
/// and `availableBalance` here is genuinely spendable headroom — the SAME quantity the engine's
/// admission gates enforce ([`derived_available_balance`]), not a parallel reporting number.
/// (Under the old escrow it double-subtracted the open-order requirement; that caveat is gone
/// with the escrow.)
///
/// `marginBalance` was renamed `crossMarginBalance` for the same reason: it is `cross + Σ upnl`,
/// whereas Binance's `totalMarginBalance` is `GROSS + Σ upnl`. `getAccount` returns that one.
///
/// # Why `totalWalletBalance` is deliberately NOT returned here
///
/// It would be a total that is not total. `Σ isolatedWallet` over a caller-supplied SHORT list
/// under-counts the silos, so the "gross wallet" it implies would be lower than the real one — and
/// unlike `totalOpenOrderInitialMargin`, whose under-count at least errs toward reporting LESS
/// headroom, an under-counted gross wallet errs toward reporting the account as poorer than it is
/// while giving the field a name that promises completeness. `getAccount` walks the whole index and
/// can name it honestly; this entry point cannot, so it does not offer the field at all.
///
/// # The one deliberate departure from Binance
///
/// **`availableBalance` is NOT clamped at zero.** Binance's is: it was measured reporting
/// `0.00000000` where the true value was `−0.00085981`, and the reference doc's own verdict is
/// that you therefore **cannot use it to tell whether an account is under-covered**. Ours is
/// `int64` and is allowed to go negative — strictly more information, at no cost. A negative
/// value is not an error state and triggers nothing here: like Binance, we do not tear resting
/// orders down mid-life on a mark move, and this call could not write anything if it wanted to.
///
/// Note the sum is over the CALLER'S list of market ids, while the engine's own gate sums over
/// the per-user market index. They agree exactly when the list covers the index; a short list
/// under-counts `totalOpenOrderInitialMargin` and therefore over-reports `availableBalance`.
pub fn run_get_account_margin<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getAccountMarginCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAccountMargin: invalid calldata"))?;
    if args.marketIds.len() > MAX_MARGIN_INFO_MARKETS {
        return Err(perp_err(format!(
            "getAccountMargin: at most {MAX_MARGIN_INFO_MARKETS} market ids"
        )));
    }

    let s = account_margin_scalars(
        context,
        args.user,
        args.marketIds.iter().copied(),
        MarketSetSource::CallerList,
        "getAccountMargin",
    )?;

    Ok(Bytes::from(getAccountMarginCall::abi_encode_returns(
        &getAccountMarginReturn {
            totalCrossWalletBalance: s.total_cross_wallet_balance,
            crossMarginBalance: s.cross_margin_balance,
            totalInitialMargin: s.total_initial_margin,
            totalPositionInitialMargin: s.total_position_initial_margin,
            totalOpenOrderInitialMargin: s.total_open_order_initial_margin,
            totalMaintMargin: s.total_maint_margin,
            totalUnrealizedProfit: s.total_unrealized_profit,
            availableBalance: s.available_balance,
        },
    )))
}

// ── The index-driven account view (ONE producer, two consumers) ──────────────────────────────

/// Every account-level number the precompile publishes for one user, folded over the PER-USER
/// MARKET INDEX — so every total is COMPLETE by construction.
///
/// **This is the single producer for BOTH published surfaces**: the `getAccount(address)` return
/// ([`crate::account::run_get_account`]) and the `AccountBalanceChanged` after-image
/// (`storage::emit_account_balance_changed`). Neither holds arithmetic of its own — each takes
/// this struct and encodes a subset of it — so the event and the view cannot report different
/// numbers for the same state. That equality is pinned by
/// `margin_view::tests::the_event_and_get_account_agree_field_for_field_on_the_same_state` and
/// `trading::tests::matched_call_emits_a_balance_event_at_each_balance_moving_write`, and by the
/// `debug_assertions` gate-agreement check inside [`index_account_view`] itself.
#[derive(Clone, Debug)]
pub struct IndexAccountView {
    /// The market ids folded — the per-user index (`umkt`) verbatim, echoed so a caller can
    /// re-derive every total. Held as the stored `Arc` (no clone).
    pub market_ids: std::sync::Arc<Vec<u64>>,
    /// Spot / withdrawal-layer USDC held inside the DEX. NOT part of any total below.
    pub usdc_balance: primitives::U256,
    /// The account-level margin scalars, all signed-and-unclamped where the quantity can be
    /// negative.
    pub scalars: AccountMarginScalars,
}

/// Fold [`account_margin_scalars`] over the user's whole market index and pair it with the spot
/// USDC balance — the complete published account view.
///
/// Pure read: every loader it reaches is a `_ref`/cache-fill reader, so it enters no key into the
/// block delta and cannot move the block commitment. It is therefore safe to call from a WRITE
/// path (the event does exactly that) — it observes state, it does not touch it.
///
/// Cost: one index load plus, per member market, `{market, position}` — **exactly 2 loads, with no
/// shape-dependent worst case.** It was up to 4 (`+ MarketHot + sell list`) while `Ask` was re-folded
/// from the sell list at a read-time `T`; both of those disappeared with the R12 freeze, since both
/// aggregates now sit in the position blob. Bounded by `MAX_USER_MARKETS` (16) ⇒ ≤ 33 `_ref` loads,
/// flat.
pub fn index_account_view<H: PerpHost>(
    context: &mut H,
    user: Address,
    who: &str,
) -> Result<IndexAccountView, PerpError> {
    // The index is the support of every sum: a market the user has left contributes
    // `N = Bid = Ask = 0`. Held as an owned `Arc` so it can be iterated while `context` is borrowed
    // mutably by the walk.
    let market_ids = storage::load_user_markets_ref(context, user)?;
    let scalars = account_margin_scalars(
        context,
        user,
        market_ids.iter().copied(),
        MarketSetSource::UserIndex,
        who,
    )?;

    // The roll-up's `Σ ooIM` and the hot admission gate's must be the SAME number over the same
    // market set — they share `position_open_order_margin` per market but fold in two places (the
    // gate's walk stays lean on purpose: no maintenance-margin tier walk, no unrealized PnL,
    // because it runs on every placeOrder). Pin the agreement here rather than trusting it, on
    // EVERY produced view — which now includes every emitted event. Compiled out in release, so
    // the second walk costs production nothing.
    #[cfg(debug_assertions)]
    {
        let gate = derived_available_balance(context, user)?;
        debug_assert_eq!(
            gate, scalars.available_balance as i128,
            "{who}: availableBalance must equal the admission gate's own basis"
        );
    }

    let usdc_balance: primitives::U256 = storage::load_account_ref(context, user)?
        .usdc_balance
        .clone()
        .into();

    Ok(IndexAccountView {
        market_ids,
        usdc_balance,
        scalars,
    })
}

/// Re-label a per-market reject so the caller can tell WHICH id in the market set failed
/// (`compute_margin_info` only knows it is "getMarginInfo: unknown market"). Fatals and the
/// shell-level variants propagate verbatim — they are not business rejects and must not be
/// reshaped into one.
fn relabel_market(e: PerpError, who: &str, market_id: u64) -> PerpError {
    match e {
        PerpError::Reject(m) => perp_err(format!("{who}: market {market_id}: {m}")),
        other => other,
    }
}

#[cfg(test)]
#[path = "margin_view_tests.rs"]
mod tests;
