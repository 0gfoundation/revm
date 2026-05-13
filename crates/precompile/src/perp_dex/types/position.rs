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
    #[serde(rename = "fr")]
    pub fee_reserved: u64,
    /// Current leverage setting (1–20).
    #[serde(rename = "lv")]
    pub leverage: u64,
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
    pub price_decimals: u32,
    /// Minimum price increment (price_decimals fixed-point units).
    pub tick_size: u64,
    /// Minimum quantity increment (base-asset units).
    pub step_size: u64,
    /// Minimum order quantity.
    pub min_quantity: u64,
    /// Maximum order quantity (bounds calc_value to prevent u128 overflow).
    pub max_quantity: u64,
    /// Maximum order price (price_decimals fixed-point units).
    pub max_price: u64,
    /// Oracle index price update cadence in seconds.
    pub price_update_interval: u64,
    /// Whether the market accepts new orders.
    pub active: bool,
    /// Seconds between funding epochs (e.g. 28 800 for 8 h). 0 = funding disabled.
    #[serde(default)]
    pub funding_interval: u64,
    /// Per-epoch interest rate in FUNDING_RATE_ONE units (1e6 = 100%). Default: 100 = 0.01%.
    #[serde(default)]
    pub interest_rate: i64,
}
