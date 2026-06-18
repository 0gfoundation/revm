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
    let buy_notional =
        calc_buy_side_reserved_notional(buy_entries, base_decimals, price_decimals, position_amount)?;
    let sell_notional = calc_sell_side_reserved_notional(
        sell_entries,
        base_decimals,
        price_decimals,
        position_amount,
    )?;

    let total_buy_qty = total_entry_amount(buy_entries, "math: total buy order amount")?;
    let total_sell_qty = total_entry_amount(sell_entries, "math: total sell order amount")?;

    // Position after every sell fills → most short; surviving buys re-open from there.
    let position_after_sells = position_amount
        .checked_sub(total_sell_qty)
        .ok_or_else(|| perp_err("math: flip-short position overflow"))?;
    let buy_flip_notional = calc_buy_side_reserved_notional(
        buy_entries,
        base_decimals,
        price_decimals,
        position_after_sells,
    )?;

    // Position after every buy fills → most long; surviving sells re-open from there.
    let position_after_buys = position_amount
        .checked_add(total_buy_qty)
        .ok_or_else(|| perp_err("math: flip-long position overflow"))?;
    let sell_flip_notional = calc_sell_side_reserved_notional(
        sell_entries,
        base_decimals,
        price_decimals,
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
fn total_entry_amount(entries: &[OrderEntry], ctx: &str) -> Result<i64, PrecompileError> {
    let mut total = 0u64;
    for e in entries {
        total = total.checked_add(e.amount).ok_or_else(|| perp_err(ctx))?;
    }
    checked_u64_to_i64(total, ctx)
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
