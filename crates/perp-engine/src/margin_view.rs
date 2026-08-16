//! Derived margin read layer — Binance-shaped margin reporting, computed on demand.
//!
//! # What this is
//!
//! Binance USDⓈ-M stores only a small ledger (`walletBalance`, per-position `isolatedWallet`,
//! `positionAmt`, `entryPrice`) and **derives** every margin quantity on read. We instead
//! **store** five quantities Binance derives — `margin_reserved`, `margin_reserved_notional`,
//! `buy/sell_side_margin_reserved`, `buy/sell_side_reserved_notional` — and physically debit
//! the wallet for them at placement.
//!
//! This module does **not** change that. It is a **pure read layer** that reports the
//! Binance-shaped numbers alongside ours, so that
//!
//! 1. integrators get the fields they actually compare against, and
//! 2. we can MEASURE the gap between Binance's formulas and our escrow before deciding
//!    whether to restructure enforcement.
//!
//! It stores nothing, moves no money, and changes no execution rule. Every loader it calls is
//! one of the `_ref` (cache-fill, never dirty-mark) readers, so no key enters the block delta
//! and the commitment is untouched.
//!
//! # Formula source
//!
//! `misc/binance-margin-verified-model.md` §1.1 (the formula set) and §2 (the rounding model),
//! plus `misc/binance-v3-account-balance-field-reference.md` §2/§4 (field-by-field, with the
//! discriminating mainnet samples). Every rounding mode below cites the evidence that settled
//! it. Two behaviours of Binance's are deliberately **not** copied — see
//! [`run_get_account_margin`] (`availableBalance` is not clamped at zero) and the ABI comment
//! on `getMarginInfo` (the inputs are returned, so the outputs are locally checkable).

use alloy_sol_types::SolCall;
use primitives::{Address, Bytes};

use crate::host::PerpHost;
use crate::{
    errors::perp_err,
    interface::IPerpDex::{
        getAccountMarginCall, getAccountMarginReturn, getMarginInfoCall, getMarginInfoReturn,
    },
    math::{
        calc_value_i64, checked_u64_to_i64, maintenance_margin, open_order_margin,
        open_order_margin_at_leverage,
    },
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
    /// `Bid` — Σ `qty × LIMIT price` over the user's resting BUYS in this market.
    pub bid_notional: u64,
    /// `Ask` — the same over resting SELLS.
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
    /// `open_order_initial_margin` recomputed with the leverage TIER-CAPPED at the combined
    /// notional ([`math::open_order_margin`]). NOT part of the `getMarginInfo` ABI — Binance
    /// reports its numbers at the position's own `leverage`, and this read layer reports
    /// Binance's numbers. This field exists for the derived-ooIM ADMISSION path, which is the
    /// one place the cap belongs: see [`total_open_order_initial_margin`].
    ///
    /// Equal to `open_order_initial_margin` whenever the position's leverage is already within
    /// the combined notional's tier — which is the common case, since `setLeverage` caps against
    /// the tier table at the time it is called.
    pub open_order_initial_margin_tier_capped: u64,
    /// Maintenance margin at `N` under this market's tier table.
    pub maint_margin: u64,
    // ── ours, for comparison ──
    /// What our engine has ACTUALLY escrowed for this position's resting orders.
    pub margin_reserved_actual: u64,
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
/// `Bid`/`Ask` come from the maintained per-side aggregates on `pos` (proven equal to the
/// resting-order fold; see [`compute_margin_info`]), `N` from `pos.amount` at `market.mark_price`,
/// and `L` from `pos.leverage`.
///
/// Returns both leverage variants because they are wanted by different callers and share every
/// input: `binance` is at the position's own leverage (what the read path reports),
/// `tier_capped_oo_im` re-derives ooIM with the tier cap applied at the combined notional (what
/// the admission path enforces).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PositionDerivedMargin {
    /// `N` — signed position notional at MARK, truncated toward zero.
    pub signed_notional: i64,
    /// `Bid` — Σ (remaining qty × LIMIT price) over resting buys.
    pub bid_notional: u64,
    /// `Ask` — the same over resting sells.
    pub ask_notional: u64,
    /// PIM / IM / ooIM at the position's own leverage, UNCAPPED by the tier table.
    pub binance: crate::math::OpenOrderMargin,
    /// ooIM with the leverage tier-capped at the combined notional.
    pub tier_capped_oo_im: u64,
}

