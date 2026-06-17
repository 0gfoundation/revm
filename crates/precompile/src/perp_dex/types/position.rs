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
    /// Current leverage setting (1–20).
    #[serde(rename = "lv")]
    pub leverage: u64,
    /// Cumulative funding index at this position's last funding settlement.
    /// Funding owed = `amount × (market cumulative_funding_index − this)`,
    /// settled lazily on every position-touching op. See the `funding` module.
    #[serde(default, rename = "fi")]
    pub last_funding_index: i128,
}

impl PerpPosition {
    /// Single source of truth for the six per-side margin-reservation fields.
    ///
    /// Given each side's open-order notional and the position leverage, writes
    /// all six fields coherently: each side's `*_margin_reserved =
    /// notional / leverage` (leverage floored at 1), and the position-level
    /// `margin_reserved` / `margin_reserved_notional` = the max across the two
    /// sides — hedged orders share collateral, so only the larger side needs
    /// margin. Callers compute the wallet delta from the change in
    /// `margin_reserved` around this call, and own `fee_reserved` separately.
    pub(crate) fn set_reservations(&mut self, buy_notional: u64, sell_notional: u64, leverage: u64) {
        let lev = leverage.max(1);
        self.buy_side_reserved_notional = buy_notional;
        self.sell_side_reserved_notional = sell_notional;
        self.buy_side_margin_reserved = buy_notional / lev;
        self.sell_side_margin_reserved = sell_notional / lev;
        self.margin_reserved_notional = buy_notional.max(sell_notional);
        self.margin_reserved = self
            .buy_side_margin_reserved
            .max(self.sell_side_margin_reserved);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_reservations_writes_all_six_fields_with_max_of_side() {
        let mut p = PerpPosition::default();
        p.set_reservations(1000, 400, 5);
        assert_eq!(p.buy_side_reserved_notional, 1000);
        assert_eq!(p.sell_side_reserved_notional, 400);
        assert_eq!(p.buy_side_margin_reserved, 200); // 1000 / 5
        assert_eq!(p.sell_side_margin_reserved, 80); // 400 / 5
        assert_eq!(p.margin_reserved_notional, 1000); // max(1000, 400)
        assert_eq!(p.margin_reserved, 200); // max(200, 80)
    }

    #[test]
    fn set_reservations_floors_zero_leverage_to_one() {
        let mut p = PerpPosition::default();
        p.set_reservations(1000, 0, 0);
        assert_eq!(p.buy_side_margin_reserved, 1000); // 1000 / max(0, 1)
        assert_eq!(p.margin_reserved, 1000);
    }

    #[test]
    fn set_reservations_zeroes_all_fields_on_zero_notional() {
        let mut p = PerpPosition::default();
        p.set_reservations(500, 500, 5);
        p.set_reservations(0, 0, 5);
        assert_eq!(p.buy_side_reserved_notional, 0);
        assert_eq!(p.sell_side_reserved_notional, 0);
        assert_eq!(p.margin_reserved, 0);
        assert_eq!(p.margin_reserved_notional, 0);
    }
}
