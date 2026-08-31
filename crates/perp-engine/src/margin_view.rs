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
    interface::IPerpDex::{getAccountMarginCall, getAccountMarginReturn, getPositionRiskCall},
    math::{calc_value_i64, checked_u64_to_i64, maintenance_margin, open_order_margin},
    storage, PerpError,
};

/// Only the debug-only Bid/Ask oracle in [`compute_margin_info`] folds the raw order lists; the
/// production path reads the maintained aggregates.
#[cfg(debug_assertions)]
use crate::math::sum_side_totals;

/// Prefix a `math::` error from the shared ooIM helper with the caller that hit it, so the two
/// call sites (the `marginInfo` read path and the admission-path Σ walk) stay distinguishable in a revert
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

/// Every Binance-shaped margin quantity for one `(user, market)`, plus the three of ours the
/// caller compares them against. Field-for-field the payload of
/// [`crate::interface::IPerpDex::AccountPosition`] minus its `marketId` — the row `getPositionRisk`
/// and every element of `getAccount`'s `positions[]` encode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MarginInfo {
    // ── inputs (returned so the caller can recompute every derived field locally) ──
    /// Market mark price, in the market's `price_decimals` fixed-point units.
    pub mark_price: u64,
    /// Signed net position in base units (positive = long).
    pub position_amt: i64,
    /// Virtual quote balance; `entry = -v_quote_balance / position_amt`, returned decoded as
    /// [`Self::entry_price`].
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
    /// Binance `entryPrice` — the position's volume-weighted average entry,
    /// `-v_quote_balance / position_amt` in the market's `price_decimals` units. `0` when the
    /// position is flat.
    ///
    /// Produced by [`crate::math::calc_entry_price`], which is also what the `PositionChanged`
    /// event calls: one derivation, so the event and every `AccountPosition` row agree digit for
    /// digit.
    pub entry_price: u64,
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
    /// The mark price at which this position **IS** liquidatable (greatest such price for a long,
    /// least for a short); `0` when there is none.
    ///
    /// From [`crate::math::calc_liquidation_price`], which BISECTS the very predicate `liquidate()`
    /// enforces ([`crate::math::is_above_maintenance_margin`]) rather than restating it as a
    /// formula — see that function for the monotonicity argument, the conservative rounding, and
    /// the three distinct states `0` collapses.
    ///
    /// # Why it lives IN `MarginInfo` and not in a wrapper beside it
    ///
    /// It was briefly a sixteenth number held OUTSIDE this struct, on the theory that `MarginInfo`
    /// should stay exactly the set two surfaces share. That inverted: `MarginInfo` is now exactly
    /// the set the ONE surface encodes (`interface::IPerpDex::AccountPosition`, which
    /// `getPositionRisk` and `getAccount`'s `positions[]` both return), so a number reported on that
    /// row belongs here or it is plumbed by hand to every construction site — which is a second
    /// derivation waiting to happen.
    ///
    /// It is also the only home where it costs NOTHING to obtain: [`margin_info_of`] already holds
    /// the market's tier table and the position's `(amount, v_quote_balance, margin)` triple, so the
    /// search adds **zero storage loads** and cannot describe a different position than the row it
    /// sits in.
    pub liquidation_price: u64,
}

/// The derived open-order requirement for ONE `(market, position)` pair, computed from
/// IN-MEMORY values only — no storage access.
///
/// This is the single entry point through which every ooIM number in the engine is produced: the
/// row read path calls it on a stored position, and the admission gates call it on
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
///
/// All the arithmetic lives in [`margin_info_of`]; this function's job is the two loads plus the
/// debug-only order-list oracle on the maintained `Bid`/`Ask` aggregates.
pub fn compute_margin_info<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<MarginInfo, PerpError> {
    let market = storage::load_market_ref(context, market_id)?
        .ok_or_else(|| perp_err("marginInfo: unknown market"))?;
    let pos = storage::load_position_ref(context, user, market_id)?;

    // ── Bid / Ask oracle (debug only) ────────────────────────────────────────────────────
    // ONE rule, both sides: `Σ (remaining qty × that order's FROZEN Assuming Price)`, read O(1) off
    // the two maintained aggregates rather than re-folding the lists. They ARE `Bid`/`Ask` by
    // construction (the same per-order-floored `calc_value(assuming_price, amount)` terms every
    // maintenance site adds and subtracts), and the property test
    // `side_aggregates_are_exactly_bid_and_ask_after_every_operation` proves it holds after every
    // transition, with the list fold as the independent ground truth. Keep that fold live here as a
    // second, per-read check — it is the only place the STORED position and the STORED lists can be
    // compared, which is why it does not move into `margin_info_of` (that one is also handed
    // hypothetical positions that deliberately disagree with the lists; see
    // [`compute_margin_info_at`]).
    #[cfg(debug_assertions)]
    {
        let (base_decimals, price_decimals) = (market.base_decimals, market.price_decimals);
        let buy_entries = storage::load_buy_orders_ref(context, user, market_id)?;
        let sell_entries = storage::load_sell_orders_ref(context, user, market_id)?;
        let (_, bid_fold) =
            sum_side_totals(buy_entries.iter().copied(), base_decimals, price_decimals)?;
        let (_, ask_fold) =
            sum_side_totals(sell_entries.iter().copied(), base_decimals, price_decimals)?;
        debug_assert_eq!(
            (pos.total_buy_notional, pos.total_sell_notional),
            (bid_fold, ask_fold),
            "marginInfo: maintained (Bid, Ask) for {user} market {market_id} diverged from the \
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
                "marginInfo: buy entry {:?} of {user} carries a marked-up assuming price",
                e.order_id
            );
        }
    }

    margin_info_of(&market, &pos)
}