/// Compute [`PositionDerivedMargin`] for one `(market, position)`. Pure function, no storage.
pub fn position_derived_margin(
    market: &crate::types::Market,
    pos: &crate::types::PerpPosition,
) -> Result<PositionDerivedMargin, PerpError> {
    let leverage = pos.leverage.max(1);
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
        binance: open_order_margin_at_leverage(
            signed_notional,
            bid_notional,
            ask_notional,
            leverage,
        )?,
        tier_capped_oo_im: open_order_margin(
            &market.tiers,
            signed_notional,
            bid_notional,
            ask_notional,
            leverage,
        )?
        .open_order_initial_margin,
    })
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
    // `PerpPosition::set_reservations` floors leverage at 1; do the same here so a defaulted or
    // corrupt 0 divides as 1 rather than trapping.
    let leverage = pos.leverage.max(1);

    // ── Bid / Ask ────────────────────────────────────────────────────────────────────────
    // `Bid = Σ (remaining buy qty × that order's LIMIT price)` — the LIMIT price, not the mark —
    // and `Ask` the same over sells. Read O(1) off the maintained per-side aggregates
    // (`total_buy_notional` / `total_sell_notional`) rather than re-folding the lists: those
    // aggregates ARE Bid/Ask by construction (same per-order-floored `calc_value` terms), and the
    // property test `side_aggregates_are_exactly_bid_and_ask_after_every_operation` proves it
    // holds after every transition, with the list fold as the independent ground truth. The
    // `debug_assert` below keeps the fold as a live oracle at zero release cost.
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
             resting-order fold"
        );
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
    // The formula itself lives in `math::open_order_margin_at_leverage` — ONE implementation,
    // shared verbatim with the derived-ooIM admission path (`total_open_order_initial_margin`
    // below). Everything above this line is this layer's job: turning storage into the four pure
    // inputs `(N, Bid, Ask, L)`.
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
    // `N` is passed SIGNED. The doc's samples are all LONGS, where signed == the unsigned
    // `notional` field, and it lists short-side signs as unverified (§6). The signed reading is
    // the one the branch SEMANTICS force ("多头暴露 / 空头暴露"): for a short, unsigned `N` would
    // make `|N − Ask|` understate the very exposure the ask branch exists to measure.
    //
    // NOTE this deliberately mixes bases: `N` is at MARK, `Bid`/`Ask` are at each order's LIMIT
    // price. That is Binance's formula, and this layer reports Binance's numbers.
    //
    // At the position's OWN leverage, deliberately UNCAPPED by the tier table: Binance derives
    // `initialMargin` from the position's `leverage` field, so applying the cap here would make
    // the reported number stop being Binance's. The tier-capped variant is computed separately
    // below for the admission path.
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
    let position_initial_margin = derived.binance.position_initial_margin;
    let initial_margin = derived.binance.initial_margin;
    let open_order_initial_margin = derived.binance.open_order_initial_margin;
    let open_order_initial_margin_tier_capped = derived.tier_capped_oo_im;

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
        open_order_initial_margin_tier_capped,
        maint_margin,
        margin_reserved_actual: pos.margin_reserved,
        position_margin: pos.margin,
    })
}

// ── Derived-ooIM admission basis (Phase 1: computed, not enforced) ───────────────────────────

/// `Σ_markets ooIM` for `user`, over the per-user market index — the account-level open-order
/// requirement on the DERIVED basis.
///
/// Enumerates exactly the markets the user is active in (non-zero position OR at least one
/// resting order), which is precisely the support of the sum: a market the user has left
/// contributes `N = Bid = Ask = 0` ⇒ `ooIM = 0`. That is what the Phase 0 index was built for —
/// there is no other way to enumerate a user's markets, and walking every market on the exchange
/// would be unbounded.
///
/// Uses the TIER-CAPPED variant ([`math::open_order_margin`]): this is an enforcement quantity,
/// so a user must not be able to buy a lower requirement by holding a leverage the combined
/// notional's tier no longer permits. The Binance-parity read path deliberately reports the
/// uncapped one; see [`MarginInfo::open_order_initial_margin_tier_capped`].
///
/// Pure read — every loader it reaches is a `_ref` (cache-fill, never dirty-mark) reader, so it
/// enters no key into the block delta and cannot move the commitment. Returns `u128` so the fold
/// cannot overflow before the caller compares it.
pub fn total_open_order_initial_margin<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<u128, PerpError> {
    let markets = storage::load_user_markets_ref(context, user)?;
    let mut total: u128 = 0;
    for market_id in markets.iter().copied() {
        // The index is a set, so no id repeats and no term is double-counted.
        total += compute_margin_info(context, user, market_id)?
            .open_order_initial_margin_tier_capped as u128;
    }
    Ok(total)
}

