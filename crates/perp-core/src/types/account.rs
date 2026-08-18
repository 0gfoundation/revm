//! User account stored in the PerpDEX precompile.
use primitives::U256;
use serde::{Deserialize, Serialize};

use crate::{as_bin::AsBinStr, error::{perp_err, PerpError}};

/// Maximum positive balance accepted by the signed perp wallet.
pub const MAX_PERP_WALLET_BALANCE: u64 = i64::MAX as u64;

/// Maximum number of markets one user may be **active** in simultaneously — active meaning a
/// non-zero position OR at least one resting order (the per-user market index, `umkt`).
///
/// Bounded for the same reason as [`MAX_MARGIN_TIERS`](crate::types::MAX_MARGIN_TIERS) and the
/// `MAX_BATCH_*` caps: every table an untrusted caller can grow has to have a ceiling, or one
/// account can make a single blob — and every read of it — arbitrarily expensive.
///
/// **16.** Two things set the number:
/// * *What it costs.* The index exists so `available = perp_wallet_balance − Σ_markets ooIM`
///   can be evaluated (the derived-ooIM work); that sum is 2 loads per member market, so the cap
///   is the worst-case fan-out of an admission check — 32 loads at 16, the same order as one
///   `MAX_BATCH_PLACE` (64) batch already pays. The stored blob is ≤ 16 × u64 ≈ 145 bytes.
/// * *What it must not block.* A user is only counted while they hold a position or a live order,
///   and leaving a market frees the slot immediately, so 16 is 16 *concurrent* exposures — well
///   past any realistic single-account book on a venue with a handful of listed markets.
///
/// Raising it is a pure constant change (no stored layout depends on it); the reject it drives is
/// `placeOrder: user market limit reached`, refused BEFORE any write.
pub const MAX_USER_MARKETS: usize = 16;

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

    /// Perp trading wallet — the CROSS wallet (Binance's `crossWalletBalance`): position margin has
    /// been physically moved out of it, the derived open-order requirement has NOT.
    ///
    /// **Signed on purpose, as the Binance-aligned representation** — not as a placeholder awaiting
    /// bankruptcy handling. Binance's `availableBalance` has a true value that goes NEGATIVE while
    /// the reported field is clamped at 0 (measured: reported `0.00000000` against a back-solved
    /// `−0.00088443`), and an already-resting order is left `status = 'NEW'` at negative headroom
    /// while a NEW order is refused `-2019` in the same instant — the exchange lets a lien be
    /// under-covered and never sweeps it. `perp_wallet_balance: i64` plus
    /// [`UserAccount::visible_perp_wallet_balance`] (clamped at the ABI boundary) is that structure
    /// exactly, and is the working model **M1′** of `misc/binance-flip-and-admission.md` §3.3, whose
    /// instruction to implementers is literally "do not change it".
    ///
    /// 🔶 §3.3 marks M1′ a CONJECTURE (the author is ~50/50 between it and M1, where the position
    /// silo is funded short and the wallet stays at 0), pending a v4 experiment. What is MEASURED is
    /// only that a negative true value is representable and that under-coverage is never swept. A
    /// maker fill whose wallet cannot cover the opening margin therefore fills and drives this
    /// negative (`trading::settlement::settle_maker_fill_core`).
    ///
    /// # A negative value is a RECEIVABLE, not a loss — and NOT protocol bad debt
    ///
    /// This has been settled by construction, not by argument. `risk::tests::usdc_custody` builds
    /// the worst end state through real calls — an underfunded maker fill drives this field
    /// negative, and the position it funded is then liquidated INSOLVENT so the Insurance Fund
    /// covers the part the silo could not — and asserts the full custody identity against the
    /// on-trie ERC-20 USDC the precompile really holds:
    ///
    /// ```text
    /// erc20(USDC, PERP_DEX) == Σ usdc_balance + Σ perp_wallet_balance + Σ position.margin
    ///                          + insurance_fund + Σ (v_quote + signed_value(mark, amount))
    /// ```
    ///
    /// It closes EXACTLY with the negative term in it. Three consequences, each pinned by that
    /// test:
    ///
    /// * **Nothing is over-promised.** The deficit enters the identity as a NEGATIVE claim, so the
    ///   sum of what everyone can withdraw is still ≤ what the DEX custodies. Clamping the field
    ///   at 0 is what breaks the identity — by exactly the deficit. The negative sign is
    ///   load-bearing accounting, not a placeholder.
    /// * **It cannot be walked away from through the perp layer.** `available` is
    ///   `perp_wallet_balance − Σ ooIM`, so a negative wallet refuses every money-out gate that
    ///   reads it (`transferFromPerp`, `addPositionMargin`, `depositInsuranceFund`) and every
    ///   risk-increasing admission; and because the deficit is ONE signed field, the next deposit
    ///   NETS against it automatically rather than landing in a fresh spendable bucket.
    /// * **The deficit is NOT double-counted with the Insurance Fund.** They are disjoint slices of
    ///   one loss: the wallet went negative to FUND `position.margin`, and the fund absorbs only
    ///   what the realized loss exceeded that margin by (`apply_position_fill` /
    ///   `settle_liquidation_residual_at_mark_price` never debit a wallet — isolated margin). The
    ///   test pins the exact split: own deposited cash + receivable + IF absorption ==
    ///   the whole realized loss, to the unit.
    ///
    /// ## ⛔ Do NOT "fix" this by absorbing the deficit from the Insurance Fund
    ///
    /// It is the change this shape invites, and it is strictly worse than leaving the deficit
    /// alone. Custody would still balance (the fund falls, the wallet rises), so the conservation
    /// gates would not object — but it FORGIVES a debt the protocol can still collect, turning a
    /// receivable into a socialised write-off and letting a user go negative and walk away with the
    /// fund eating it. It also creates the very double count the audit worried about: with that fix
    /// wired up, the test above measures the fund absorbing the beyond-margin shortfall PLUS the
    /// receivable. Two assertions there fail on purpose if anyone adds it. Leave them failing.
    ///
    /// Binance's behaviour is the same: an under-covered lien is left under-covered and never
    /// swept (`misc/binance-margin-verified-model.md` §1.6 — at
    /// `crossWalletBalance − totalOpenOrderInitialMargin = −0.00085981` an already-resting order
    /// stayed `status = 'NEW'` for the whole observation window while a NEW order was refused
    /// `-2019` in the same instant; the doc's instruction to implementers is that no mid-life
    /// teardown logic is needed).
    ///
    /// ## Observability
    ///
    /// The true signed value IS readable through the ABI: `getAccountMargin` returns
    /// `int64 walletBalance` and `int64 availableBalance` unclamped. Only the `uint64` surfaces —
    /// `getAccount().availablePerpBalance` and the `AccountBalanceChanged` after-image, both via
    /// [`UserAccount::visible_perp_wallet_balance`] — floor at 0, matching Binance's own clamped
    /// `availableBalance` (`misc/binance-v3-account-balance-field-reference.md`, the
    /// `availableBalance` row / R5: computed `−0.00085981`, reported `0.00000000`). An operator
    /// watching only the EVENT stream therefore cannot see an accumulating deficit and must poll
    /// `getAccountMargin`.
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
    // `wallet + Σ_positions(margin + the since-deleted margin_reserved)` — fully derivable from state
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

