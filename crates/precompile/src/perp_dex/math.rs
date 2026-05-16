//! Pure financial math functions.
//!
//! Prices use each market's configured fixed-point `price_decimals`.
//! Quote amounts use 6-decimal fixed-point (`QUOTE_DECIMALS = 6`).

use crate::{
    perp_dex::{errors::perp_err, types::OrderEntry},
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

/// Compute the funding rate using Binance's formula:
///   F = P + clamp(interest_rate − P, CLAMP_LOWER_BOUND, CLAMP_UPPER_BOUND)
///   F_final = clamp(F, MIN_FUNDING_RATE, MAX_FUNDING_RATE)
#[inline]
pub fn calc_funding_rate(average_premium_index: i64, interest_rate: i64) -> i64 {
    let clamped =
        (interest_rate - average_premium_index).clamp(CLAMP_LOWER_BOUND, CLAMP_UPPER_BOUND);
    (average_premium_index + clamped).clamp(MIN_FUNDING_RATE, MAX_FUNDING_RATE)
}

/// Apply a funding-rate payment to a position.
#[inline]
pub fn calc_funding_fee(
    funding_rate: i64,
    amount: i64,
    mark_price: u64,
    price_decimals: u32,
) -> Result<i64, PrecompileError> {
    let v = -((amount as i128)
        .checked_mul(mark_price as i128)
        .ok_or_else(|| perp_err("math: funding value overflow"))?
        / pow10_i128(price_decimals)?);
    let fee = (funding_rate as i128)
        .checked_mul(v)
        .ok_or_else(|| perp_err("math: funding fee overflow"))?
        / FUNDING_RATE_ONE as i128;
    i64::try_from(fee).map_err(|_| perp_err("math: funding fee exceeds i64"))
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

/// Recalculate the buy-side margin reserve from the current buy-order list.
/// Rounding happens once after summing all opening notional.
pub fn calc_buy_side_margin_reserved(
    buy_entries: &[OrderEntry],
    leverage: u64,
    base_decimals: u32,
    price_decimals: u32,
    position_amount: i64,
) -> Result<u64, PrecompileError> {
    if leverage == 0 {
        return Err(perp_err("math: leverage cannot be zero"));
    }
    Ok(calc_buy_side_reserved_notional(
        buy_entries,
        base_decimals,
        price_decimals,
        position_amount,
    )? / leverage)
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

/// Recalculate the sell-side margin reserve from the current sell-order list.
/// Rounding happens once after summing all opening notional.
pub fn calc_sell_side_margin_reserved(
    sell_entries: &[OrderEntry],
    leverage: u64,
    base_decimals: u32,
    price_decimals: u32,
    position_amount: i64,
) -> Result<u64, PrecompileError> {
    if leverage == 0 {
        return Err(perp_err("math: leverage cannot be zero"));
    }
    Ok(calc_sell_side_reserved_notional(
        sell_entries,
        base_decimals,
        price_decimals,
        position_amount,
    )? / leverage)
}
