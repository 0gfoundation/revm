//! Pure financial math functions.
//!
//! Prices use each market's configured fixed-point `price_decimals`.
//! Quote amounts use 6-decimal fixed-point (`QUOTE_DECIMALS = 6`).

use crate::{
    perp_dex::{
        errors::perp_err,
        types::{Market, OrderEntry},
    },
    PrecompileError,
};

pub const QUOTE_DECIMALS: u32 = 6;
/// Fixed-point base for funding rates: 1_000_000 = 100%.  Minimum granularity: 0.0001%.
pub const FUNDING_RATE_ONE: i64 = 1_000_000;
pub const MAX_FUNDING_RATE: i64 = 7_500; // +0.75%
pub const MIN_FUNDING_RATE: i64 = -7_500; // -0.75%
pub const CLAMP_UPPER_BOUND: i64 = 500; // +0.05%  (inner clamp for I−P)
pub const CLAMP_LOWER_BOUND: i64 = -500; // -0.05%
/// Maintenance margin = notional / 6, approximately 16.67%.
pub const MAINTENANCE_MARGIN_DENOMINATOR: i128 = 6;
/// Trading fee denominator. 1 basis point = 1 / 10_000.
pub const FEE_BPS_DENOMINATOR: u64 = 10_000;

/// Default price-band half-width (basis points) used when a market's
/// `price_band_bps` is left at `0`. 1_000 bps = ±10%.
pub const DEFAULT_PRICE_BAND_BPS: u32 = 1_000;

/// Resolve a market's stored `price_band_bps` to the effective value:
/// `0` maps to [`DEFAULT_PRICE_BAND_BPS`], any other value is used verbatim
/// (a large value such as `>= 10_000` effectively disables the band).
#[inline]
pub fn effective_price_band_bps(price_band_bps: u32) -> u32 {
    if price_band_bps == 0 {
        DEFAULT_PRICE_BAND_BPS
    } else {
        price_band_bps
    }
}

/// Inclusive price-band bounds `(upper, lower)` around `mark`, in the u128 price space.
/// A price `P` is IN band iff `lower <= P <= upper`. The band is enforced at FILL time
/// (in the matching loop), not at placement: a resting order may sit anywhere in the
/// book, but a taker/liquidation fill never executes farther than ±band from the CURRENT
/// mark — which is immune to post-placement mark drift and lets harmless deep passive
/// orders rest. `mark == 0` (unset) returns `(u128::MAX, 0)` = no band. `bps >= 10_000`
/// widens the lower bound to `0` (matching [`effective_price_band_bps`] "disabled").
#[inline]
pub fn mark_band_bounds(mark: u64, price_band_bps: u32) -> (u128, u128) {
    if mark == 0 {
        return (u128::MAX, 0);
    }
    let bps = effective_price_band_bps(price_band_bps) as u128;
    let m = mark as u128;
    let upper = m.saturating_mul(10_000 + bps) / 10_000;
    let lower = if bps >= 10_000 {
        0
    } else {
        m * (10_000 - bps) / 10_000
    };
    (upper, lower)
}

#[inline]
fn pow10_u128(exp: u32) -> Result<u128, PrecompileError> {
    10u128
        .checked_pow(exp)
        .ok_or_else(|| perp_err("math: decimal exponent overflow"))
}

#[inline]
fn pow10_i128(exp: u32) -> Result<i128, PrecompileError> {
    10i128
        .checked_pow(exp)
        .ok_or_else(|| perp_err("math: decimal exponent overflow"))
}

#[inline]
pub fn checked_u64_to_i64(value: u64, context: &str) -> Result<i64, PrecompileError> {
    i64::try_from(value).map_err(|_| perp_err(format!("{context}: value exceeds i64::MAX")))
}