/// `Σ_markets pos.margin_reserved` for `user` — the account-level escrow that has ALREADY been
/// physically debited from `perp_wallet_balance`, over the same index.
///
/// This is the gross-up term. `margin_reserved` is exactly what `debit_perp` moved: placement
/// debits the DELTA of `margin_reserved` and cancel/fill credits it back, so the running sum of
/// those deltas is the current value of the field. It is the FLIP-AWARE combined reservation
/// `c_notional / leverage` (`max(S + B', B + S')`), NOT the sum of the two per-side fields —
/// `buy_side_margin_reserved` and `sell_side_margin_reserved` are informational and are used only
/// as a cancel-ordering heuristic (see `PerpPosition::set_reservations`). Summing those instead
/// would over-count a two-sided book.
pub fn total_margin_reserved<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<u128, PerpError> {
    let markets = storage::load_user_markets_ref(context, user)?;
    let mut total: u128 = 0;
    for market_id in markets.iter().copied() {
        total += storage::load_position_ref(context, user, market_id)?.margin_reserved as u128;
    }
    Ok(total)
}

/// The user's available balance on the DERIVED basis, grossed up out of the escrow basis.
///
/// # The double-count trap
///
/// During Phase 1 the escrow is STILL ACTIVE, so `perp_wallet_balance` has ALREADY been debited
/// by every reservation. `perp_wallet_balance − Σ ooIM` would therefore subtract the open-order
/// requirement TWICE — once physically (escrow), once arithmetically (derived). The escrow must
/// be added back first:
///
/// ```text
/// wallet_gross  = perp_wallet_balance + Σ_markets pos.margin_reserved
/// available_new = wallet_gross − Σ_markets ooIM
/// ```
///
/// `wallet_gross` is the analogue of Binance's `crossWalletBalance`; `available_new` of their
/// `availableBalance`. In Phase 2, when the escrow is deleted, the gross-up term becomes
/// identically zero and this collapses to `perp_wallet_balance − Σ ooIM`.
///
/// Signed and unclamped, in `i128`: the comparison the caller makes is against a `u64`
/// requirement, and clamping at zero would hide exactly the under-coverage the gate exists to
/// detect.
pub fn derived_available_balance<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<i128, PerpError> {
    let wallet = storage::load_account_ref(context, user)?.perp_wallet_balance as i128;
    let gross = wallet + total_margin_reserved(context, user)? as i128;
    Ok(gross - total_open_order_initial_margin(context, user)? as i128)
}

/// `ooIM(after) − ooIM(before)` for one market on the TIER-CAPPED basis — the marginal derived
/// requirement of an operation that changes a position's resting-order aggregates.
///
/// Signed: the tier cap can lower the effective leverage as the combined notional grows, which
/// raises PIM as well as IM, so `ooIM = IM − PIM` is not monotone in `Bid`/`Ask` in general.
#[cfg(debug_assertions)]
pub fn derived_requirement_delta(
    market: &crate::types::Market,
    before: &crate::types::PerpPosition,
    after: &crate::types::PerpPosition,
) -> Result<i128, PerpError> {
    Ok(
        position_derived_margin(market, after)?.tier_capped_oo_im as i128
            - position_derived_margin(market, before)?.tier_capped_oo_im as i128,
    )
}

