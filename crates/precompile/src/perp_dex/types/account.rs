//! User account stored in the PerpDEX precompile.
use serde::{Deserialize, Serialize};

use crate::as_bin::AsBinStr;

/// On-chain record for a single user's DEX account.
///
/// * `usdc_balance`        – U256 USDC units held in the DEX (deposit/withdraw layer).
/// * `perp_wallet_balance` – u64 USDC units available for trading (6-decimal fixed-point).
///
/// Serialised as a MessagePack struct-map so future fields can be added without
/// breaking existing storage (same pattern as `wa0gi_base::MinterSupply`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserAccount {
    /// Spot / withdrawal layer balance (stored as decimal string for U256 range).
    #[serde(rename = "UB")]
    pub usdc_balance: AsBinStr,

    /// Perp trading wallet (u64, USDC with 6 decimals, max ~1.8 × 10¹³).
    #[serde(rename = "PB")]
    pub perp_wallet_balance: u64,
}

impl Default for UserAccount {
    fn default() -> Self {
        Self {
            usdc_balance: "0".into(),
            perp_wallet_balance: 0,
        }
    }
}