/// [`compute_margin_info`] with the position supplied IN MEMORY instead of read from storage — the
/// override arm of the account-level fold ([`fold_account_margin`]).
///
/// Same market load, same arithmetic, same reject shape for an unknown market. What it deliberately
/// does NOT do is run `compute_margin_info`'s order-list oracle: the position it is handed is a
/// HYPOTHETICAL (the post-placement aggregates of an order that is not in the list yet), so it is
/// *supposed* to disagree with the stored lists.
pub fn compute_margin_info_at<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    pos: &crate::types::PerpPosition,
) -> Result<MarginInfo, PerpError> {
    let market = storage::load_market_ref(context, market_id)?
        .ok_or_else(|| perp_err("marginInfo: unknown market"))?;
    margin_info_of(&market, pos)
}

/// Every [`MarginInfo`] field for one `(market, position)` pair, from IN-MEMORY values only — **the
/// single implementation of the per-market margin arithmetic.** Pure function, no storage, no
/// context.
///
/// Both storage-driven entry points ([`compute_margin_info`], [`compute_margin_info_at`]) end here,
/// so a stored position and a hypothetical one are priced by the same code — which is what lets the
/// admission gate, `getAccount` and the `AccountBalanceChanged` after-image share one set of
/// numbers.
pub fn margin_info_of(
    market: &crate::types::Market,
    pos: &crate::types::PerpPosition,
) -> Result<MarginInfo, PerpError> {
    let (base_decimals, price_decimals) = (market.base_decimals, market.price_decimals);
    // Leverage is floored at 1 here as it is inside `open_order_margin`, so a defaulted or
    // corrupt 0 divides as 1 rather than trapping.
    let leverage = pos.leverage.max(1);

    // ── Bid / Ask ────────────────────────────────────────────────────────────────────────
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

    // ── entryPrice ──────────────────────────────────────────────────────────────────────
    // `-v_quote_balance / position_amt`, i.e. the per-unit inverse of the accumulated
    // `-(qty × price)`. From `math::calc_entry_price`, which is ALSO what `PositionChanged`
    // calls — one derivation, so the event and both views cannot report different entries for
    // the same position. Zero for a flat position (no entry exists; that is the function's own
    // early return, not a price of zero). Pure arithmetic on values already in hand: no load.
    let entry_price = crate::math::calc_entry_price(
        pos.amount,
        pos.v_quote_balance,
        base_decimals,
        price_decimals,
    )?;

    // ── unrealizedProfit ────────────────────────────────────────────────────────────────
    // Binance: `positionAmt × (markPrice − entryPrice)`. Our `v_quote_balance` is
    // `−(positionAmt × entryPrice)` accumulated exactly at fill time, so
    // `signedNotional + vQuoteBalance` is the same quantity with no second rounding step: the
    // ONLY rounding in it is the truncation already inside `signedNotional`.
    let unrealized_profit = signed_notional
        .checked_add(pos.v_quote_balance)
        .ok_or_else(|| perp_err("marginInfo: unrealized profit overflow"))?;

    // ── isolatedMargin ──────────────────────────────────────────────────────────────────
    // `isolatedWallet + unrealizedProfit`, where `isolatedWallet` is `pos.margin`.
    // Market-valued EQUITY, not a balance: it can sit below `pos.margin`, and can go negative.
    let isolated_margin = pos
        .margin
        .checked_add(unrealized_profit)
        .ok_or_else(|| perp_err("marginInfo: isolated margin overflow"))?;

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
        position_derived_margin(market, pos).map_err(|e| relabel_derived(e, "marginInfo"))?;
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
        checked_u64_to_i64(notional, "marginInfo: notional")?,
    )?)
    .map_err(|_| perp_err("marginInfo: maintenance margin negative"))?;

    // ── liquidationPrice ────────────────────────────────────────────────────────────────
    // A SEARCH, not a formula: `calc_liquidation_price` bisects `is_above_maintenance_margin` —
    // the exact predicate `liquidate()` and the auto-liquidation sweep enforce — so the reported
    // price is by construction the flip point of the real rule and not an algebraic restatement of
    // it that can drift from the code. Do NOT re-derive it anywhere: this is the single call site
    // in the reporting layer, and every surface reads THIS field.
    //
    // ZERO LOADS. Every input is already in hand at this point in the function: `market.tiers` and
    // the two decimal grids off the `Market` this function was handed, and the position's
    // `(amount, v_quote_balance, margin)` triple off the same `pos` every field above was built
    // from — which is also why the price cannot describe a different position than its own row.
    // The cost is arithmetic only: ~64 iterations for the domain bound plus ~64 for the search,
    // each a handful of `i128` multiplications and a walk of the ≤ MAX_MARGIN_TIERS = 8 table
    // already resident in `market`.
    let liquidation_price = crate::math::calc_liquidation_price(
        &market.tiers,
        pos.amount,
        pos.v_quote_balance,
        pos.margin,
        base_decimals,
        price_decimals,
    )
    .map_err(|e| relabel_derived(e, "marginInfo"))?;

    Ok(MarginInfo {
        mark_price: market.mark_price,
        position_amt: pos.amount,
        v_quote_balance: pos.v_quote_balance,
        leverage,
        bid_notional,
        ask_notional,
        entry_price,
        notional,
        unrealized_profit,
        isolated_margin,
        position_initial_margin,
        open_order_initial_margin,
        initial_margin,
        maint_margin,
        position_margin: pos.margin,
        liquidation_price,
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
/// what the placement gate needs. [`fold_account_margin`]'s override is the same rule, term for
/// term, on the wide fold.
///
/// # Who folds THIS and who folds the wide one
///
/// This is the LEAN fold: one accumulator, no maintenance-margin tier walk, no unrealized PnL, no
/// `Σ isolatedWallet`. It serves the gates that want nothing but `available` —
/// `TakerSettlement::finalize_compute`, `trading::rest_in_book`'s admission gate, its pre-walk
/// early-out, the taker wallet-cover check, `removePositionMargin` — where the five extra
/// accumulators would be arithmetic thrown away.
///
/// The WIDE fold ([`account_margin_scalars`]) is now REST-only: `getAccount` and `getAccountMargin`
/// are its only callers. Both other consumers it once had are gone — `trading::rest_in_book` reverted
/// to this lean form when a pure placement became silent, and the `AccountBalanceChanged` drain took
/// the leaner [`index_account_wallet_balances`] when its payload narrowed to the balances.
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
/// This must be RECOMPUTED on every check. The temptation is real and the gates now invite it:
/// `rest_in_book`'s gate and the taker gates evaluate this at the POST state and read like
/// incremental arithmetic, and the identity `available(after) == available(before) − Δ ooIM` genuinely holds —
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

/// `getPositionRisk(address user, uint64 marketId) returns (AccountPosition)` — ONE
/// [`AccountPositionRow`] for one `(user, market)`. See the ABI doc comment in [`crate::interface`].
///
/// # ONE ROW TYPE, ONE ENCODER, TWO CALL PATHS
///
/// There is no per-selector encoding here at all: this builds the same [`AccountPositionRow`]
/// [`fold_account_margin`] pushes and hands it to the same [`AccountPositionRow::to_abi`]
/// `getAccount` uses. That is the whole point of the selector returning the struct rather than a
/// flat tuple — the single-market and bulk paths cannot report a different row for the same state
/// because there is only one function that writes a row out.
///
/// It replaced a hand-written `sol!` encoder listing all seventeen fields, which was a second
/// implementation of `to_abi` differing only in the type it filled. The row's VALUES could not drift
/// (one [`MarginInfo`], one [`margin_info_of`] call), but a field could still be dropped on the way
/// out of one encoder and not the other, and `liquidation_price` had already been a hand-plumbed
/// sixteenth number once. Now a field added to `AccountPosition` reaches both surfaces or neither.
pub fn run_get_position_risk<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getPositionRiskCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getPositionRisk: invalid calldata"))?;

    // Market FIRST, so an unknown market rejects under THIS selector's name rather than under
    // `compute_margin_info`'s. The load inside `compute_margin_info` then hits the resident blob.
    if storage::load_market_ref(context, args.marketId)?.is_none() {
        return Err(perp_err("getPositionRisk: unknown market"));
    }
    let row = AccountPositionRow {
        market_id: args.marketId,
        info: compute_margin_info(context, args.user, args.marketId)?,
    };

    // `sol!` collapses a single-struct return to the struct itself, so `getPositionRiskReturn` IS
    // `IPerpDex::AccountPosition` — there is not even a per-selector wrapper type left to fill.
    Ok(Bytes::from(getPositionRiskCall::abi_encode_returns(
        &row.to_abi(),
    )))
}

/// One row of `getAccount`'s `positions[]`: a market id and the [`MarginInfo`] for it.
///
/// **Captured, never recomputed.** [`fold_account_margin`] already builds a full `MarginInfo` per
/// market and used to discard everything but the six Σ terms; a row is that same value kept. So
/// `getAccount` returning the rows costs no additional load, no second walk and no second
/// derivation — which is also what makes a bulk row and the one `getPositionRisk` returns incapable
/// of disagreeing: they are the same `margin_info_of` output through the same encoder.
///
/// Bounded by [`crate::types::MAX_USER_MARKETS`] (16) on the index-driven path and by
/// [`MAX_MARGIN_INFO_MARKETS`] (64) on the caller-list one, so the array cannot grow unbounded
/// under a flat gas price.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccountPositionRow {
    /// The market this row is for.
    pub market_id: u64,
    /// Every reported margin quantity for this `(user, market_id)` — the whole row bar its id.
    pub info: MarginInfo,
}

