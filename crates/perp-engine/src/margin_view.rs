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
    math::{calc_value_i64, checked_u64_to_i64, maintenance_margin, sum_side_totals},
    storage, PerpError,
};

/// Maximum number of market ids `getAccountMargin` will fold in one call.
///
/// The array is caller-supplied and the selector's gas is flat, so it needs a bound. 64 is the
/// same order as `MAX_BATCH_PLACE`; a client with more markets than this pages the call.
pub const MAX_MARGIN_INFO_MARKETS: usize = 64;

/// `ROUND_UP(numerator / leverage)` — the rounding Binance uses for `initialMargin` and
/// `positionInitialMargin`.
///
/// Settled as ROUND_UP (not `HALF_UP`, not `trunc`, not `floor`) on **14/14 discriminating
/// mainnet samples**: `trunc8` and `floor` are refuted by all 14, `HALF_UP` by 5
/// (`binance-margin-verified-model.md` §2; the early three are run1 S4
/// `12.680733023 → 12.68073303`, run1 S10 `6.3410910215 → 6.34109103`, run2 P1
/// `12.757347893 → 12.75734790`). An earlier model recorded these as `HALF_UP`; that was
/// withdrawn — testnet samples happened not to separate the two, mainnet does.
///
/// `leverage` is floored at 1 exactly as [`crate::types::PerpPosition::set_reservations`]
/// floors it, so a zero/corrupt leverage cannot divide by zero.
#[inline]
fn round_up_div(numerator: u128, leverage: u64) -> u128 {
    numerator.div_ceil(leverage.max(1) as u128)
}

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
    /// Maintenance margin at `N` under this market's tier table.
    pub maint_margin: u64,
    // ── ours, for comparison ──
    /// What our engine has ACTUALLY escrowed for this position's resting orders.
    pub margin_reserved_actual: u64,
    /// The position's own allocated margin — our `isolatedWallet`.
    pub position_margin: i64,
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
    // `Bid = Σ (resting buy qty × that order's LIMIT price)` — the LIMIT price, not the mark.
    // `sum_side_totals` is the engine's own per-order-floored fold (the same terms
    // `total_buy_notional` mirrors incrementally), so `bidNotional` is exactly the quantity the
    // reservation math already works in and a client can reproduce it from `getOpenOrders`.
    //
    // Binance warns that its OWN `bidNotional`/`askNotional` are not reproducible from
    // `qty × price` (two arithmetic paths coexist server-side, 1e-5 apart) and must be read
    // from the response body. We have no such split: this IS the definition, evaluated once.
    let buy_entries = storage::load_buy_orders_ref(context, user, market_id)?;
    let sell_entries = storage::load_sell_orders_ref(context, user, market_id)?;
    let (_, bid_notional) = sum_side_totals(
        buy_entries.iter().copied(),
        base_decimals,
        price_decimals,
    )?;
    let (_, ask_notional) = sum_side_totals(
        sell_entries.iter().copied(),
        base_decimals,
        price_decimals,
    )?;

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
    // PIM = ROUND_UP(N / L).
    let position_initial_margin = u64::try_from(round_up_div(notional as u128, leverage))
        .map_err(|_| perp_err("getMarginInfo: position initial margin exceeds u64"))?;

    // IM = ROUND_UP( max(|N + Bid|, |N − Ask|) / L ) — the JOINT requirement over position and
    // resting orders. This is a genuine `max()`, not "one side always wins" and not "the two
    // sides add": mainnet run2 P2/P3/P4 rule out both rivals, and P3→P4 switches the winning
    // branch by changing only the buy quantity (|N+Bid| goes from 15.31 behind to 168.39
    // ahead). The two branches are the exposure left if every BUY fills and if every SELL fills.
    //
    // `N` here is the SIGNED notional. The doc's samples are all LONGS, where signed == the
    // unsigned `notional` field, and it lists short-side signs as unverified (§6). The signed
    // reading is the one the branch SEMANTICS force ("多头暴露 / 空头暴露"): for a short,
    // unsigned `N` would make `|N − Ask|` understate the very exposure the ask branch exists to
    // measure. Widened to `i128` so `N ± (Bid|Ask)` cannot overflow.
    //
    // NOTE this deliberately mixes bases: `N` is at MARK, `Bid`/`Ask` are at each order's LIMIT
    // price. That is Binance's formula, and this layer reports Binance's numbers.
    let n = signed_notional as i128;
    let bid_branch = n
        .checked_add(bid_notional as i128)
        .ok_or_else(|| perp_err("getMarginInfo: bid branch overflow"))?
        .unsigned_abs();
    let ask_branch = n
        .checked_sub(ask_notional as i128)
        .ok_or_else(|| perp_err("getMarginInfo: ask branch overflow"))?
        .unsigned_abs();
    let initial_margin = u64::try_from(round_up_div(bid_branch.max(ask_branch), leverage))
        .map_err(|_| perp_err("getMarginInfo: initial margin exceeds u64"))?;

    // ooIM = IM − PIM. Deliberately the DIFFERENCE OF TWO ROUND_UPs, never a single round-up of
    // a difference: the convenience form `ROUND_UP(max(0, Bid, Ask − 2N) / L)` is NOT equivalent
    // at 1 ulp because `ceil(a) − ceil(b) != ceil(a − b)` (`binance-margin-verified-model.md`
    // §1.1). A parity checker built on the convenience form mis-reports by one unit.
    //
    // `max(|N+Bid|, |N−Ask|) >= |N|` for any `Bid, Ask >= 0` (whichever branch matches `N`'s
    // sign already dominates), so `IM >= PIM` and the subtraction cannot wrap; `saturating_sub`
    // is belt-and-braces on that invariant, not a rounding decision.
    let open_order_initial_margin = initial_margin.saturating_sub(position_initial_margin);

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
        margin_reserved_actual: pos.margin_reserved,
        position_margin: pos.margin,
    })
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