/// `price * quantity * 10^QUOTE_DECIMALS / (10^price_decimals * 10^base_decimals)` in quote units.
#[inline]
pub fn calc_value(
    price: u64,
    quantity: u64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<u64, PrecompileError> {
    let quote_scale = pow10_u128(QUOTE_DECIMALS)?;
    let numerator = (price as u128)
        .checked_mul(quantity as u128)
        .and_then(|v| v.checked_mul(quote_scale))
        .ok_or_else(|| perp_err("math: value numerator overflow"))?;
    let denominator = pow10_u128(price_decimals)?
        .checked_mul(pow10_u128(base_decimals)?)
        .ok_or_else(|| perp_err("math: value denominator overflow"))?;
    let value = numerator / denominator;
    u64::try_from(value).map_err(|_| perp_err("math: value exceeds u64"))
}

/// Trading fee in quote units, rounded down.
#[inline]
pub fn calc_trading_fee(notional: u64, fee_bps: u64) -> Result<u64, PrecompileError> {
    let fee = (notional as u128)
        .checked_mul(fee_bps as u128)
        .ok_or_else(|| perp_err("math: trading fee overflow"))?
        / FEE_BPS_DENOMINATOR as u128;
    u64::try_from(fee).map_err(|_| perp_err("math: trading fee exceeds u64"))
}

/// Maker fee (quote units) for `qty` of an order resting at `price`, using a
/// pre-snapshotted `maker_fee_bps`. Single source of truth shared by the
/// placement, fill-release, and cancel-release paths so the reserve↔release
/// fee math cannot drift between them.
#[inline]
pub fn calc_maker_fee_for_order_qty_with_bps(
    price: u64,
    qty: u64,
    maker_fee_bps: u64,
    market: &Market,
) -> Result<u64, PrecompileError> {
    let notional = calc_value(price, qty, market.base_decimals, market.price_decimals)?;
    calc_trading_fee(notional, maker_fee_bps)
}

/// Signed version of `calc_value` for negative quantities.
#[inline]
pub fn calc_value_i64(
    price: u64,
    quantity: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<i64, PrecompileError> {
    let quote_scale = pow10_i128(QUOTE_DECIMALS)?;
    let numerator = (price as i128)
        .checked_mul(quantity as i128)
        .and_then(|v| v.checked_mul(quote_scale))
        .ok_or_else(|| perp_err("math: signed value numerator overflow"))?;
    let denominator = pow10_i128(price_decimals)?
        .checked_mul(pow10_i128(base_decimals)?)
        .ok_or_else(|| perp_err("math: signed value denominator overflow"))?;
    let value = numerator / denominator;
    i64::try_from(value).map_err(|_| perp_err("math: signed value exceeds i64"))
}

/// Returns `true` if the position is above the maintenance-margin threshold.
#[inline]
pub fn is_above_maintenance_margin(
    mark_price: u64,
    amount: i64,
    v_quote_balance: i64,
    margin: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<bool, PrecompileError> {
    let notional = calc_value_i64(mark_price, amount, base_decimals, price_decimals)?;
    let position_value = notional
        .checked_add(v_quote_balance)
        .and_then(|v| v.checked_add(margin))
        .ok_or_else(|| perp_err("math: maintenance margin value overflow"))?;
    let threshold_denominator = i64::try_from(MAINTENANCE_MARGIN_DENOMINATOR)
        .map_err(|_| perp_err("math: maintenance margin denominator exceeds i64"))?;
    let threshold = notional
        .checked_abs()
        .ok_or_else(|| perp_err("math: maintenance margin abs overflow"))?
        / threshold_denominator;
    Ok(position_value >= threshold)
}

/// Position equity at `mark_price`: isolated margin plus unrealized PnL.
#[inline]
pub fn calc_position_equity(
    mark_price: u64,
    amount: i64,
    v_quote_balance: i64,
    margin: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<i64, PrecompileError> {
    calc_value_i64(mark_price, amount, base_decimals, price_decimals)?
        .checked_add(v_quote_balance)
        .and_then(|v| v.checked_add(margin))
        .ok_or_else(|| perp_err("math: position equity overflow"))
}

/// Proportionally scale `initial_margin` down by the unfilled portion.
#[inline]
pub fn calc_remaining_margin(
    total_quantity: u64,
    incoming_quantity: u64,
    initial_margin: i64,
) -> Result<i64, PrecompileError> {
    if incoming_quantity >= total_quantity {
        return Ok(0);
    }
    let remaining = (total_quantity - incoming_quantity) as i128;
    let scaled = remaining
        .checked_mul(initial_margin as i128)
        .ok_or_else(|| perp_err("math: remaining margin overflow"))?
        / total_quantity as i128;
    i64::try_from(scaled).map_err(|_| perp_err("math: remaining margin exceeds i64"))
}

/// Recalculate margin reserves after a leverage change.
#[inline]
pub fn calc_new_margin_reserved_after_leverage_update(
    old_leverage: u64,
    new_leverage: u64,
    old_margin_reserved: u64,
) -> Result<u64, PrecompileError> {
    if new_leverage == 0 {
        return Err(perp_err("math: leverage cannot be zero"));
    }
    let value = (old_margin_reserved as u128)
        .checked_mul(old_leverage as u128)
        .ok_or_else(|| perp_err("math: leverage margin overflow"))?
        / new_leverage as u128;
    u64::try_from(value).map_err(|_| perp_err("math: leverage margin exceeds u64"))
}

/// Entry price derived from position state. Returns 0 if `amount == 0`.
#[inline]
pub fn calc_entry_price(
    amount: i64,
    v_quote_balance: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<u64, PrecompileError> {
    if amount == 0 {
        return Ok(0);
    }
    let numerator = (-(v_quote_balance as i128))
        .checked_mul(pow10_i128(price_decimals)?)
        .and_then(|v| v.checked_mul(pow10_i128(base_decimals).ok()?))
        .ok_or_else(|| perp_err("math: entry price numerator overflow"))?;
    let denominator = (amount as i128)
        .checked_mul(pow10_i128(QUOTE_DECIMALS)?)
        .ok_or_else(|| perp_err("math: entry price denominator overflow"))?;
    let price = numerator / denominator;
    u64::try_from(price).map_err(|_| perp_err("math: entry price exceeds u64"))
}

/// Liquidation price. Returns 0 if `amount == 0`.
#[inline]
pub fn calc_liquidation_price(
    amount: i64,
    v_quote_balance: i64,
    margin: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<i64, PrecompileError> {
    if amount == 0 {
        return Ok(0);
    }
    let numerator = (-(v_quote_balance as i128 + margin as i128))
        .checked_mul(pow10_i128(price_decimals)?)
        .and_then(|v| v.checked_mul(pow10_i128(base_decimals).ok()?))
        .ok_or_else(|| perp_err("math: liquidation price numerator overflow"))?;
    let margin_adjust = if amount > 0 {
        MAINTENANCE_MARGIN_DENOMINATOR - 1
    } else {
        MAINTENANCE_MARGIN_DENOMINATOR + 1
    };
    let denominator = (amount as i128)
        .checked_mul(pow10_i128(QUOTE_DECIMALS)?)
        .and_then(|v| v.checked_mul(margin_adjust))
        .ok_or_else(|| perp_err("math: liquidation price denominator overflow"))?
        / MAINTENANCE_MARGIN_DENOMINATOR;
    let price = numerator / denominator;
    i64::try_from(price).map_err(|_| perp_err("math: liquidation price exceeds i64"))
}

/// Bankruptcy price: the price at which the position's equity is exactly zero
/// (`calc_value_i64(P_b, amount) + v_quote_balance + margin == 0`). This is
/// [`calc_liquidation_price`] WITHOUT the maintenance-margin adjustment (equity == 0,
/// not equity == maintenance). Returns 0 if `amount == 0`.
///
/// Used by ADL to close a liquidated residual against opposite-side holders as a
/// forced trade at `P_b`. Rounding is chosen so the LIQUIDATED position's equity at
/// `P_b` is `>= 0` (a long rounds the price UP, a short rounds it DOWN): closing the
/// residual at `P_b` then never realizes a loss beyond the position's own margin, so
/// it never produces bad debt — ADL scheme X routes NO bad debt to the Insurance
/// Fund. A tiny (<= 1 sub-unit) equity surplus is a conserving credit from the ADL
/// counterparty, never a mint.
#[inline]
pub fn calc_bankruptcy_price(
    amount: i64,
    v_quote_balance: i64,
    margin: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<u64, PrecompileError> {
    if amount == 0 {
        return Ok(0);
    }
    let numerator = (-(v_quote_balance as i128 + margin as i128))
        .checked_mul(pow10_i128(price_decimals)?)
        .and_then(|v| v.checked_mul(pow10_i128(base_decimals).ok()?))
        .ok_or_else(|| perp_err("math: bankruptcy price numerator overflow"))?;
    let denominator = (amount as i128)
        .checked_mul(pow10_i128(QUOTE_DECIMALS)?)
        .ok_or_else(|| perp_err("math: bankruptcy price denominator overflow"))?;
    // Integer division truncates toward zero. For a long (num>0, den>0) that floors,
    // so round UP to keep equity(P_b) >= 0; a short (num<0, den<0) yields a floored
    // positive, which is already the DOWN rounding we want.
    let mut price = numerator / denominator;
    if amount > 0 && numerator % denominator != 0 {
        price += 1;
    }
    // `calc_value_i64` floors internally, so nudge once more if that flooring pushed
    // the liquidated equity a sub-unit negative (long: price up, short: price down).
    // Bounded, deterministic.
    for _ in 0..2 {
        let p = u64::try_from(price).map_err(|_| perp_err("math: bankruptcy price exceeds u64"))?;
        let eq = calc_value_i64(p, amount, base_decimals, price_decimals)?
            .checked_add(v_quote_balance)
            .and_then(|v| v.checked_add(margin))
            .ok_or_else(|| perp_err("math: bankruptcy equity overflow"))?;
        if eq >= 0 {
            return Ok(p);
        }
        price += if amount > 0 { 1 } else { -1 };
    }
    u64::try_from(price).map_err(|_| perp_err("math: bankruptcy price exceeds u64"))
}

/// Compute the funding rate using Binance's formula:
///   F = P + clamp(interest_rate − P, CLAMP_LOWER_BOUND, CLAMP_UPPER_BOUND)
///   F_final = clamp(F, MIN_FUNDING_RATE, MAX_FUNDING_RATE)
#[inline]
pub fn calc_funding_rate(average_premium_index: i64, interest_rate: i64) -> i64 {
    let clamped =
        (interest_rate - average_premium_index).clamp(CLAMP_LOWER_BOUND, CLAMP_UPPER_BOUND);
    (average_premium_index + clamped).clamp(MIN_FUNDING_RATE, MAX_FUNDING_RATE)
}

/// Funding payment (signed, quote units) accrued by a position between two
/// cumulative-funding-index checkpoints.
///
/// `index_delta = cumulative_funding_index_now - position.last_funding_index`,
/// where the per-market index accumulates `mark_price * funding_rate` at each
/// epoch boundary. The result is what should be ADDED to the wallet: positive =
/// credit (the position receives funding), negative = charge (it pays).
///
/// ```text
/// payment = -(amount * index_delta * 10^QUOTE_DECIMALS)
///           / (10^price_decimals * 10^base_decimals * FUNDING_RATE_ONE)
/// ```
///
/// A long (`amount > 0`) pays when the rate is positive (longs pay shorts); the
/// scaling matches `calc_value` so the payment is in 6-decimal quote units like
/// every other balance in the engine.
#[inline]
pub fn calc_funding_payment(
    amount: i64,
    index_delta: i128,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<i64, PrecompileError> {
    let numerator = (amount as i128)
        .checked_mul(index_delta)
        .and_then(|v| v.checked_mul(pow10_i128(QUOTE_DECIMALS).ok()?))
        .ok_or_else(|| perp_err("math: funding payment numerator overflow"))?;
    let denominator = pow10_i128(price_decimals)?
        .checked_mul(pow10_i128(base_decimals)?)
        .and_then(|v| v.checked_mul(FUNDING_RATE_ONE as i128))
        .ok_or_else(|| perp_err("math: funding payment denominator overflow"))?;
    let payment = -(numerator / denominator);
    i64::try_from(payment).map_err(|_| perp_err("math: funding payment exceeds i64"))
}

/// Recalculate the buy-side opening notional from the current buy-order list.
/// `buy_entries` must be sorted by price descending.
/// `position_amount` is the current net position before this order list.
pub fn calc_buy_side_reserved_notional(
    buy_entries: &[OrderEntry],
    base_decimals: u32,
    price_decimals: u32,
    position_amount: i64,
) -> Result<u64, PrecompileError> {
    let mut remaining = if position_amount >= 0 {
        0i64
    } else {
        position_amount
            .checked_neg()
            .ok_or_else(|| perp_err("math: position amount overflow"))?
    };
    let mut reserved_notional = 0u64;
    for e in buy_entries {
        let amount = checked_u64_to_i64(e.amount, "math: buy order amount")?;
        remaining = remaining
            .checked_sub(amount)
            .ok_or_else(|| perp_err("math: buy remaining overflow"))?;
        if remaining <= 0 {
            let net_open = remaining
                .checked_neg()
                .ok_or_else(|| perp_err("math: buy net open overflow"))?
                as u64;
            let notional = calc_value(e.price, net_open, base_decimals, price_decimals)?;
            reserved_notional = reserved_notional
                .checked_add(notional)
                .ok_or_else(|| perp_err("math: buy reserve notional overflow"))?;
            remaining = 0;
        }
    }
    Ok(reserved_notional)
}

/// Recalculate the sell-side opening notional from the current sell-order list.
/// `sell_entries` must be sorted by price ascending.
pub fn calc_sell_side_reserved_notional(
    sell_entries: &[OrderEntry],
    base_decimals: u32,
    price_decimals: u32,
    position_amount: i64,
) -> Result<u64, PrecompileError> {
    let mut remaining = if position_amount <= 0 {
        0i64
    } else {
        position_amount
    };
    let mut reserved_notional = 0u64;
    for e in sell_entries {
        let amount = checked_u64_to_i64(e.amount, "math: sell order amount")?;
        remaining = remaining
            .checked_sub(amount)
            .ok_or_else(|| perp_err("math: sell remaining overflow"))?;
        if remaining <= 0 {
            let net_open = remaining
                .checked_neg()
                .ok_or_else(|| perp_err("math: sell net open overflow"))?
                as u64;
            let notional = calc_value(e.price, net_open, base_decimals, price_decimals)?;
            reserved_notional = reserved_notional
                .checked_add(notional)
                .ok_or_else(|| perp_err("math: sell reserve notional overflow"))?;
            remaining = 0;
        }
    }
    Ok(reserved_notional)
}

/// One step of the cover→open scan, shared by the single and dual notional fns: subtract this
/// order's `amount` from the remaining-to-cover, returning the amount of THIS order that opens
/// (0 while still covering; the overflow at the boundary; the full amount once past). Matches the
/// per-entry logic in [`calc_buy_side_reserved_notional`] exactly.
#[inline]
fn open_amount(remaining: &mut i64, amount: i64) -> Result<i64, PrecompileError> {
    *remaining = remaining
        .checked_sub(amount)
        .ok_or_else(|| perp_err("math: cover remaining overflow"))?;
    if *remaining <= 0 {
        let open = remaining
            .checked_neg()
            .ok_or_else(|| perp_err("math: net open overflow"))?;
        *remaining = 0;
        Ok(open)
    } else {
        Ok(0)
    }
}

/// Buy-side opening notional at TWO positions in a single DESC pass (#21 靶子3): returns
/// `(B(position_a), B(position_b), total_buy_qty)`. Each value is byte-identical to a separate
/// [`calc_buy_side_reserved_notional`] call — but when an order opens the SAME amount for both
/// covers (the common deep-open case), `calc_value` is computed once and added to both, halving the
/// expensive notional math vs two scans. The total order qty is summed in the same pass (free),
/// removing a separate sum pass.
pub fn calc_buy_side_dual(
    buy_entries: impl Iterator<Item = OrderEntry>,
    base_decimals: u32,
    price_decimals: u32,
    position_a: i64,
    position_b: i64,
) -> Result<(u64, u64, i64), PrecompileError> {
    let mut rem_a = if position_a >= 0 {
        0i64
    } else {
        position_a
            .checked_neg()
            .ok_or_else(|| perp_err("math: position amount overflow"))?
    };
    let mut rem_b = if position_b >= 0 {
        0i64
    } else {
        position_b
            .checked_neg()
            .ok_or_else(|| perp_err("math: position amount overflow"))?
    };
    let mut res_a = 0u64;
    let mut res_b = 0u64;
    let mut total = 0u64;
    for e in buy_entries {
        total = total
            .checked_add(e.amount)
            .ok_or_else(|| perp_err("math: total buy order amount"))?;
        let amount = checked_u64_to_i64(e.amount, "math: buy order amount")?;
        let open_a = open_amount(&mut rem_a, amount)?;
        let open_b = open_amount(&mut rem_b, amount)?;
        if open_a == open_b {
            if open_a > 0 {
                let v = calc_value(e.price, open_a as u64, base_decimals, price_decimals)?;
                res_a = res_a
                    .checked_add(v)
                    .ok_or_else(|| perp_err("math: buy reserve notional overflow"))?;
                res_b = res_b
                    .checked_add(v)
                    .ok_or_else(|| perp_err("math: buy reserve notional overflow"))?;
            }
        } else {
            if open_a > 0 {
                let v = calc_value(e.price, open_a as u64, base_decimals, price_decimals)?;
                res_a = res_a
                    .checked_add(v)
                    .ok_or_else(|| perp_err("math: buy reserve notional overflow"))?;
            }
            if open_b > 0 {
                let v = calc_value(e.price, open_b as u64, base_decimals, price_decimals)?;
                res_b = res_b
                    .checked_add(v)
                    .ok_or_else(|| perp_err("math: buy reserve notional overflow"))?;
            }
        }
    }
    Ok((
        res_a,
        res_b,
        checked_u64_to_i64(total, "math: total buy order amount")?,
    ))
}

/// Sell-side opening notional at TWO positions in a single ASC pass (#21 靶子3); see
/// [`calc_buy_side_dual`]. Returns `(S(position_a), S(position_b), total_sell_qty)`.
pub fn calc_sell_side_dual(
    sell_entries: impl Iterator<Item = OrderEntry>,
    base_decimals: u32,
    price_decimals: u32,
    position_a: i64,
    position_b: i64,
) -> Result<(u64, u64, i64), PrecompileError> {
    let mut rem_a = if position_a <= 0 { 0i64 } else { position_a };
    let mut rem_b = if position_b <= 0 { 0i64 } else { position_b };
    let mut res_a = 0u64;
    let mut res_b = 0u64;
    let mut total = 0u64;
    for e in sell_entries {
        total = total
            .checked_add(e.amount)
            .ok_or_else(|| perp_err("math: total sell order amount"))?;
        let amount = checked_u64_to_i64(e.amount, "math: sell order amount")?;
        let open_a = open_amount(&mut rem_a, amount)?;
        let open_b = open_amount(&mut rem_b, amount)?;
        if open_a == open_b {
            if open_a > 0 {
                let v = calc_value(e.price, open_a as u64, base_decimals, price_decimals)?;
                res_a = res_a
                    .checked_add(v)
                    .ok_or_else(|| perp_err("math: sell reserve notional overflow"))?;
                res_b = res_b
                    .checked_add(v)
                    .ok_or_else(|| perp_err("math: sell reserve notional overflow"))?;
            }
        } else {
            if open_a > 0 {
                let v = calc_value(e.price, open_a as u64, base_decimals, price_decimals)?;
                res_a = res_a
                    .checked_add(v)
                    .ok_or_else(|| perp_err("math: sell reserve notional overflow"))?;
            }
            if open_b > 0 {
                let v = calc_value(e.price, open_b as u64, base_decimals, price_decimals)?;
                res_b = res_b
                    .checked_add(v)
                    .ok_or_else(|| perp_err("math: sell reserve notional overflow"))?;
            }
        }
    }
    Ok((
        res_a,
        res_b,
        checked_u64_to_i64(total, "math: total sell order amount")?,
    ))
}

/// Flip-aware worst-case reservation notional for a user's resting book.
///
/// Returns `(buy_notional B, sell_notional S, c_notional)` where:
/// - `B = calc_buy_side_reserved_notional(buys, p)` — buy-side opening notional
///   at the current position `p`,
/// - `S = calc_sell_side_reserved_notional(sells, p)` — sell-side likewise,
/// - `c_notional = max(S + B', B + S')` — the peak capital the position can
///   require across a full sign-flip in either direction:
///   - `B' = B(p − total_sell_qty)`: if every sell fills first the position
///     goes maximally short, so the surviving buys re-open more notional,
///   - `S' = S(p + total_buy_qty)`: symmetric for the long extreme.
///
/// `max-of-side = max(B, S)` under-reserves because it ignores that a fill on
/// one side flips the position and re-prices the *other* side's opening leg;
/// `c_notional` is the tight peak (verified exact: it equals the reachable
/// maximum of [realized-position margin + remaining-order reservation] given
/// the engine's price-priority fill order — buys filled DESC, sells ASC — which
/// is a load-bearing precondition). It dominates `max-of-side`
/// (`c_notional ≥ max(B, S)` since `B', S' ≥ 0`), so reservations only grow.
///
/// The combined leg is summed in `u128` and floored once on division by
/// leverage in `set_reservations` (single floor, not a sum of per-leg floors),
/// which is the strictly-safer rounding.
pub fn calc_reservation_notionals(
    buy_entries: &[OrderEntry],
    sell_entries: &[OrderEntry],
    base_decimals: u32,
    price_decimals: u32,
    position_amount: i64,
) -> Result<(u64, u64, u64), PrecompileError> {
    calc_reservation_notionals_it(
        buy_entries.iter().copied(),
        sell_entries.iter().copied(),
        base_decimals,
        price_decimals,
        position_amount,
    )
}

/// Iterator form of [`calc_reservation_notionals`] (commit-only #23 perf): folds the two sides
/// from iterators instead of slices, so a caller can evaluate the reservation of a HYPOTHETICAL
/// book (e.g. "current list + one entry at its sorted position", via `.chain`) WITHOUT cloning the
/// list or writing the overlay — the validate-then-apply probe for rest_in_book / release. The
/// slice form above is a thin wrapper (`.iter().copied()`), so both paths run the exact same fold
/// → byte-identical results (the price-index/notional bytes folded into the commitment are
/// unchanged). `S: Clone` because the sell side is iterated twice (total-qty pass + dual pass); the
/// buy side is consumed once. `OrderEntry: Copy`, so by-value iteration is a cheap stack copy.
pub fn calc_reservation_notionals_it<B, S>(
    buy_entries: B,
    sell_entries: S,
    base_decimals: u32,
    price_decimals: u32,
    position_amount: i64,
) -> Result<(u64, u64, u64), PrecompileError>
where
    B: Iterator<Item = OrderEntry>,
    S: Iterator<Item = OrderEntry> + Clone,
{
    // #21 靶子3: compute all four opening notionals in THREE passes instead of six. The buy leg
    // needs the post-all-sells position (max-short), so total_sell_qty comes first (one cheap sum
    // pass); the buy dual then yields B + B' AND total_buy_qty in one pass; the sell dual yields
    // S + S' (it needs the post-all-buys position from total_buy_qty). Each value is byte-identical
    // to the old separate calc_*_reserved_notional calls (gated by the dual_matches_separate test).
    let total_sell_qty = total_entry_amount(sell_entries.clone(), "math: total sell order amount")?;
    // Position after every sell fills → most short; surviving buys re-open from there.
    let position_after_sells = position_amount
        .checked_sub(total_sell_qty)
        .ok_or_else(|| perp_err("math: flip-short position overflow"))?;
    let (buy_notional, buy_flip_notional, total_buy_qty) = calc_buy_side_dual(
        buy_entries,
        base_decimals,
        price_decimals,
        position_amount,
        position_after_sells,
    )?;

    // Position after every buy fills → most long; surviving sells re-open from there.
    let position_after_buys = position_amount
        .checked_add(total_buy_qty)
        .ok_or_else(|| perp_err("math: flip-long position overflow"))?;
    let (sell_notional, sell_flip_notional, _) = calc_sell_side_dual(
        sell_entries,
        base_decimals,
        price_decimals,
        position_amount,
        position_after_buys,
    )?;

    let leg_short = (sell_notional as u128)
        .checked_add(buy_flip_notional as u128)
        .ok_or_else(|| perp_err("math: flip-short leg overflow"))?;
    let leg_long = (buy_notional as u128)
        .checked_add(sell_flip_notional as u128)
        .ok_or_else(|| perp_err("math: flip-long leg overflow"))?;
    let c_notional = u64::try_from(leg_short.max(leg_long))
        .map_err(|_| perp_err("math: flip-aware reservation notional exceeds u64"))?;

    Ok((buy_notional, sell_notional, c_notional))
}

/// Sum of all order-entry amounts as an `i64` (checked).
fn total_entry_amount(
    entries: impl Iterator<Item = OrderEntry>,
    ctx: &str,
) -> Result<i64, PrecompileError> {
    let mut total = 0u64;
    for e in entries {
        total = total.checked_add(e.amount).ok_or_else(|| perp_err(ctx))?;
    }
    checked_u64_to_i64(total, ctx)
}

// ── Incremental-reservation primitives (catalog #A, Step 1) ─────────────────────────────────
//
// The flip-aware reservation `calc_reservation_notionals` folds THREE passes over the acting
// account's whole buy+sell lists on every place/cancel — O(n) in the account's resting-order
// count. Sim A (docs/hl-rust-sim-bottleneck-results-20260727.md) showed this is THE hot-path
// bottleneck: a 3 655-order MM's place+cancel is 19× a 1-order account's, and those long-list MMs
// are the dominant churners.
//
// These primitives reconstruct each side's opening notional from a MAINTAINED per-side aggregate
// instead of a full fold. The key identity (proven byte-exact below):
//
//     side_notional(cover C) = TotalNotional − Σ_{covered prefix} calc_value(price_i, amount_i)
//                                             + calc_value(price_boundary, open_at_boundary)
//
// where `TotalNotional = Σ_i calc_value(price_i, amount_i)` is the sum of the SAME per-order
// floored `calc_value` terms the fold produces (so it is maintainable ± one term per insert/remove
// with ZERO floor-composition error — `calc_value` floors per call, so only this per-order-floored
// definition stays byte-identical). The covered prefix is the highest-price buys / lowest-price
// sells totalling `C` in quantity; it is EMPTY when C = 0 (a flat/aligned position or a one-sided
// book) → the leg is just `TotalNotional`, computed with NO list walk. Otherwise the walk spans
// only the cover prefix (bounded by |position| / the flip totals), never the full list.

/// `(Σ amount, Σ calc_value(price, amount))` over a side's order list — the maintained aggregate
/// the leg reconstruction below consumes. Cold-rebuild / test helper; production maintains these
/// incrementally so this full pass runs only on a cold first-touch.
pub fn sum_side_totals(
    entries: impl Iterator<Item = OrderEntry>,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<(u64, u64), PrecompileError> {
    let mut qty = 0u64;
    let mut notional = 0u64;
    for e in entries {
        qty = qty
            .checked_add(e.amount)
            .ok_or_else(|| perp_err("math: side total qty overflow"))?;
        let v = calc_value(e.price, e.amount, base_decimals, price_decimals)?;
        notional = notional
            .checked_add(v)
            .ok_or_else(|| perp_err("math: side total notional overflow"))?;
    }
    Ok((qty, notional))
}

/// Reconstruct one side's opening notional at cover threshold `cover` (≥ 0) from `total_notional`,
/// walking ONLY the cover prefix. `entries` must be in the side's cover order (buys DESC, sells
/// ASC — the same order the fold consumes). Byte-identical to
/// `calc_{buy,sell}_side_reserved_notional` (see the fuzz gate `from_totals_matches_fold`).
fn side_leg_from_total(
    entries: impl Iterator<Item = OrderEntry>,
    total_notional: u64,
    base_decimals: u32,
    price_decimals: u32,
    cover: i64,
) -> Result<u64, PrecompileError> {
    if cover <= 0 {
        return Ok(total_notional);
    }
    let mut remaining = cover as u64;
    let mut leg = total_notional;
    for e in entries {
        if remaining >= e.amount {
            // Fully covered: this order opens nothing → drop its full per-order notional.
            let cv = calc_value(e.price, e.amount, base_decimals, price_decimals)?;
            leg = leg
                .checked_sub(cv)
                .ok_or_else(|| perp_err("math: side leg cover underflow"))?;
            remaining -= e.amount;
            if remaining == 0 {
                break; // all subsequent orders open fully → already counted in total_notional
            }
        } else {
            // Boundary order: covers `remaining`, opens `amount - remaining`.
            let open = e.amount - remaining;
            let cv_full = calc_value(e.price, e.amount, base_decimals, price_decimals)?;
            let cv_open = calc_value(e.price, open, base_decimals, price_decimals)?;
            leg = leg
                .checked_sub(cv_full)
                .ok_or_else(|| perp_err("math: side leg boundary underflow"))?
                .checked_add(cv_open)
                .ok_or_else(|| perp_err("math: side leg boundary overflow"))?;
            break;
        }
    }
    Ok(leg)
}

/// Flip-aware worst-case reservation `(B, S, c_notional)` — byte-identical to
/// [`calc_reservation_notionals`] — reconstructed from the maintained per-side aggregates
/// `(total_buy_qty, total_buy_notional, total_sell_qty, total_sell_notional)` instead of a full
/// fold. Each of the four legs (B, B′, S, S′) walks only its cover prefix, so a flat/aligned or
/// one-sided book is O(1). `buy_entries` DESC, `sell_entries` ASC.
pub fn calc_reservation_notionals_from_totals(
    buy_entries: &[OrderEntry],
    sell_entries: &[OrderEntry],
    total_buy_qty: u64,
    total_buy_notional: u64,
    total_sell_qty: u64,
    total_sell_notional: u64,
    base_decimals: u32,
    price_decimals: u32,
    position_amount: i64,
) -> Result<(u64, u64, u64), PrecompileError> {
    calc_reservation_notionals_from_totals_it(
        buy_entries.iter().copied(),
        sell_entries.iter().copied(),
        total_buy_qty,
        total_buy_notional,
        total_sell_qty,
        total_sell_notional,
        base_decimals,
        price_decimals,
        position_amount,
    )
}

/// Iterator form of [`calc_reservation_notionals_from_totals`] — lets a caller evaluate the
/// reservation of a HYPOTHETICAL book (current list + one entry at its sorted slot, via `.chain`)
/// with NO owned clone (the validate-then-apply probe in `rest_in_book`). `B`/`S: Clone` because
/// each side is iterated twice (own-position leg + flip leg). Byte-identical to the slice form.
#[allow(clippy::too_many_arguments)]
pub fn calc_reservation_notionals_from_totals_it<B, S>(
    buy_entries: B,
    sell_entries: S,
    total_buy_qty: u64,
    total_buy_notional: u64,
    total_sell_qty: u64,
    total_sell_notional: u64,
    base_decimals: u32,
    price_decimals: u32,
    position_amount: i64,
) -> Result<(u64, u64, u64), PrecompileError>
where
    B: Iterator<Item = OrderEntry> + Clone,
    S: Iterator<Item = OrderEntry> + Clone,
{
    let p = position_amount;
    let tsq = checked_u64_to_i64(total_sell_qty, "math: total sell qty")?;
    let tbq = checked_u64_to_i64(total_buy_qty, "math: total buy qty")?;

    // B = buy leg at current position (cover the short, if any).
    let cover_b = if p >= 0 { 0 } else { -p };
    let b = side_leg_from_total(
        buy_entries.clone(),
        total_buy_notional,
        base_decimals,
        price_decimals,
        cover_b,
    )?;
    // B′ = buy leg after all sells fill (position → most short).
    let pos_after_sells = p
        .checked_sub(tsq)
        .ok_or_else(|| perp_err("math: flip-short position overflow"))?;
    let cover_b_flip = if pos_after_sells >= 0 {
        0
    } else {
        -pos_after_sells
    };
    let b_flip = side_leg_from_total(
        buy_entries,
        total_buy_notional,
        base_decimals,
        price_decimals,
        cover_b_flip,
    )?;
    // S = sell leg at current position (cover the long, if any).
    let cover_s = if p <= 0 { 0 } else { p };
    let s = side_leg_from_total(
        sell_entries.clone(),
        total_sell_notional,
        base_decimals,
        price_decimals,
        cover_s,
    )?;
    // S′ = sell leg after all buys fill (position → most long).
    let pos_after_buys = p
        .checked_add(tbq)
        .ok_or_else(|| perp_err("math: flip-long position overflow"))?;
    let cover_s_flip = if pos_after_buys <= 0 { 0 } else { pos_after_buys };
    let s_flip = side_leg_from_total(
        sell_entries,
        total_sell_notional,
        base_decimals,
        price_decimals,
        cover_s_flip,
    )?;

    let leg_short = (s as u128)
        .checked_add(b_flip as u128)
        .ok_or_else(|| perp_err("math: flip-short leg overflow"))?;
    let leg_long = (b as u128)
        .checked_add(s_flip as u128)
        .ok_or_else(|| perp_err("math: flip-long leg overflow"))?;
    let c_notional = u64::try_from(leg_short.max(leg_long))
        .map_err(|_| perp_err("math: flip-aware reservation notional exceeds u64"))?;
    Ok((b, s, c_notional))
}

#[cfg(test)]
mod reservation_notional_tests {
    use super::*;

    fn entry(price: u64, amount: u64) -> OrderEntry {
        OrderEntry {
            order_id: [0u8; 32],
            price,
            amount,
            maker_fee_bps: 0,
        }
    }

    // Deterministic xorshift PRNG (no std rng in the precompile crate).
    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// #21 靶子3 GATE: the dual-cover scans must be byte-identical to two separate single-cover
    /// scans (so `calc_reservation_notionals` stays byte-identical → commitment unchanged). Fuzzed
    /// over random two-sided books + positions, using the SAME (position, position-after-flip)
    /// covers `calc_reservation_notionals` feeds them.
    #[test]
    fn dual_matches_separate_scans() {
        let mut s: u64 = 0x9e3779b97f4a7c15;
        let bd = 4u32;
        let pd = 2u32;
        for _ in 0..5000 {
            let nb = (next(&mut s) % 6) as usize;
            let ns = (next(&mut s) % 6) as usize;
            let mut buys: Vec<OrderEntry> = (0..nb)
                .map(|_| entry(next(&mut s) % 50 + 1, next(&mut s) % 100 + 1))
                .collect();
            buys.sort_by(|a, b| b.price.cmp(&a.price)); // DESC
            let mut sells: Vec<OrderEntry> = (0..ns)
                .map(|_| entry(next(&mut s) % 50 + 1, next(&mut s) % 100 + 1))
                .collect();
            sells.sort_by(|a, b| a.price.cmp(&b.price)); // ASC
            let p = (next(&mut s) % 400) as i64 - 200;

            let total_buy: i64 = buys.iter().map(|e| e.amount as i64).sum();
            let total_sell: i64 = sells.iter().map(|e| e.amount as i64).sum();
            let pos_after_sells = p - total_sell;
            let pos_after_buys = p + total_buy;

            let (ba, bb, tb) =
                calc_buy_side_dual(buys.iter().copied(), bd, pd, p, pos_after_sells).unwrap();
            assert_eq!(
                ba,
                calc_buy_side_reserved_notional(&buys, bd, pd, p).unwrap()
            );
            assert_eq!(
                bb,
                calc_buy_side_reserved_notional(&buys, bd, pd, pos_after_sells).unwrap()
            );
            assert_eq!(tb, total_buy);

            let (sa, sb, ts) =
                calc_sell_side_dual(sells.iter().copied(), bd, pd, p, pos_after_buys).unwrap();
            assert_eq!(
                sa,
                calc_sell_side_reserved_notional(&sells, bd, pd, p).unwrap()
            );
            assert_eq!(
                sb,
                calc_sell_side_reserved_notional(&sells, bd, pd, pos_after_buys).unwrap()
            );
            assert_eq!(ts, total_sell);
        }
    }

    /// #A Step 1 GATE: the incremental `calc_reservation_notionals_from_totals` must be
    /// BYTE-IDENTICAL to the fold `calc_reservation_notionals` over random two-sided books +
    /// positions (so switching the call sites to it leaves the commitment/golden unchanged).
    /// Totals are computed via `sum_side_totals` exactly as production will maintain them.
    #[test]
    fn from_totals_matches_fold() {
        let mut s: u64 = 0x243f6a8885a308d3;
        for &(bd, pd) in &[(0u32, 0u32), (4, 2), (8, 9), (3, 2)] {
            for _ in 0..5000 {
                let nb = (next(&mut s) % 8) as usize;
                let ns = (next(&mut s) % 8) as usize;
                let mut buys: Vec<OrderEntry> = (0..nb)
                    .map(|_| entry(next(&mut s) % 50 + 1, next(&mut s) % 100 + 1))
                    .collect();
                buys.sort_by(|a, b| b.price.cmp(&a.price)); // DESC
                let mut sells: Vec<OrderEntry> = (0..ns)
                    .map(|_| entry(next(&mut s) % 50 + 1, next(&mut s) % 100 + 1))
                    .collect();
                sells.sort_by(|a, b| a.price.cmp(&b.price)); // ASC
                let p = (next(&mut s) % 800) as i64 - 400;

                let (tbq, tbn) = sum_side_totals(buys.iter().copied(), bd, pd).unwrap();
                let (tsq, tsn) = sum_side_totals(sells.iter().copied(), bd, pd).unwrap();

                let fold = calc_reservation_notionals(&buys, &sells, bd, pd, p).unwrap();
                let incr = calc_reservation_notionals_from_totals(
                    &buys, &sells, tbq, tbn, tsq, tsn, bd, pd, p,
                )
                .unwrap();
                assert_eq!(fold, incr, "bd={bd} pd={pd} p={p} buys={buys:?} sells={sells:?}");
            }
        }
    }

    #[test]
    fn flip_aware_c_matches_worked_example() {
        // buys = [(price 3, qty 2), (price 2, qty 1)] (DESC), sells = [(4, 1)],
        // position = -1, base_decimals = price_decimals = 0 (values scale by
        // QUOTE_DECIMALS = 1e6). Hand-derived:
        //   B = B(-1)      = 3 + 2 = 5      (both buys re-open from short 1)
        //   S = S(-1)      = 4              (sell opens short)
        //   B' = B(-2)     = 2     (after the lone sell fills → short 2)
        //   S' = S(+2)     = 0     (after all 3 buys fill → long 2; sell only closes)
        //   C = max(S + B', B + S') = max(4 + 2, 5 + 0) = 6.
        let buys = [entry(3, 2), entry(2, 1)];
        let sells = [entry(4, 1)];
        let (b, s, c) = calc_reservation_notionals(&buys, &sells, 0, 0, -1).unwrap();
        assert_eq!(b, 5_000_000);
        assert_eq!(s, 4_000_000);
        assert_eq!(c, 6_000_000);
        // C strictly exceeds the old max-of-side (5e6): it covers the flip.
        assert!(c > b.max(s));
    }

    #[test]
    fn flip_aware_c_equals_max_of_side_for_one_sided_book() {
        // A one-sided (buy-only) book cannot flip the position the other way,
        // so the flip-aware C collapses to the plain buy-side notional.
        let buys = [entry(10, 3)];
        let (b, s, c) = calc_reservation_notionals(&buys, &[], 0, 0, 0).unwrap();
        assert_eq!(s, 0);
        assert_eq!(c, b);
        assert_eq!(c, b.max(s));
    }

    #[test]
    fn flip_aware_c_is_symmetric_for_one_sided_sell_book() {
        // Symmetric to the buy-only case: sell-only book → C == sell notional.
        let sells = [entry(7, 4)];
        let (b, s, c) = calc_reservation_notionals(&[], &sells, 0, 0, 0).unwrap();
        assert_eq!(b, 0);
        assert_eq!(c, s);
    }
}

#[cfg(test)]
mod bankruptcy_price_tests {
    use super::*;

    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// For realistic longs AND shorts across many decimals/leverages, the bankruptcy
    /// price must satisfy `0 <= equity(P_b) <= dust` — i.e. closing the residual at
    /// `P_b` NEVER leaves the position insolvent (no bad debt, ADL scheme X routes none
    /// to the IF), and `P_b` is tight (equity is a sub-unit above zero, a conserving
    /// crumb, never a mint).
    #[test]
    fn bankruptcy_price_keeps_liquidated_equity_nonnegative_and_tight() {
        let mut s: u64 = 0xc0ffee_1234_5678;
        for _ in 0..20000 {
            let bd = (next(&mut s) % 5) as u32; // 0..=4
            let pd = (next(&mut s) % 5) as u32; // 0..=4
            let entry = (next(&mut s) % 10_000_000 + 1) as u64; // <= maxPrice (1e8)
            let qty = (next(&mut s) % 100_000 + 1) as u64;
            let lev = (next(&mut s) % 6 + 1) as i64; // 1..=6
            let notional = match calc_value(entry, qty, bd, pd) {
                Ok(v) if v > 0 && v <= i64::MAX as u64 => v as i64,
                _ => continue, // beyond realistic on-chain bounds (maxPrice/maxQuantity)
            };
            let margin = notional / lev;
            let is_long = next(&mut s) & 1 == 0;
            // Long: paid the notional (vq negative); short: received it (vq positive).
            let (amount, v_quote) = if is_long {
                (qty as i64, -notional)
            } else {
                (-(qty as i64), notional)
            };

            let p_b = calc_bankruptcy_price(amount, v_quote, margin, bd, pd).unwrap();
            // P_b == 0 is valid for a <=1x position (bankrupt only at price 0 => never
            // insolvent => never ADL'd). Only the equity invariant must hold.
            let eq = match calc_position_equity(p_b, amount, v_quote, margin, bd, pd) {
                Ok(e) => e,
                Err(_) => continue, // P_b*amount beyond i64 (unrealistic)
            };
            assert!(eq >= 0, "equity(P_b) < 0 => bad debt: eq={eq} P_b={p_b} amount={amount} vq={v_quote} m={margin} bd={bd} pd={pd}");
            // Tightness: equity at P_b is within one price sub-unit's worth of value.
            let one_tick = calc_value(1, qty, bd, pd).unwrap() as i64 + 2;
            assert!(eq <= one_tick, "equity(P_b) not tight: eq={eq} bound={one_tick} P_b={p_b}");
        }
    }

    #[test]
    fn bankruptcy_price_zero_amount_is_zero() {
        assert_eq!(calc_bankruptcy_price(0, -100, 10, 2, 2).unwrap(), 0);
    }
}

#[cfg(test)]
mod funding_payment_tests {
    use super::*;

    // amount=10 (base_decimals=0), mark=$100 (10000 @ price_decimals=2),
    // one epoch at rate 7500 (0.75%): index_delta = mark*rate = 75_000_000.
    // notional = $1000 = 1e9 quote units; funding = 1e9 * 0.0075 = 7_500_000.
    const DELTA_ONE_EPOCH: i128 = 10_000 * 7_500; // 75_000_000

    #[test]
    fn long_pays_funding_when_rate_positive() {
        // Long (amount > 0) pays → negative (debit).
        assert_eq!(
            calc_funding_payment(10, DELTA_ONE_EPOCH, 0, 2).unwrap(),
            -7_500_000
        );
    }

    #[test]
    fn short_receives_funding_when_rate_positive() {
        // Short (amount < 0) receives → positive (credit).
        assert_eq!(
            calc_funding_payment(-10, DELTA_ONE_EPOCH, 0, 2).unwrap(),
            7_500_000
        );
    }

    #[test]
    fn zero_position_pays_nothing() {
        assert_eq!(calc_funding_payment(0, DELTA_ONE_EPOCH, 0, 2).unwrap(), 0);
    }

    #[test]
    fn accrual_scales_linearly_across_epochs() {
        // Two epochs of accrued index → twice the payment.
        assert_eq!(
            calc_funding_payment(10, DELTA_ONE_EPOCH * 2, 0, 2).unwrap(),
            -15_000_000
        );
    }

    #[test]
    fn long_receives_when_rate_negative() {
        // Negative funding rate flips the direction: long receives.
        assert_eq!(
            calc_funding_payment(10, -DELTA_ONE_EPOCH, 0, 2).unwrap(),
            7_500_000
        );
    }
}

#[cfg(test)]
mod mark_band_bounds_tests {
    use super::{mark_band_bounds, DEFAULT_PRICE_BAND_BPS};

    #[test]
    fn unset_mark_disables_band() {
        // mark == 0: cannot evaluate a band -> (MAX, 0) = no bound either way.
        assert_eq!(mark_band_bounds(0, 1_000), (u128::MAX, 0));
        assert_eq!(mark_band_bounds(0, 0), (u128::MAX, 0));
    }

    #[test]
    fn zero_bps_uses_default_ten_percent() {
        assert_eq!(DEFAULT_PRICE_BAND_BPS, 1_000);
        // mark 100 -> [90, 110].
        assert_eq!(mark_band_bounds(100, 0), (110, 90));
    }

    #[test]
    fn configured_bps_is_used_verbatim() {
        // 500 bps = +-5%. mark 100 -> [95, 105].
        assert_eq!(mark_band_bounds(100, 500), (105, 95));
    }

    #[test]
    fn large_bps_widens_lower_bound_to_zero() {
        // bps >= 10_000: lower clamps to 0; upper still grows with bps.
        assert_eq!(mark_band_bounds(100, 10_000), (200, 0));
        let (upper, lower) = mark_band_bounds(100, 1_000_000);
        assert_eq!(lower, 0);
        assert_eq!(upper, 100 * (10_000 + 1_000_000) / 10_000);
    }

    #[test]
    fn no_overflow_at_extremes() {
        // huge mark * huge bps must saturate, not panic.
        let (upper, lower) = mark_band_bounds(u64::MAX, u32::MAX);
        assert_eq!(lower, 0);
        assert!(upper > 0);
    }
}