/// Public account values emitted by the precompile in `AccountBalanceChanged`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublicAccountBalance {
    /// Spot USDC held inside the DEX.
    pub usdc_balance: U256,
    /// The CROSS perp wallet, clamped at 0 — Binance's `crossWalletBalance`.
    ///
    /// NOT spendable headroom: the open-order requirement (`Σ ooIM`) is derived, never debited,
    /// so it is still sitting inside this number. The spendable figure is
    /// `getAccount().availablePerpBalance`. This after-image deliberately reports the STORED
    /// balance rather than the derived available, because it is emitted at the account write
    /// site — mid-operation, before the position and order-list writes of the same call — where a
    /// derived number would be computed against half-updated state and would also cost a
    /// `Σ ooIM` walk on the hottest write path.
    pub perp_wallet_balance: u64,
}

impl UserAccount {
    /// The CROSS perp wallet clamped at 0, as exposed in `AccountBalanceChanged`.
    ///
    /// The clamp exists because the ABI field is `uint64`, NOT because a negative balance is an
    /// unfinished state awaiting a bankruptcy subsystem: a negative
    /// [`UserAccount::perp_wallet_balance`] is a settled, self-consistent RECEIVABLE (see that
    /// field's docs). Binance clamps the analogous `availableBalance` the same way — measured
    /// reporting `0.00000000` against a true `−0.00085981`
    /// (`misc/binance-v3-account-balance-field-reference.md`, the `availableBalance` row / R5).
    ///
    /// ⚠️ Do NOT reach for this in engine logic. It is a REPORTING projection only, and the
    /// clamped view is exactly what breaks the custody identity: summing wallets through this
    /// function over-states the protocol's liabilities by the size of every deficit
    /// (`risk::tests::usdc_custody` measures that directly). Gates use
    /// `margin_view::derived_available_balance`, which is signed.
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
            perp_wallet_balance: self.visible_perp_wallet_balance(),
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

    /// The signed wallet passes NEGATIVE values through to the ledger and clamps only the
    /// public view. A close-path fee, a funding charge or a maker settlement deficit can each
    /// drive it below zero, and the clamp must not hide that from `credit_perp`/`debit_perp`.
    #[test]
    fn a_negative_wallet_is_reported_as_zero_but_kept_internally() {
        let a = acct(-5);
        assert_eq!(a.visible_perp_wallet_balance(), 0);
        assert_eq!(a.perp_wallet_balance, -5);
        assert_eq!(acct(10).visible_perp_wallet_balance(), 10);
        assert_eq!(acct(0).visible_perp_wallet_balance(), 0);
    }

    /// `debit_perp` is a pure ledger move with NO affordability rule of its own — the caller
    /// gates. (It used to have a companion `has_available_perp(amount)`, which asked
    /// `perp_wallet_balance >= amount`. That question is wrong now: the wallet is the CROSS
    /// wallet and still holds the collateral backing every resting order, so the gate is
    /// `margin_view::derived_available_balance` and its `derived_can_afford`. The predicate was
    /// deleted rather than left lying around for someone to reuse.)
    #[test]
    fn debit_and_credit_are_pure_ledger_moves_that_allow_a_negative_balance() {
        let mut a = acct(10);
        a.debit_perp(25).unwrap();
        assert_eq!(a.perp_wallet_balance, -15, "no affordability rule here");
        a.credit_perp(5).unwrap();
        assert_eq!(a.perp_wallet_balance, -10);
        // Above i64::MAX is still refused outright rather than wrapping.
        assert!(acct(0).debit_perp(i64::MAX as u64 + 1).is_err());
        assert!(acct(i64::MAX).credit_perp(1).is_err());
    }
}
