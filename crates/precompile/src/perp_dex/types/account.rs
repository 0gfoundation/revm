//! User account stored in the PerpDEX precompile.
use primitives::U256;
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

    // ── Folded per-user scalars (were separate off-trie keys) ──────────────────
    // Maker/taker fee bps + the order-id nonce are per-USER (like the account) and are read
    // TOGETHER with the account on the hot placement path, so they ride in the account blob — one
    // probe/decode instead of three. Appended positionally (`#[serde(default)]` so a shorter blob
    // still decodes); read via `load_account_ref` (Arc, no usdc_balance String clone).
    /// Maker fee in basis points (order-entry fee rate).
    #[serde(rename = "MF", default)]
    pub maker_fee_bps: u64,
    /// Taker fee in basis points.
    #[serde(rename = "TF", default)]
    pub taker_fee_bps: u64,
    /// Monotonic per-user nonce used to derive order ids (`keccak(account ‖ nonce)`).
    #[serde(rename = "NO", default)]
    pub nonce: u64,

    /// Total perp collateral allocated to this account: available wallet plus
    /// position margin, order-margin reservation, and fee reservation.
    /// Unrealized PnL is deliberately excluded.
    #[serde(rename = "TC", default)]
    pub total_perp_collateral: i128,
}

impl Default for UserAccount {
    fn default() -> Self {
        Self {
            usdc_balance: "0".into(),
            perp_wallet_balance: 0,
            maker_fee_bps: 0,
            taker_fee_bps: 0,
            nonce: 0,
            total_perp_collateral: 0,
        }
    }
}

/// Public account values emitted by the precompile and returned by `getAccount`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublicAccountBalance {
    /// Spot USDC held inside the DEX.
    pub usdc_balance: U256,
    /// Total perp collateral excluding unrealized PnL.
    pub total_perp_collateral: U256,
    /// Perp collateral currently available for trading or transfer.
    pub available_perp_balance: u64,
}

impl UserAccount {
    /// Available balance exposed through the ABI. Negative internal balances are
    /// reported as zero until liquidation/bankruptcy handling is wired.
    pub fn visible_perp_wallet_balance(&self) -> u64 {
        if self.perp_wallet_balance <= 0 {
            0
        } else {
            self.perp_wallet_balance as u64
        }
    }

    /// Total collateral exposed through the ABI. A negative internal value is
    /// never public and indicates an account awaiting bankruptcy handling.
    pub fn visible_total_perp_collateral(&self) -> U256 {
        u128::try_from(self.total_perp_collateral)
            .map(U256::from)
            .unwrap_or_default()
    }

    /// Returns the clamped public balance after-image for this account.
    pub fn public_balance(&self) -> PublicAccountBalance {
        PublicAccountBalance {
            usdc_balance: self.usdc_balance.clone().into(),
            total_perp_collateral: self.visible_total_perp_collateral(),
            available_perp_balance: self.visible_perp_wallet_balance(),
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
    /// Raw 32-byte ed25519 public key. `serde_bytes` → msgpack bin instead of a 32-integer array
    /// (P4/#20). (The `rename` is now inert under positional encoding but kept harmlessly.)
    #[serde(rename = "K", with = "serde_bytes")]
    pub pubkey: [u8; 32],
    /// Unix-second expiry timestamp. `0` means the key never expires.
    #[serde(rename = "E")]
    pub expiry: u64,
}