/// **Phase 1 dual gate.** Compare the DERIVED admission basis against the live ESCROW check at
/// one admission point, and panic if they disagree for a reason that is not one of the two
/// characterised mechanisms.
///
/// # What this asserts, and why it is not simply "the two agree"
///
/// It is not, because they do not. The escrow and the derived requirement are DIFFERENT
/// FUNCTIONS of the same state, and a suite-wide census (5099 gate evaluations across the whole
/// test suite) measured them disagreeing on the DECISION in 238 of them (4.7%), and on the
/// QUANTITIES far more often: 1499/5099 (29%) price the same operation differently, and
/// 1249/5099 (25%) have `Σ margin_reserved != Σ ooIM`. Asserting plain equality would fail 9 of
/// the 386 existing tests. Those failures are findings, not test bugs — see the
/// `derived_ooim_divergence` module for each one pinned with concrete numbers.
///
/// So what is asserted is the one claim that IS universal, and that is exactly the claim that
/// catches a bug in this probe or in the formula:
///
/// > **If the two bases price the operation identically AND hold the same available balance,
/// > they MUST reach the same decision.**
///
/// A disagreement outside those two escape hatches would mean the gate arithmetic itself is
/// wrong — a third mechanism that nothing in the model predicts. The census confirms it never
/// happens (0/5099).
///
/// The two sanctioned mechanisms, both of which Phase 2 changes DELIBERATELY:
///
/// 1. **Marginal-charge divergence** (`escrow_requirement != derived_requirement`, 29% of
///    evaluations). Our escrow prices an order by the change in the flip-aware worst-case
///    reservation `max(S + B', B + S')`, computed in QUANTITY space against the position and
///    evaluated at the orders' LIMIT prices. Binance prices it by the change in
///    `max(|N + Bid|, |N − Ask|)`, computed in NOTIONAL space with the position leg at MARK.
///    Different functions; they coincide only by accident.
/// 2. **Available-stock divergence** (`Σ margin_reserved != Σ ooIM`, 25%). The same mismatch
///    integrated over the account's whole book, plus the fact that the escrow is FROZEN at
///    placement while ooIM re-values the position leg at the CURRENT mark on every read.
///
/// # The gross-up (the trap this function exists to get right)
///
/// The escrow is still active, so `perp_wallet_balance` has ALREADY been debited by every
/// reservation. The derived available must add it back before subtracting Σ ooIM, or the
/// open-order requirement is charged twice — see [`derived_available_balance`].
///
/// # Why the requirement is comparable across the two bases
///
/// The escrow move at every admission point is wallet → escrow (or wallet → position margin /
/// out of the system), so `wallet_gross = wallet + Σ margin_reserved` is INVARIANT across an
/// order placement. Hence `available_new_before >= Δ ooIM` is exactly `available_new_after >= 0`,
/// the same shape as `wallet >= delta` ⟺ `wallet_after >= 0` on the escrow basis. The two gates
/// are therefore answering the same question about the same operation, on two different bases.
///
/// The ESCROW check alone still decides accept/reject: this function only observes. Behaviour in
/// Phase 1 is unchanged BY CONSTRUCTION, so the golden commitment is unchanged by construction
/// rather than by hope.
///
/// Debug builds only, and `#[cfg]`-gated (not `if cfg!(…)`) so neither the function nor the
/// argument expressions at the call sites exist in a release build.
#[cfg(debug_assertions)]
pub fn debug_assert_gates_agree<H: PerpHost>(
    context: &mut H,
    user: Address,
    site: &str,
    market_id: Option<u64>,
    escrow_requirement: u64,
    derived_requirement: i128,
) {
    let account = storage::load_account_ref(context, user).expect("dual gate: load account");
    let escrow_available = account.perp_wallet_balance;
    let escrow_ok = account.has_available_perp(escrow_requirement);

    let escrow_sum = total_margin_reserved(context, user).expect("dual gate: Σ margin_reserved");
    let oo_im_sum = total_open_order_initial_margin(context, user).expect("dual gate: Σ ooIM");
    let derived_available = escrow_available as i128 + escrow_sum as i128 - oo_im_sum as i128;
    let derived_ok = derived_available >= derived_requirement;

    if escrow_ok == derived_ok {
        return;
    }
    // Characterised mechanism 1: the two bases priced the operation differently.
    if escrow_requirement as i128 != derived_requirement {
        return;
    }
    // Characterised mechanism 2: the two bases hold a different available.
    if escrow_sum != oo_im_sum {
        return;
    }
    panic!(
        "derived-ooIM dual gate: UNCHARACTERISED divergence at {site}\n           The two bases priced this operation IDENTICALLY and hold the SAME available, yet reached \n           opposite decisions. No mechanism in the model predicts this — the gate arithmetic is wrong.\n           user               = {user}\n           market             = {market_id:?}\n           escrow available   = {escrow_available} (perp_wallet_balance)\n           escrow requirement = {escrow_requirement}  -> {}\n           derived available  = {derived_available} (= {escrow_available} + Σmr {escrow_sum} - ΣooIM {oo_im_sum})\n           derived requirement= {derived_requirement}  -> {}",
        if escrow_ok { "ACCEPT" } else { "REJECT" },
        if derived_ok { "ACCEPT" } else { "REJECT" },
    );
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
            marginReservedActual: info.margin_reserved_actual,
            positionMargin: info.position_margin,
        },
    )))
}

