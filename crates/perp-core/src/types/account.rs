//! User account stored in the PerpDEX precompile.
use primitives::U256;
use serde::{Deserialize, Serialize};

use crate::{as_bin::AsBinStr, error::{perp_err, PerpError}};

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
    // NOTE: the former "TC" (`total_perp_collateral`) aggregate is GONE. It was
    // `wallet + Σ_positions(margin + margin_reserved)` — fully derivable from state
    // that is already published, used by no protocol rule, yet incrementally maintained on the
    // hottest write paths (an extra account read + clone + write per order rest/cancel). Consumers
    // that want it compute it off-chain from `getAccount` + `getPosition`.
}

impl Default for UserAccount {
    fn default() -> Self {
        Self {
            usdc_balance: "0".into(),
            perp_wallet_balance: 0,
            maker_fee_bps: 0,
            taker_fee_bps: 0,
            nonce: 0,
        }
    }
}

/// Public account values emitted by the precompile and returned by `getAccount`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublicAccountBalance {
    /// Spot USDC held inside the DEX.
    pub usdc_balance: U256,
    /// Perp collateral currently available for trading or transfer.
    pub available_perp_balance: u64,
}

impl UserAccount {
    /// Available balance exposed through the ABI. Negative internal balances are
    /// reported as zero until liquidation/bankruptcy handling is wired.
    #[inline]
    pub fn visible_perp_wallet_balance(&self) -> u64 {
        if self.perp_wallet_balance <= 0 {
            0
        } else {
            self.perp_wallet_balance as u64
        }
    }

    /// Returns the clamped public balance after-image for this account.
    #[inline]
    pub fn public_balance(&self) -> PublicAccountBalance {
        PublicAccountBalance {
            usdc_balance: self.usdc_balance.clone().into(),
            available_perp_balance: self.visible_perp_wallet_balance(),
        }
    }

    /// Returns whether the wallet can cover a user-initiated debit of `amount`.
    ///
    /// # Invariant
    ///
    /// **A risk-reducing or zero-cost action must never be gated on a balance the
    /// user does not need.**
    ///
    /// `perp_wallet_balance` is deliberately SIGNED and can legitimately be
    /// negative — a close-path fee, a funding charge, or a maker settlement
    /// deficit can drive it below zero. A debit of **zero** is therefore always
    /// affordable: nothing is being taken from the wallet, so there is nothing to
    /// afford. Without the `amount == 0` arm the comparison is `-5 >= 0` ==
    /// `false`, and a negative-balance user is refused precisely the actions that
    /// would REDUCE their risk — a pure-reduce order (whose flip-aware
    /// reservation delta is 0), and a close (whose `total_required` is 0 whenever
    /// the fill opens nothing and the taker fee is covered).
    ///
    /// This widens no funding hole: every NON-zero debit is still refused unless
    /// the signed balance covers it in full, and a debit above `i64::MAX` is
    /// still refused outright.
    #[inline]
    pub fn has_available_perp(&self, amount: u64) -> bool {
        if amount == 0 {
            return true;
        }
        match i64::try_from(amount) {
            Ok(amount) => self.perp_wallet_balance >= amount,
            Err(_) => false,
        }
    }

    /// Adds positive perp wallet balance.
    #[inline]
    pub fn credit_perp(&mut self, amount: u64) -> Result<(), PerpError> {
        let amount =
            i64::try_from(amount).map_err(|_| perp_err("perp wallet: amount exceeds i64::MAX"))?;
        self.perp_wallet_balance = self
            .perp_wallet_balance
            .checked_add(amount)
            .ok_or_else(|| perp_err("perp wallet: balance overflow"))?;
        Ok(())
    }

    /// Debits perp wallet balance, allowing the internal value to go negative.
    #[inline]
    pub fn debit_perp(&mut self, amount: u64) -> Result<(), PerpError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(perp_wallet_balance: i64) -> UserAccount {
        UserAccount {
            perp_wallet_balance,
            ..UserAccount::default()
        }
    }

    /// REGRESSION (B1). Pinned bug: `has_available_perp(0)` was `-5 >= 0` ==
    /// `false`, so a NEGATIVE wallet refused a debit of ZERO — blocking the
    /// risk-REDUCING actions (pure-reduce placement, close) whose required debit
    /// is exactly 0.
    #[test]
    fn a_zero_debit_is_affordable_at_any_balance_including_negative() {
        for balance in [i64::MIN, -1_000_000, -5, -1, 0, 1, i64::MAX] {
            assert!(
                acct(balance).has_available_perp(0),
                "a zero debit must be affordable at balance {balance}"
            );
        }
    }

    /// The fix must not open a hole: every non-zero debit keeps the old rule.
    #[test]
    fn a_nonzero_debit_still_requires_the_balance_to_cover_it() {
        assert!(!acct(-5).has_available_perp(1));
        assert!(!acct(-5).has_available_perp(u64::MAX));
        assert!(!acct(0).has_available_perp(1));
        assert!(!acct(9).has_available_perp(10));
        assert!(acct(10).has_available_perp(10), "the >= boundary is unchanged");
        assert!(acct(11).has_available_perp(10));
        // Above i64::MAX is still refused outright, even from a maximal balance.
        assert!(!acct(i64::MAX).has_available_perp(i64::MAX as u64 + 1));
        assert!(acct(i64::MAX).has_available_perp(i64::MAX as u64));
    }
}