impl AccountPositionRow {
    /// ABI form of this row.
    ///
    /// **THE ONLY PLACE A ROW IS WRITTEN OUT.** Both surfaces that report one — the bulk
    /// `getAccount().positions[]` and the single-market [`run_get_position_risk`] — end here, so a
    /// field added to `AccountPosition` reaches both or neither. There is no second encoder to keep
    /// in step; there used to be two, and `liquidation_price` had already gone missing from one.
    ///
    /// The VALUES could not drift even then (one [`MarginInfo`] per row), but a field could still be
    /// dropped on the way out, which is what
    /// `margin_view_tests::get_account_positions_agree_with_get_position_risk_field_for_field`
    /// checks — over both CALL PATHS, since the encoder is now shared and only the path differs.
    ///
    /// `liquidationPrice` is read off the same [`MarginInfo`] as everything else, so the bulk path
    /// carries it for zero extra loads and a multi-market backend needs no `1 + N` calls for it.
    pub fn to_abi(&self) -> crate::interface::IPerpDex::AccountPosition {
        let i = &self.info;
        crate::interface::IPerpDex::AccountPosition {
            marketId: self.market_id,
            markPrice: i.mark_price,
            positionAmt: i.position_amt,
            vQuoteBalance: i.v_quote_balance,
            leverage: i.leverage,
            bidNotional: i.bid_notional,
            askNotional: i.ask_notional,
            entryPrice: i.entry_price,
            notional: i.notional,
            unrealizedProfit: i.unrealized_profit,
            isolatedMargin: i.isolated_margin,
            positionInitialMargin: i.position_initial_margin,
            openOrderInitialMargin: i.open_order_initial_margin,
            initialMargin: i.initial_margin,
            maintMargin: i.maint_margin,
            isolatedWallet: i.position_margin,
            liquidationPrice: i.liquidation_price,
        }
    }
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
/// `totalCrossWalletBalance` (our stored `perp_wallet_balance`).
///
/// It is ALSO a stored scalar — [`crate::types::UserAccount::total_position_margin`], maintained by
/// `storage::save_position` — which is what makes the `AccountBalanceChanged` emit path walk-free.
/// This fold still WALKS it, deliberately and on purpose: `getAccount` is loading every position blob
/// anyway (it needs `amount`, `v_quote_balance`, `leverage` and both side aggregates per market), so
/// the walked value is free here, and it is the independent ground truth the `debug_assertions`
/// cross-check in [`index_account_scalars`] compares the stored field against. Do NOT "optimise" this
/// accumulator into a read of the stored field — that would delete the only guard against the stored
/// one drifting. (The since-deleted `total_perp_collateral` "TC" aggregate is a different story; see
/// the note on the stored field for why its recorded objection does not apply to `Σ pos.margin`.)
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
    /// Σ `isolatedWallet` — the silos.
    pub total_position_margin: i128,
}

