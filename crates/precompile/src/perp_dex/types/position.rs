//! Position and market types for the PerpDEX precompile.
use serde::{Deserialize, Serialize};

// ── PerpPosition ──────────────────────────────────────────────────────────

/// On-chain perpetual position for one user in one market.
///
/// All numeric fields use native integer types; msgpack serialises them
/// efficiently without string encoding (unlike the U256 `AsBinStr` pattern).
///
/// Field naming mirrors the offchain `PerpPosition` in `balance/balance.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PerpPosition {
    /// Net base-asset position: positive = long, negative = short.
    #[serde(rename = "a")]
    pub amount: i64,
    /// Virtual quote balance.  Entry price = -v_quote_balance / amount.
    #[serde(rename = "v")]
    pub v_quote_balance: i64,
    /// Margin (collateral) allocated to this position.
    #[serde(rename = "m")]
    pub margin: i64,
    /// Total margin reserved for open orders (max of buy- and sell-side).
    #[serde(rename = "mr")]
    pub margin_reserved: u64,
    /// Total open-order notional used to derive `margin_reserved`.
    #[serde(default, rename = "mrn")]
    pub margin_reserved_notional: u64,
    /// Margin reserved for the buy side of open orders.
    #[serde(rename = "br")]
    pub buy_side_margin_reserved: u64,
    /// Buy-side open-order notional before leverage division.
    #[serde(default, rename = "brn")]
    pub buy_side_reserved_notional: u64,
    /// Margin reserved for the sell side of open orders.
    #[serde(rename = "sr")]
    pub sell_side_margin_reserved: u64,
    /// Sell-side open-order notional before leverage division.
    #[serde(default, rename = "srn")]
    pub sell_side_reserved_notional: u64,
    /// Total maker fee reserved for open orders (buy side + sell side).
    #[serde(default, rename = "fr")]
    pub fee_reserved: u64,
    /// Current leverage setting (1–6).
    #[serde(rename = "lv")]
    pub leverage: u64,
    /// Cumulative funding index at this position's last funding settlement.
    /// Funding owed = `amount × (market cumulative_funding_index − this)`,
    /// settled lazily on every position-touching op. See the `funding` module.
    #[serde(default, rename = "fi")]
    pub last_funding_index: i128,
}

impl PerpPosition {
    /// Single source of truth for the margin-reservation fields.
    ///
    /// `buy_notional` / `sell_notional` are each side's open-order opening
    /// notional at the current position; `c_notional` is the **flip-aware**
    /// worst-case reservation notional `max(S + B', B + S')` produced by
    /// [`crate::perp_dex::math::calc_reservation_notionals`], which accounts for
    /// a position sign-flip when one side of the book fully fills. Writes:
    /// - per-side `*_reserved_notional` = each side's notional (informational),
    /// - per-side `*_margin_reserved`   = notional / leverage (informational;
    ///   used only as a cancel-ordering heuristic),
    /// - `margin_reserved_notional`     = `c_notional`,
    /// - `margin_reserved`              = `c_notional / leverage` — the capital
    ///   actually locked. A single floor of the combined leg (not a sum of
    ///   per-leg floors), so it never under-reserves; and `c_notional ≥
    ///   max(buy_notional, sell_notional)`, so it is always ≥ the old
    ///   max-of-side reservation.
    ///
    /// Leverage is floored at 1. Callers compute the wallet delta from the
    /// change in `margin_reserved` around this call (NOT from the per-side
    /// fields — those lag `margin_reserved` under the flip-aware model) and own
    /// `fee_reserved` separately.
    pub(crate) fn set_reservations(
        &mut self,
        buy_notional: u64,
        sell_notional: u64,
        c_notional: u64,
        leverage: u64,
    ) {
        let lev = leverage.max(1);
        self.buy_side_reserved_notional = buy_notional;
        self.sell_side_reserved_notional = sell_notional;
        self.buy_side_margin_reserved = buy_notional / lev;
        self.sell_side_margin_reserved = sell_notional / lev;
        self.margin_reserved_notional = c_notional;
        self.margin_reserved = c_notional / lev;
    }
}

impl Default for PerpPosition {
    fn default() -> Self {
        Self {
            amount: 0,
            v_quote_balance: 0,
            margin: 0,
            margin_reserved: 0,
            margin_reserved_notional: 0,
            buy_side_margin_reserved: 0,
            buy_side_reserved_notional: 0,
            sell_side_margin_reserved: 0,
            sell_side_reserved_notional: 0,
            fee_reserved: 0,
            leverage: 1,
            last_funding_index: 0,
        }
    }
}

// ── Market ────────────────────────────────────────────────────────────────

