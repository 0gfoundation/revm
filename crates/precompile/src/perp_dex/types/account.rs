//! User account stored in the PerpDEX precompile.
use serde::{Deserialize, Serialize};

use crate::{as_bin::AsBinStr, perp_dex::errors::perp_err, PrecompileError};

/// Maximum positive balance accepted by the signed perp wallet.
pub const MAX_PERP_WALLET_BALANCE: u64 = i64::MAX as u64;

/// On-chain record for a single user's DEX account.
///
/// * `usdc_balance`        – U256 USDC units held in the DEX (deposit/withdraw layer).
/// * `perp_wallet_balance` – signed USDC units available for trading (6-decimal fixed-point).
///
/// Serialised as a MessagePack struct-map so future fields can be added without
/// breaking existing storage (same pattern as `wa0gi_base::MinterSupply`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserAccount {
    /// Spot / withdrawal layer balance (stored as decimal string for U256 range).
    #[serde(rename = "UB")]
    pub usdc_balance: AsBinStr,

    /// Perp trading wallet. Internally signed so maker settlement can carry a
    /// temporary deficit until liquidation/bankruptcy handling is added.
    #[serde(rename = "PB")]
    pub perp_wallet_balance: i64,
}

impl Default for UserAccount {
    fn default() -> Self {
        Self {
            usdc_balance: "0".into(),
            perp_wallet_balance: 0,
        }
    }
}

impl UserAccount {
    /// Balance exposed through the existing ABI. Negative internal balances are
    /// reported as zero until liquidation/bankruptcy handling is wired.
    pub fn visible_perp_wallet_balance(&self) -> u64 {
        if self.perp_wallet_balance <= 0 {
            0
        } else {
            self.perp_wallet_balance as u64
        }
    }

    /// Returns whether the wallet can cover a user-initiated debit.
    pub fn has_available_perp(&self, amount: u64) -> bool {
        match i64::try_from(amount) {
            Ok(amount) => self.perp_wallet_balance >= amount,
            Err(_) => false,
        }
    }

    /// Adds positive perp wallet balance.
    pub fn credit_perp(&mut self, amount: u64) -> Result<(), PrecompileError> {
        let amount =
            i64::try_from(amount).map_err(|_| perp_err("perp wallet: amount exceeds i64::MAX"))?;
        self.perp_wallet_balance = self
            .perp_wallet_balance
            .checked_add(amount)
            .ok_or_else(|| perp_err("perp wallet: balance overflow"))?;
        Ok(())
    }

    /// Debits perp wallet balance, allowing the internal value to go negative.
    pub fn debit_perp(&mut self, amount: u64) -> Result<(), PrecompileError> {
        let amount =
            i64::try_from(amount).map_err(|_| perp_err("perp wallet: amount exceeds i64::MAX"))?;
        self.perp_wallet_balance = self
            .perp_wallet_balance
            .checked_sub(amount)
            .ok_or_else(|| perp_err("perp wallet: balance underflow"))?;
        Ok(())
    }
}

/// Registered ed25519 API key for a user.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiKey {
    /// Raw 32-byte ed25519 public key.
    #[serde(rename = "K")]
    pub pubkey: [u8; 32],
    /// Unix-second expiry timestamp. `0` means the key never expires.
    #[serde(rename = "E")]
    pub expiry: u64,
}