impl AccountMarginTotals {
    /// Add one market's [`MarginInfo`] to every accumulator. The ONLY place the six terms are
    /// summed, so the in-index and the entering-a-new-market arms of [`fold_account_margin`] cannot
    /// accumulate different sets of fields.
    #[inline]
    fn add(&mut self, info: &MarginInfo) {
        self.total_initial_margin += info.initial_margin as u128;
        self.total_position_initial_margin += info.position_initial_margin as u128;
        self.total_open_order_initial_margin += info.open_order_initial_margin as u128;
        self.total_maint_margin += info.maint_margin as u128;
        self.total_unrealized_profit += info.unrealized_profit as i128;
        self.total_position_margin += info.position_margin as i128;
    }
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
///
/// # `override_market` — pricing the state a caller is ABOUT to write
///
/// Substitutes an IN-MEMORY position for one market id, so a gate can fold the hypothetical
/// post-write account WITHOUT touching storage. Semantics are **verbatim**
/// [`total_open_order_initial_margin`]'s, including the case that matters most: when the overridden
/// id is not yet in the market set — a user ENTERING a market, whose index entry is only written at
/// save time — its term is appended anyway, and a market id that names no market contributes
/// nothing. Every other market's term is read from storage exactly as without the override, which is
/// what makes the two forms comparable within one call.
///
/// ⚠️ **No live caller passes `Some` here today.** The placement gate takes the LEAN
/// [`derived_available_balance_with`] (whose own override, on
/// [`total_open_order_initial_margin`], IS live), and this fold's only remaining callers are the two
/// READ selectors, which price stored state. Kept because it is the one form in which this fold can price
/// an unwritten state, it costs a matched `None` on the live paths, and its
/// entering-a-new-market rule is the subtle half of the pair `derived_available_balance_with`
/// relies on — the two must stay semantically identical, and the `debug_assertions` check in
/// [`index_account_scalars`] is what compares them.
///
/// # `rows` — KEEPING what the fold already computed
///
/// Every iteration builds a complete [`MarginInfo`] and, historically, threw all but six numbers of
/// it away. `Some(sink)` pushes one [`AccountPositionRow`] per folded market instead, in the same
/// order the ids were consumed, so `getAccount` can publish the per-market detail behind its totals
/// for **zero extra loads and zero extra arithmetic** — it is the value that was already in hand.
/// `None` keeps the discard for the callers that only want the sums.
///
/// This is the ONLY sanctioned way to obtain those rows. A second walk that re-derives them would
/// re-open the exact `1 + N` inconsistency `getAccount`'s one-call shape exists to close: two
/// traversals can only be guaranteed to see one state by accident.
pub fn fold_account_margin<H: PerpHost, I: IntoIterator<Item = u64>>(
    context: &mut H,
    user: Address,
    market_ids: I,
    source: MarketSetSource,
    who: &str,
    override_market: Option<(u64, &crate::types::PerpPosition)>,
    mut rows: Option<&mut Vec<AccountPositionRow>>,
) -> Result<AccountMarginTotals, PerpError> {
    let market_ids = market_ids.into_iter();
    let mut totals = AccountMarginTotals::default();
    let (lower, _) = market_ids.size_hint();
    let mut seen: Vec<u64> = Vec::with_capacity(lower);
    let mut applied_override = false;
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
        let info = match override_market {
            Some((id, over)) if id == market_id => {
                applied_override = true;
                compute_margin_info_at(context, market_id, over)
            }
            _ => compute_margin_info(context, user, market_id),
        }
        .map_err(|e| relabel_market(e, who, market_id))?;
        totals.add(&info);
        // Kept, not recomputed: `info` is the value the six Σ terms above were just taken from.
        if let Some(rows) = rows.as_mut() {
            rows.push(AccountPositionRow { market_id, info });
        }
    }
    if let Some((id, over)) = override_market {
        if !applied_override {
            // Entering a market: the index is only written at save time, so the id the caller is
            // pricing is legitimately absent. A market that does not exist contributes nothing
            // (the placement path rejects an unknown market long before this point).
            if let Some(market) = storage::load_market_ref(context, id)? {
                let info = margin_info_of(&market, over).map_err(|e| relabel_market(e, who, id))?;
                totals.add(&info);
                // Appended for the same reason its term is: the row set must name exactly the
                // market set the totals were summed over, override arm included.
                if let Some(rows) = rows.as_mut() {
                    rows.push(AccountPositionRow {
                        market_id: id,
                        info,
                    });
                }
            }
        }
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
///
/// `override_market` is [`fold_account_margin`]'s, unchanged. The stored cross wallet is NOT
/// overridable and does not need to be: every caller that supplies a position override is pricing a
/// hypothetical whose wallet leg has not moved (resting an order escrows nothing), so the stored
/// value IS the post-state one.
///
/// `rows` is [`fold_account_margin`]'s, passed straight through: the scalars and the per-market rows
/// come out of ONE walk, at one state, which is the property `getAccount`'s single-call shape rests
/// on.
pub fn account_margin_scalars<H: PerpHost, I: IntoIterator<Item = u64>>(
    context: &mut H,
    user: Address,
    market_ids: I,
    source: MarketSetSource,
    who: &str,
    override_market: Option<(u64, &crate::types::PerpPosition)>,
    rows: Option<&mut Vec<AccountPositionRow>>,
) -> Result<AccountMarginScalars, PerpError> {
    let totals = fold_account_margin(
        context,
        user,
        market_ids,
        source,
        who,
        override_market,
        rows,
    )?;
    // Signed and unclamped: this is the CROSS wallet exactly as stored. A negative value is a
    // settled receivable (see `types::UserAccount::perp_wallet_balance`), and hiding it behind a
    // `uint64` floor is precisely the blind spot the old `availablePerpBalance` had.
    let total_cross_wallet_balance = storage::load_account_ref(context, user)?.perp_wallet_balance;

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
        total_cross_wallet_balance as i128 + totals.total_position_margin,
        "total wallet balance",
    )?;

    Ok(AccountMarginScalars {
        total_wallet_balance,
        total_cross_wallet_balance,
        total_margin_balance: narrow_i64(
            total_wallet_balance as i128 + total_unrealized_profit as i128,
            "total margin balance",
        )?,
        cross_margin_balance: narrow_i64(
            total_cross_wallet_balance as i128 + total_unrealized_profit as i128,
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
            total_cross_wallet_balance as i128 - total_open_order_initial_margin as i128,
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
/// Binance totalWalletBalance  == our cross + Σ isolatedWallet   ← `getAccount` returns this
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
        None,
        // Scalars only: this selector's return shape is the account block, with no per-market rows.
        None,
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
/// **REST-only.** `getAccount(address)` ([`crate::account::run_get_account`]) is the sole consumer:
/// the seven account-level MARGIN totals in here are Binance `/fapi/v2/account` fields, not
/// `ACCOUNT_UPDATE` fields, and the `AccountBalanceChanged` snapshot no longer carries them (it takes
/// [`index_account_wallet_balances`] instead). The two surfaces still share three fields —
/// `usdcBalance`, `totalWalletBalance`, `totalCrossWalletBalance` — and must report the same numbers
/// for them; that is pinned by
/// `margin_view::tests::the_event_and_get_account_agree_field_for_field_on_the_same_state`,
/// `trading::tests::matched_call_publishes_one_snapshot_per_economic_event`,
/// `trading::tests::account_snapshot_events::a_crossing_fill_publishes_a_snapshot_closing_each_party_s_rows`,
/// and by the `debug_assertions` cross-check inside [`index_account_wallet_balances`], which compares
/// the walk-free producer against this one on every published snapshot — plus the one inside
/// [`index_account_scalars`], which compares this fold's `Σ pos.margin` against the stored aggregate
/// the event reads.
#[derive(Clone, Debug)]
pub struct IndexAccountView {
    /// Spot / withdrawal-layer USDC held inside the DEX. NOT part of any total below.
    pub usdc_balance: primitives::U256,
    /// The account-level margin scalars, all signed-and-unclamped where the quantity can be
    /// negative.
    pub scalars: AccountMarginScalars,
    /// One row per market in the per-user index (`umkt`), in index order — the market set
    /// [`Self::scalars`] was summed over, together with the per-market numbers it was summed FROM.
    ///
    /// These are the fold's own `MarginInfo`s, kept rather than recomputed, so they are the same
    /// state as the scalars beside them by construction — not by a second traversal happening to
    /// land on the same block. That is the point of the shape: a backend assembling a Binance-style
    /// `/account` from `getAccount` + N × `getPositionRisk` was reading `N + 1` states, and
    /// `totalWalletBalance == totalCrossWalletBalance + Σ isolatedWallet` could then fail on a
    /// healthy account with no way to tell a race from a bug.
    ///
    /// SUPERSEDES the `market_ids` field this struct used to carry (and the `uint64[] marketIds`
    /// return it fed): each row names its own market, so the id list is a projection of this one.
    pub positions: Vec<AccountPositionRow>,
}

/// Fold [`account_margin_scalars`] over the user's whole market index — the account-level scalar
/// half of [`IndexAccountView`], and **the producer of the `getAccount` payload.**
///
/// Pure read: every loader it reaches is a `_ref`/cache-fill reader, so it enters no key into the
/// block delta and cannot move the block commitment. It is therefore safe to call from a WRITE
/// path — it observes state, it does not touch it. (Nothing on a write path calls it any more: the
/// account snapshot took [`index_account_wallet_balances`] when its payload narrowed to the three
/// balances. In debug builds that function calls back into this one purely to cross-check them.)
///
/// Cost: one index load plus, per member market, `{market, position}` — **exactly 2 loads, with no
/// shape-dependent worst case.** It was up to 4 (`+ MarketHot + sell list`) while `Ask` was re-folded
/// from the sell list at a read-time `T`; both of those disappeared with the R12 freeze, since both
/// aggregates now sit in the position blob. Bounded by `MAX_USER_MARKETS` (16) ⇒ ≤ 33 `_ref` loads,
/// flat.
///
/// That walk is why this is the right home for the stored-aggregate cross-check below: `getAccount`
/// is loading every position blob regardless, so re-deriving `Σ pos.margin` from it costs nothing,
/// and it is the only independent ground truth for the field the event is read from.
///
/// Its live caller passes `override_market: None` — `getAccount` folds the SETTLED store. See
/// [`fold_account_margin`] for what the override means and why it is kept.
///
/// `rows` is that same fold's row sink, threaded through unchanged. `getAccount` passes `Some`, and
/// that is the whole of how `positions[]` is produced: the rows and the scalars leave this ONE walk
/// together, so they are necessarily the same state.
pub fn index_account_scalars<H: PerpHost>(
    context: &mut H,
    user: Address,
    who: &str,
    override_market: Option<(u64, &crate::types::PerpPosition)>,
    rows: Option<&mut Vec<AccountPositionRow>>,
) -> Result<AccountMarginScalars, PerpError> {
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
        override_market,
        rows,
    )?;

    // The roll-up's `Σ ooIM` and the LEAN admission gate's must be the SAME number over the same
    // market set. They share `position_open_order_margin` per market but fold in two places: this
    // wide fold, and `total_open_order_initial_margin`, which the taker gates / `finalize_compute`
    // still use because they want only `available` and would otherwise pay for five accumulators
    // they discard. `rest_in_book`'s placement gate is one of those lean callers,
    // so this is the check that keeps the number IT enforces equal to the number `getAccount` and
    // `AccountBalanceChanged` report. Pinned on EVERY produced snapshot rather than trusted.
    // Compiled out in release, so the second walk costs production nothing.
    #[cfg(debug_assertions)]
    {
        let gate = derived_available_balance_with(context, user, None, override_market)?;
        debug_assert_eq!(
            gate, scalars.available_balance as i128,
            "{who}: availableBalance must equal the admission gate's own basis"
        );
    }

    // ── The stored `Σ pos.margin` against a FRESH WALK of the user's positions ────────────────
    //
    // `AccountBalanceChanged` publishes `totalWalletBalance` off the STORED
    // `UserAccount::total_position_margin` (one account load, no walk). A stored aggregate can drift
    // where the derived one it replaced could not, so this is where it is checked: `scalars` above came
    // from the wide fold, which sums `info.position_margin` — i.e. `pos.margin` read back from each
    // position blob — so `total_wallet_balance − total_cross_wallet_balance` IS the independent walk,
    // and it must equal the field `storage::save_position` maintains.
    //
    // This is the natural home for the guard: `getAccount` walks anyway, so the check is free here,
    // and `index_account_wallet_balances` calls into this function on every published snapshot, which
    // puts the guard on the write path too. Any `pos.margin` write that ever skips the maintenance in
    // `save_position` therefore blows up in the test suite instead of silently mis-reporting a balance.
    //
    // Skipped when an override is in play: `override_market` deliberately substitutes a HYPOTHETICAL
    // position for one market, so the fold is *supposed* to disagree with what is stored. (No live
    // caller passes `Some`; see `fold_account_margin`.) The walk is over the INDEX while the stored
    // field is over ALL markets — equal because a market can only leave the index while its position
    // is flat, and a flat position holds no margin (asserted at every `save_position`).
    #[cfg(debug_assertions)]
    if override_market.is_none() {
        let stored = storage::load_account_ref(context, user)?.total_position_margin;
        debug_assert_eq!(
            stored as i128,
            scalars.total_wallet_balance as i128 - scalars.total_cross_wallet_balance as i128,
            "{who}: stored total_position_margin for {user} diverged from Σ pos.margin walked over \
             the per-user market index — a pos.margin write bypassed save_position's maintenance"
        );
    }

    Ok(scalars)
}

/// The three balances the `AccountBalanceChanged` snapshot publishes — Binance's `B[]` leg, and the
/// whole of it.
///
/// Deliberately NOT a projection of [`AccountMarginScalars`]: see
/// [`index_account_wallet_balances`] for why the narrow payload gets its own, walk-free producer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountWalletBalances {
    /// Spot / withdrawal-layer USDC held inside the DEX. Our extension; not a Binance stream field.
    pub usdc_balance: primitives::U256,
    /// Binance `wb` — GROSS perp wallet = `total_cross_wallet_balance + Σ pos.margin`.
    pub total_wallet_balance: i64,
    /// Binance `cw` — the stored `perp_wallet_balance`, verbatim, signed and unclamped.
    pub total_cross_wallet_balance: i64,
}

/// [`AccountWalletBalances`] built from an IN-MEMORY working copy instead of the settled store.
///
/// Three emit points need this shape, and they all need it for the same reason: they publish the
/// account header for a state the store does not hold yet, because the position/account writes that
/// would make it true come later (or, on the match path, at the flush).
///
/// * `trading::settlement::UserWork::wallet_balances` — a maker mid-sweep;
/// * `funding::compute_funding_settlement` — funding moves `pos.margin` in memory only;
/// * `trading::liquidation::run_adl`'s LOSER leg — its position/account are written once after the
///   fill loop.
///
/// The awkward term is `Σ pos.margin`. It is [`crate::types::UserAccount::total_position_margin`],
/// owned exclusively by `storage::save_position` and deliberately CLOBBERED from the store by
/// `storage::save_account`, so the field on an owned copy is not a value anything may read. What the
/// callers hold instead is `other_market_position_margin` — the stored aggregate MINUS this market's
/// stored `pos.margin`, captured in the one breath where both are in hand and both are still
/// pre-mutation. That difference is invariant for as long as only this market's position moves, so
/// `base + live pos.margin` is the Σ the eventual `save_position` will store.
///
/// Its convergence on the settled store is not argued but checked: `MatchRegistry::flush` compares
/// this formula's output against `index_account_wallet_balances` for every flushed user in
/// `debug_assertions` builds.
pub(crate) fn wallet_balances_from_parts(
    account: &crate::types::UserAccount,
    other_market_position_margin: i64,
    position_margin: i64,
) -> Result<AccountWalletBalances, PerpError> {
    let total_position_margin = other_market_position_margin
        .checked_add(position_margin)
        .ok_or_else(|| perp_err("account snapshot: Σ position margin overflow"))?;
    let total_cross_wallet_balance = account.perp_wallet_balance;
    let total_wallet_balance = total_cross_wallet_balance
        .checked_add(total_position_margin)
        .ok_or_else(|| perp_err("account snapshot: total wallet balance exceeds i64"))?;
    Ok(AccountWalletBalances {
        usdc_balance: account.usdc_balance.clone().into(),
        total_wallet_balance,
        total_cross_wallet_balance,
    })
}

/// **The single producer of the `AccountBalanceChanged` payload.** ONE account load and an
/// addition — **no index load, no position loads, no walk at all.**
///
/// # Why this is not `index_account_scalars` with fields thrown away
///
/// The event carries three balances, and all three are STORED SCALARS on the one account blob:
/// `usdcBalance` and `cw` (`perp_wallet_balance`) directly, and `wb − cw = Σ pos.margin` as the
/// incrementally maintained [`crate::types::UserAccount::total_position_margin`] (kept up to date by
/// `storage::save_position`, from a delta it computes out of a position read it was already paying
/// for). So the emit path needs neither the per-user market index, nor any position blob, nor the
/// `Market` (no mark, no `base/price_decimals`, no tier table), nor any of the arithmetic the wide
/// fold exists for:
///
/// ```text
///                        wide fold (getAccount)        this producer (the event)
///   loads                1 index + 2/market            1 account, flat
///   maintenance tier     ≤8-band walk                  —
///   unrealized PnL       calc_value_i64 + add          —
///   ooIM                 open_order_margin (2 divs)    —
/// ```
///
/// At `MAX_USER_MARKETS` = 16 that is **1** `_ref` load against ≤ 33, with the per-market arithmetic
/// gone entirely. It was ≤ 18 while `Σ pos.margin` was walked off the index. Projecting the wide fold
/// down would have preserved the payload and thrown all of that away.
///
/// It also has strictly FEWER ways to fail than the wide fold, which matters because it runs on a
/// WRITE path: the six narrowing guards, the per-market `compute_margin_info` rejects and the
/// "user market index holds unknown market" invariant are all gone from here (`getAccount` still
/// checks every one of them). Only `total_wallet_balance` can still narrow-fail, and only at
/// Σ ≥ 9.2e12 USD.
///
/// # The one thing the stored aggregate costs: it can DRIFT
///
/// A walked Σ cannot be wrong; a stored one can be stale if a `pos.margin` write ever skips its
/// maintenance. Two things contain that. (1) There is exactly ONE maintenance point, because
/// `storage::save_position` is the single door for every `pos.margin` write. (2)
/// [`index_account_scalars`] re-walks the user's positions in `debug_assertions` builds and compares
/// the walk against the stored field — and the cross-check just below calls into it on every
/// published snapshot, so the guard runs on this path too, not only where a test happens to call
/// `getAccount`.
///
/// Pure read: the one loader is a `_ref`/cache-fill reader, so it enters no key into the block delta
/// and cannot move the block commitment.
pub fn index_account_wallet_balances<H: PerpHost>(
    context: &mut H,
    user: Address,
    who: &str,
) -> Result<AccountWalletBalances, PerpError> {
    let (usdc_balance, total_cross_wallet_balance, total_position_margin) = {
        let a = storage::load_account_ref(context, user)?;
        // ONE account load for all three. The wide path pays an index load plus two per market on top
        // of it.
        let usdc: primitives::U256 = a.usdc_balance.clone().into();
        (usdc, a.perp_wallet_balance, a.total_position_margin)
    };
    let total_wallet_balance = total_cross_wallet_balance
        .checked_add(total_position_margin)
        .ok_or_else(|| perp_err(format!("{who}: total wallet balance exceeds i64")))?;

    // The event and `getAccount` must report the SAME number for every field they share, and this is
    // where the stored aggregate and the wide fold's WALK could drift (that is now the substance of
    // this check: the event reads `UserAccount::total_position_margin`, `getAccount` sums
    // `pos.margin` over the index — see the cross-check inside `index_account_scalars`, which
    // compares the two directly). Compiled out in release, so the lean path costs production nothing;
    // in debug it runs on EVERY published snapshot rather than only where a test happens to call
    // `getAccount`.
    //
    // `Ok(..)` guard, not `unwrap`: the wide fold has narrowing guards and per-market rejects this
    // one does not need, and a state that trips them is exactly a state in which `getAccount` itself
    // reverts — there is then no published number to disagree with.
    #[cfg(debug_assertions)]
    if let Ok(wide) = index_account_scalars(context, user, who, None, None) {
        debug_assert_eq!(
            (total_wallet_balance, total_cross_wallet_balance),
            (wide.total_wallet_balance, wide.total_cross_wallet_balance),
            "{who}: the lean snapshot fold and the wide getAccount fold disagree for {user}"
        );
    }

    Ok(AccountWalletBalances {
        usdc_balance,
        total_wallet_balance,
        total_cross_wallet_balance,
    })
}

/// [`index_account_scalars`] paired with the spot USDC balance and the fold's per-market rows — the
/// complete published account view, as `getAccount` returns it.
///
/// **ONE walk.** The rows are captured by that same [`index_account_scalars`] call, not gathered by
/// a second pass, so this function adds no load over the scalars alone. (It also no longer loads the
/// user market index for itself: the id list it used to echo is now carried by the rows, one id per
/// row, so that read went away with the field.)
pub fn index_account_view<H: PerpHost>(
    context: &mut H,
    user: Address,
    who: &str,
) -> Result<IndexAccountView, PerpError> {
    // `MAX_USER_MARKETS` is the hard bound on the index, so this allocates exactly once.
    let mut positions = Vec::with_capacity(crate::types::MAX_USER_MARKETS);
    let scalars = index_account_scalars(context, user, who, None, Some(&mut positions))?;
    let usdc_balance: primitives::U256 = storage::load_account_ref(context, user)?
        .usdc_balance
        .clone()
        .into();

    Ok(IndexAccountView {
        usdc_balance,
        scalars,
        positions,
    })
}

/// Re-label a per-market reject so the caller can tell WHICH id in the market set failed
/// (`compute_margin_info` only knows it is "marginInfo: unknown market"). Fatals and the
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