/// Configuration for a single perpetual market, stored on-chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Market {
    pub market_id: u64,
    /// Decimal places for the base asset (e.g. 8 for BTC).
    pub base_decimals: u32,
    /// Decimal places for price representation (e.g. 2 for BTC at $0.01 precision, 18 for meme coins).
    #[serde(default)]
    pub price_decimals: u32,
    /// Minimum price increment (price_decimals fixed-point units).
    pub tick_size: u64,
    /// Minimum quantity increment (base-asset units).
    pub step_size: u64,
    /// Minimum order quantity.
    pub min_quantity: u64,
    /// Maximum order quantity (bounds calc_value to prevent u128 overflow).
    #[serde(default)]
    pub max_quantity: u64,
    /// Maximum order price (price_decimals fixed-point units).
    #[serde(default)]
    pub max_price: u64,
    /// Oracle index price update cadence in seconds.
    #[serde(default)]
    pub price_update_interval: u64,
    /// Whether the market accepts new orders.
    pub active: bool,
    /// Seconds between funding epochs (e.g. 28 800 for 8 h). 0 = funding disabled.
    #[serde(default)]
    pub funding_interval: u64,
    /// Per-epoch interest rate in FUNDING_RATE_ONE units (1e6 = 100%). Default: 100 = 0.01%.
    #[serde(default)]
    pub interest_rate: i64,
    /// Liquidation clearance fee in basis points (1 bps = 0.01%).
    /// Charged from remaining margin on solvent liquidations; credited to Insurance Fund.
    /// 0 = no fee.
    #[serde(default, rename = "lf")]
    pub liquidation_fee_rate_bps: u32,
    /// Price band half-width in basis points (1 bps = 0.01%). A limit order is
    /// rejected at placement if its price lies outside `mark ± price_band_bps`.
    /// `0` means "use `DEFAULT_PRICE_BAND_BPS`"; a large value (e.g. `>= 10_000`)
    /// effectively disables the band. Resolve via `crate::perp_dex::math::effective_price_band_bps`.
    #[serde(default, rename = "pb")]
    pub price_band_bps: u32,
}

/// Per-market HOT scalars, grouped into ONE off-trie blob so the frequently-co-accessed values
/// (matching / BBO / funding / mark-price median) cost a SINGLE map probe + decode + Arc, sharing
/// one cache line — instead of five separate keys (five probes, five decodes, five allocs). All
/// `Copy` u64s, so a whole-struct clone to extract one field is a trivial memcpy. Grouping coarsens
/// the block-delta granularity to per-market (a write to any field re-emits the blob), which is a
/// CHAIN change (commitment bytes regroup) but leaves every value — and thus the business
/// snapshot — identical.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct MarketHot {
    /// Mark price (oracle-driven median input to funding / risk).
    pub mark_price: u64,
    /// Cached best bid (0 = no bids). Kept in sync with the bid price index.
    pub best_bid: u64,
    /// Cached best ask (0 = no asks). Kept in sync with the ask price index.
    pub best_ask: u64,
    /// Last traded ("contract") price — an input to the mark-price median.
    pub last_traded: u64,
    /// Open interest (base-asset units).
    pub open_interest: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_reservations_writes_fields_from_flip_aware_notional() {
        let mut p = PerpPosition::default();
        // c_notional (1500) is the flip-aware reservation; per-side fields stay
        // informational (each = side_notional / leverage).
        p.set_reservations(1000, 400, 1500, 5);
        assert_eq!(p.buy_side_reserved_notional, 1000);
        assert_eq!(p.sell_side_reserved_notional, 400);
        assert_eq!(p.buy_side_margin_reserved, 200); // 1000 / 5 (informational)
        assert_eq!(p.sell_side_margin_reserved, 80); // 400 / 5  (informational)
        assert_eq!(p.margin_reserved_notional, 1500); // the flip-aware notional
        assert_eq!(p.margin_reserved, 300); // 1500 / 5, single floor of the combined leg
    }

    #[test]
    fn set_reservations_floors_zero_leverage_to_one() {
        let mut p = PerpPosition::default();
        p.set_reservations(1000, 0, 1000, 0);
        assert_eq!(p.buy_side_margin_reserved, 1000); // 1000 / max(0, 1)
        assert_eq!(p.margin_reserved, 1000); // c_notional / max(0, 1)
    }

    #[test]
    fn set_reservations_zeroes_all_fields_on_zero_notional() {
        let mut p = PerpPosition::default();
        p.set_reservations(500, 500, 800, 5);
        p.set_reservations(0, 0, 0, 5);
        assert_eq!(p.buy_side_reserved_notional, 0);
        assert_eq!(p.sell_side_reserved_notional, 0);
        assert_eq!(p.margin_reserved, 0);
        assert_eq!(p.margin_reserved_notional, 0);
    }
}