/// `getAccountMargin(address user, uint64[] marketIds) returns (...)`.
///
/// Folds [`compute_margin_info`] over `marketIds` (duplicates counted once, calldata order
/// preserved — no map iteration anywhere, so the output is deterministic) and adds the two
/// account-level identities.
///
/// # The two deliberate departures from Binance
///
/// **`availableBalance` is NOT clamped at zero.** Binance's is: it was measured reporting
/// `0.00000000` where the true value was `−0.00085981`, and the reference doc's own verdict is
/// that you therefore **cannot use it to tell whether an account is under-covered**. Ours is
/// `int64` and is allowed to go negative — strictly more information, at no cost. (A negative
/// value is not an error state and triggers nothing: like Binance, we do not tear resting
/// orders down mid-life, and this layer could not write anything if it wanted to.)
///
/// **`walletBalance` is the INNERMOST of Binance's three nested balances, not the outermost.**
/// Binance keeps `walletBalance` (gross) and `crossWalletBalance = walletBalance − Σ
/// isolatedWallet` as ledger state, and derives `availableBalance = crossWalletBalance − Σ
/// ooIM` on read — the open-order requirement is never debited from anything there. We keep one
/// balance, and BOTH subtractions have already been physically applied to it: position margin at
/// open, and the `margin_reserved` delta at placement (credited back at cancel/fill). So our
/// `walletBalance` is the analogue of their `availableBalance`, their `crossWalletBalance` is
/// `walletBalance + Σ marginReservedActual`, and their gross `walletBalance` is that plus
/// Σ `positionMargin`.
///
/// **Consequence, stated plainly: `availableBalance` here subtracts the open-order requirement
/// TWICE** — once physically inside `walletBalance` on OUR escrow basis, once arithmetically on
/// BINANCE's `ooIM` basis. It is not spendable headroom (that is `walletBalance` itself); it is
/// Binance's formula evaluated against our ledger, which is the point of this layer. The
/// like-for-like reconstruction is `walletBalance + Σ marginReservedActual −
/// totalOpenOrderInitialMargin`, and `Σ marginReservedActual − totalOpenOrderInitialMargin` is
/// itself the measured gap between Binance's requirement and our escrow. `marginBalance` carries
/// the same caveat (Binance's is built on their gross wallet, ours on the innermost one).
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

    // Signed/wide accumulators: each per-market term is bounded by u64/i64, and the id list is
    // bounded by MAX_MARGIN_INFO_MARKETS, so i128/u128 cannot overflow here — the narrowing
    // conversions at the end are where a pathological state surfaces as a clean revert.
    let mut total_initial_margin: u128 = 0;
    let mut total_position_initial_margin: u128 = 0;
    let mut total_open_order_initial_margin: u128 = 0;
    let mut total_maint_margin: u128 = 0;
    let mut total_unrealized_profit: i128 = 0;
    // OUR escrow sum, so the account level is self-contained: a client can reconstruct the
    // Binance-basis gross wallet without also fetching every per-market getMarginInfo.
    let mut total_margin_reserved: u128 = 0;

    let user = args.user;
    let mut seen: Vec<u64> = Vec::with_capacity(args.marketIds.len());
    for market_id in args.marketIds.iter().copied() {
        // A repeated id would double-count every total; fold it once. O(n^2) over n <= 64.
        if seen.contains(&market_id) {
            continue;
        }
        seen.push(market_id);
        let info = compute_margin_info(context, user, market_id)
            .map_err(|e| relabel(e, market_id))?;
        total_initial_margin += info.initial_margin as u128;
        total_position_initial_margin += info.position_initial_margin as u128;
        total_open_order_initial_margin += info.open_order_initial_margin as u128;
        total_maint_margin += info.maint_margin as u128;
        total_unrealized_profit += info.unrealized_profit as i128;
        total_margin_reserved += info.margin_reserved_actual as u128;
    }

    // `walletBalance` SIGNED and unclamped — `getAccount`'s `availablePerpBalance` floors a
    // negative internal balance at 0; here the sign is the point.
    let wallet_balance = storage::load_account_ref(context, user)?.perp_wallet_balance;

    let total_unrealized_profit = i64::try_from(total_unrealized_profit)
        .map_err(|_| perp_err("getAccountMargin: total unrealized profit exceeds i64"))?;
    let total_open_order_initial_margin = u64::try_from(total_open_order_initial_margin)
        .map_err(|_| perp_err("getAccountMargin: total open-order initial margin exceeds u64"))?;

    // `marginBalance = walletBalance + totalUnrealizedProfit` (exact on 22/22 mainnet snapshots).
    let margin_balance = (wallet_balance as i128) + (total_unrealized_profit as i128);
    // `availableBalance = walletBalance − totalOpenOrderInitialMargin`. NOT clamped at zero
    // (Binance clamps; the doc's own verdict is that theirs is therefore useless for detecting
    // under-coverage). See the doc comment above for why this double-subtracts the open-order
    // requirement and what the like-for-like Binance reconstruction is.
    // `availableBalance` IS `perp_wallet_balance`, with no further subtraction.
    //
    // Binance keeps three nested balances — `walletBalance` ⊃ `crossWalletBalance` ⊃
    // `availableBalance` — and never debits the open-order requirement from any of them; their
    // `availableBalance` is derived by subtracting it. We keep exactly ONE balance and it is the
    // INNERMOST: `place_order` physically debits the reservation (`trading/mod.rs:2173`
    // `debit_perp(delta)`) and credits it back on cancel/fill, and the position allocation was
    // already moved out at fill. So our wallet is spendable headroom by construction, and
    // subtracting `totalOpenOrderInitialMargin` here would charge the open-order requirement
    // TWICE — once physically on our escrow basis, once arithmetically on Binance's.
    //
    // The Binance-basis quantities are still reported, as requirements, for comparison:
    //   their crossWalletBalance ≈ walletBalance + totalMarginReserved
    //   like-for-like available  = walletBalance + totalMarginReserved − totalOpenOrderInitialMargin
    // and the delta between that and `walletBalance` is exactly the escrow-vs-requirement
    // divergence (see the divergence test).
    //
    // NOT clamped at zero, deliberately: the reference doc measured Binance reporting
    // `0.00000000` where the true value was `−0.00085981` and concluded the field therefore
    // cannot be used to detect under-coverage. Ours is signed and stays signed.
    let available_balance = wallet_balance as i128;

    Ok(Bytes::from(getAccountMarginCall::abi_encode_returns(
        &getAccountMarginReturn {
            walletBalance: wallet_balance,
            marginBalance: i64::try_from(margin_balance)
                .map_err(|_| perp_err("getAccountMargin: margin balance exceeds i64"))?,
            totalInitialMargin: u64::try_from(total_initial_margin)
                .map_err(|_| perp_err("getAccountMargin: total initial margin exceeds u64"))?,
            totalPositionInitialMargin: u64::try_from(total_position_initial_margin).map_err(
                |_| perp_err("getAccountMargin: total position initial margin exceeds u64"),
            )?,
            totalOpenOrderInitialMargin: total_open_order_initial_margin,
            totalMaintMargin: u64::try_from(total_maint_margin)
                .map_err(|_| perp_err("getAccountMargin: total maintenance margin exceeds u64"))?,
            totalUnrealizedProfit: total_unrealized_profit,
            availableBalance: i64::try_from(available_balance)
                .map_err(|_| perp_err("getAccountMargin: available balance exceeds i64"))?,
            totalMarginReserved: u64::try_from(total_margin_reserved)
                .map_err(|_| perp_err("getAccountMargin: total margin reserved exceeds u64"))?,
        },
    )))
}

/// Re-label a per-market reject so the caller can tell WHICH id in the array failed
/// (`compute_margin_info` only knows it is "getMarginInfo: unknown market"). Fatals and the
/// shell-level variants propagate verbatim — they are not business rejects and must not be
/// reshaped into one.
fn relabel(e: PerpError, market_id: u64) -> PerpError {
    match e {
        PerpError::Reject(m) => perp_err(format!("getAccountMargin: market {market_id}: {m}")),
        other => other,
    }
}

#[cfg(test)]
#[path = "margin_view_tests.rs"]
mod tests;
