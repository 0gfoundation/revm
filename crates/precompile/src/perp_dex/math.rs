//! Pure financial math functions – direct port of the offchain
//! `utils/constants.rs` from the perpdex-rust-backend.
//!
//! All prices use 9-decimal fixed-point (PRICE_ONE = 1_000_000_000).
//! Quote amounts use 6-decimal fixed-point (QUOTE_DECIMALS = 6).

pub const QUOTE_DECIMALS: u32 = 6;
/// 10^9  –  the unit for price representation.
pub const PRICE_ONE: u128 = 1_000_000_000;
pub const FUNDING_RATE_ONE: i128 = 1_000_000_000;
pub const MAX_FUNDING_RATE: i128 = 7_500_000;
pub const MIN_FUNDING_RATE: i128 = -7_500_000;
pub const CLAMP_UPPER_BOUND: i128 = 500_000;
pub const CLAMP_LOWER_BOUND: i128 = -500_000;
pub const INTEREST_RATE: i128 = 100_000;
/// Maintenance margin = notional / 6  ≈ 16.67 %
pub const MAINTENANCE_MARGIN_DENOMINATOR: i128 = 6;

// ── Value helpers ─────────────────────────────────────────────────────────

/// `price × quantity / 10^base_decimals` → quote units (6 decimals).
#[inline]
pub fn calc_value(price: u64, quantity: u64, base_decimals: u32) -> u64 {
    ((price as u128 * quantity as u128 * 10u128.pow(QUOTE_DECIMALS))
        / (PRICE_ONE * 10u128.pow(base_decimals))) as u64
}

/// Signed version of `calc_value` (quantity can be negative).
#[inline]
pub fn calc_value_i64(price: u64, quantity: i64, base_decimals: u32) -> i64 {
    ((price as i128 * quantity as i128 * 10i128.pow(QUOTE_DECIMALS))
        / (PRICE_ONE as i128 * 10i128.pow(base_decimals))) as i64
}

// ── Margin helpers ────────────────────────────────────────────────────────

/// Returns `true` if the position is above the maintenance-margin threshold.
#[inline]
pub fn is_above_maintenance_margin(
    mark_price: u64,
    amount: i64,
    v_quote_balance: i64,
    margin: i64,
    base_decimals: u32,
) -> bool {
    let notional = calc_value_i64(mark_price, amount, base_decimals);
    let position_value = notional + v_quote_balance + margin;
    let threshold = notional.abs() / MAINTENANCE_MARGIN_DENOMINATOR as i64;
    position_value >= threshold
}

/// Proportionally scale `initial_margin` down by the unfilled portion.
#[inline]
pub fn calc_remaining_margin(total_quantity: u64, incoming_quantity: u64, initial_margin: i64) -> i64 {
    if incoming_quantity >= total_quantity {
        0
    } else {
        ((total_quantity - incoming_quantity) as i128 * initial_margin as i128
            / total_quantity as i128) as i64
    }
}

/// Recalculate margin reserves after a leverage change.
#[inline]
pub fn calc_new_margin_reserved_after_leverage_update(
    old_leverage: u64,
    new_leverage: u64,
    old_margin_reserved: u64,
) -> u64 {
    ((old_margin_reserved as u128 * old_leverage as u128) / (new_leverage as u128)) as u64
}

// ── Position analytics (read-only) ───────────────────────────────────────

/// Entry price derived from position state.  Returns 0 if `amount == 0`.
#[inline]
pub fn calc_entry_price(amount: i64, v_quote_balance: i64, base_decimals: u32) -> u64 {
    if amount == 0 {
        return 0;
    }
    ((-v_quote_balance as i128 * PRICE_ONE as i128 * 10i128.pow(base_decimals))
        / (amount as i128 * 10i128.pow(QUOTE_DECIMALS))) as u64
}

/// Liquidation price.  Returns 0 if `amount == 0`.
#[inline]
pub fn calc_liquidation_price(
    amount: i64,
    v_quote_balance: i64,
    margin: i64,
    base_decimals: u32,
) -> i64 {
    if amount > 0 {
        (-(v_quote_balance + margin) as i128 * PRICE_ONE as i128 * 10i128.pow(base_decimals)
            / (amount as i128
                * 10i128.pow(QUOTE_DECIMALS)
                * (MAINTENANCE_MARGIN_DENOMINATOR - 1)
                / MAINTENANCE_MARGIN_DENOMINATOR)) as i64
    } else if amount < 0 {
        (-(v_quote_balance + margin) as i128 * PRICE_ONE as i128 * 10i128.pow(base_decimals)
            / (amount as i128
                * 10i128.pow(QUOTE_DECIMALS)
                * (MAINTENANCE_MARGIN_DENOMINATOR + 1)
                / MAINTENANCE_MARGIN_DENOMINATOR)) as i64
    } else {
        0
    }
}

// ── Funding rate ──────────────────────────────────────────────────────────

/// Compute the funding rate from the average premium index.
#[inline]
pub fn calc_funding_rate(average_premium_index: i64) -> i64 {
    let clamped =
        (INTEREST_RATE - average_premium_index as i128).clamp(CLAMP_LOWER_BOUND, CLAMP_UPPER_BOUND);
    ((average_premium_index as i128 + clamped).clamp(MIN_FUNDING_RATE, MAX_FUNDING_RATE)) as i64
}

/// Apply a funding-rate payment to a position.
#[inline]
pub fn calc_funding_fee(funding_rate: i64, amount: i64, mark_price: u64) -> i64 {
    let v = -((amount as i128 * mark_price as i128) / PRICE_ONE as i128) as i64;
    (funding_rate as i128 * v as i128 / FUNDING_RATE_ONE as i128) as i64
}

// ── Buy/sell side margin reserve helpers ─────────────────────────────────
//
// These are direct ports of the per-side margin reserve calculations from
// `balance/order_entry.rs`.  They operate on a sorted slice rather than a
// DashMap so they work with the on-chain Vec<OrderEntry> storage.

use crate::perp_dex::types::OrderEntry;

/// Recalculate the buy-side margin reserve from the current buy-order list.
/// `buy_entries` must be sorted by price **descending** (highest first).
/// `position_amount` is the *current* net position (before this order list).
pub fn calc_buy_side_margin_reserved(
    buy_entries: &[OrderEntry],
    leverage: u64,
    base_decimals: u32,
    position_amount: i64,
) -> u64 {
    // Only short positions need buy-side margin reservation.
    let mut remaining = if position_amount >= 0 { 0i64 } else { -position_amount };
    let mut reserved = 0u64;
    for e in buy_entries {
        remaining -= e.amount as i64;
        if remaining <= 0 {
            let net_open = (-remaining) as u64;
            reserved += calc_value(e.price, net_open, base_decimals) / leverage;
            remaining = 0;
        }
    }
    reserved
}

/// Recalculate the sell-side margin reserve from the current sell-order list.
/// `sell_entries` must be sorted by price **ascending** (lowest first).
pub fn calc_sell_side_margin_reserved(
    sell_entries: &[OrderEntry],
    leverage: u64,
    base_decimals: u32,
    position_amount: i64,
) -> u64 {
    // Only long positions need sell-side margin reservation.
    let mut remaining = if position_amount <= 0 { 0i64 } else { position_amount };
    let mut reserved = 0u64;
    for e in sell_entries {
        remaining -= e.amount as i64;
        if remaining <= 0 {
            let net_open = (-remaining) as u64;
            reserved += calc_value(e.price, net_open, base_decimals) / leverage;
            remaining = 0;
        }
    }
    reserved
}