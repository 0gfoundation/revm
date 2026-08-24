//! User account stored in the PerpDEX precompile.
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
    /// **Signed on purpose** — not as a placeholder awaiting bankruptcy handling, but because one
    /// narrow path can still legitimately drive it under zero (below).
    ///
    /// ⚠️ **This used to be documented as "the Binance-aligned representation". That claim was too
    /// strong and has been withdrawn.** Binance's `availableBalance` does have a true value that
    /// goes negative while the reported field is clamped at 0 (measured: reported `0.00000000`
    /// against a back-solved `−0.00088443`), so the SHAPE — a genuinely signed internal balance —
    /// is real. (We no longer copy the ABI-side floor at all; see the Observability section below.)
    /// What is not aligned is what the
    /// negative MEANS. R11 measured Binance's negative appearing only when the fill COMMISSION could
    /// not be paid, at exactly the commission (`−0.25717240`), and cleared by the insurance fund
    /// three seconds later (`derived-ooim-plan.md` §3a). This field used to represent something else
    /// entirely: a PERSISTENT margin deficit, of unbounded size, that nothing in the engine cleared.
    ///
    /// That difference is gone on our side too. An underfunded maker fill no longer funds the silo in
    /// full and drives this negative (model M1′, adopted from a conjecture §3.3 marked ~50/50); it
    /// funds `pos.margin` with `min(requirement, cash at hand)` and leaves the SILO short — model
    /// **M1**, which R11 measured (`trading::settlement::settle_maker_fill_core`). The deficit is
    /// therefore attached to the POSITION, and closing or liquidating that position resolves it
    /// through the existing bad-debt → insurance-fund path.
    ///
    /// # When this is still negative, and what it means then
    ///
    /// One producer remains: a maker fee the fill's (capped) opening margin could not absorb —
    /// `fee_from_wallet` in `settle_maker_fill_core` — chiefly on a PURE CLOSE, which has no opening
    /// margin at all, whose own proceeds an insolvent close consumed. It is bounded by that fee.
    /// Pinned by `trading::tests::a_maker_close_fee_is_the_only_remaining_way_to_a_negative_wallet`,
    /// which also enumerates why no other path can get here (every money-out gate and the taker fill
    /// are gated on `available ≤ wallet`; funding settles into `pos.margin`; the clearance fee is
    /// `.min(perp_wallet_balance.max(0))`; residual settlement and ADL only credit).
    ///
    /// While negative it is a RECEIVABLE, not a loss, and the machinery for that is unchanged:
    ///
    /// * **Nothing is over-promised.** The deficit enters the custody identity as a NEGATIVE claim,
    ///   so the sum of what everyone can withdraw is still ≤ what the DEX custodies. Clamping the
    ///   field at 0 is what breaks the identity — by exactly the deficit. The negative sign is
    ///   load-bearing accounting, not a placeholder.
    /// * **It cannot be walked away from through the perp layer.** `available` is
    ///   `perp_wallet_balance − Σ ooIM`, so a negative wallet refuses every money-out gate that
    ///   reads it (`transferFromPerp`, `addPositionMargin`, `depositInsuranceFund`) and every
    ///   risk-increasing admission; and because the deficit is ONE signed field, the next deposit
    ///   NETS against it automatically rather than landing in a fresh spendable bucket.
    /// * **It is NOT double-counted with the Insurance Fund.** Disjoint slices: the wallet only ever
    ///   goes negative to pay the FEE RECIPIENT, and the fund absorbs only what a realized loss
    ///   exceeded the position's margin by (`apply_position_fill` /
    ///   `settle_liquidation_residual_at_mark_price` never debit a wallet — isolated margin).
    ///
    /// # The custody identity
    ///
    /// `risk::tests::usdc_custody` asserts, through real calls and across an underfunded maker fill,
    /// an insolvent liquidation and a later deposit, that the on-trie ERC-20 USDC the precompile
    /// really holds equals the sum of every internal claim:
    ///
    /// ```text
    /// erc20(USDC, PERP_DEX) == Σ usdc_balance + Σ perp_wallet_balance + Σ position.margin
    ///                          + insurance_fund + Σ (v_quote + signed_value(mark, amount))
    /// ```
    ///
    /// The M1 switch needed NO new term in it: a short silo means `Σ position.margin` is lower and
    /// `Σ perp_wallet_balance` higher by the same amount, and the identity sums both.
    ///
    /// ## ⛔ Do NOT "fix" a negative value by absorbing it from the Insurance Fund
    ///
    /// It is the change this shape invites, and it is strictly worse than leaving it alone. Custody
    /// would still balance (the fund falls, the wallet rises), so the conservation gates would not
    /// object — but it FORGIVES a debt the protocol can still collect, turning a receivable into a
    /// socialised write-off. The same applies to the fill-time silo shortfall: the position may yet
    /// close in profit, and the fund already absorbs the beyond-silo slice if it does not. Assertions
    /// in `usdc_custody` fail on purpose if anyone adds either. Leave them failing.
    ///
    /// ## Observability
    ///
    /// The true signed value is readable through EVERY published surface — there is no clamped one
    /// left. Both account views (`getAccount(address)`, index-driven, and
    /// `getAccountMargin(address, uint64[])`) return `int64 totalCrossWalletBalance` and
    /// `int64 availableBalance` unclamped, AND so does the `AccountBalanceChanged` event, whose
    /// `int64 totalCrossWalletBalance` is this field verbatim. (The event carries only the three
    /// BALANCES — the account-level margin totals around them are `getAccount`-only, the way Binance
    /// keeps them on REST rather than on `ACCOUNT_UPDATE`.) An operator watching only the EVENT stream can therefore
    /// see a deficit appear, which used not to be true: the event projected the wallet through a
    /// `uint64` floored at 0 (via [`UserAccount::visible_perp_wallet_balance`], now test-only), the
    /// way Binance's own clamped `availableBalance` does
    /// (`misc/binance-v3-account-balance-field-reference.md`, the `availableBalance` row / R5:
    /// computed `−0.00085981`, reported `0.00000000`). (`getAccount` used to floor too, under the
    /// name `availablePerpBalance`; that blind spot went when it became the full account-level
    /// roll-up. The event's went with this change.)
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

    /// `Σ pos.margin` over ALL of this user's positions — Binance's
    /// `totalWalletBalance − totalCrossWalletBalance`, i.e. the money physically sitting in the
    /// per-position silos.
    ///
    /// # Why this is STORED and the deleted "TC" was not
    ///
    /// The former `total_perp_collateral` ("TC") aggregate that used to live here was removed for a
    /// reason recorded below the previous field: it was
    /// `wallet + Σ_positions(margin + margin_reserved)` and cost "an extra account read + clone +
    /// write **per order rest/cancel**". **That objection no longer applies, and the reason is the
    /// `margin_reserved` term, not the shape.** `margin_reserved` was the open-order ESCROW, which
    /// moved on every placement and every cancel — so TC had to be re-derived and re-written on the
    /// two hottest paths in the engine. The escrow is GONE (see the note where the six escrow fields
    /// used to be in [`crate::types::PerpPosition`]); the requirement it stood for is DERIVED on read
    /// now. What is left, `Σ pos.margin`, does **not** move on a rest or a cancel — that is exactly
    /// the invariant `storage::save_position_reservation_only` `debug_assert`s — so this field is
    /// maintained only where `pos.margin` itself moves: opens, closes, fills, funding, liquidation,
    /// ADL and `add/removePositionMargin`.
    ///
    /// And on those paths the maintenance is **free**: `storage::save_position` already reads the OLD
    /// position (it needs `old.amount` for the registry / market-index zero-crossing hooks), so
    /// `old.margin` costs nothing and the increment is `pos.margin − old.margin`. TC's cost was a
    /// read it did not otherwise need, on a path that fired constantly.
    ///
    /// Note this stores `Σ pos.margin` and NOT `wb` itself. `wb = perp_wallet_balance + this` is one
    /// addition at read time, and keeping the two apart means their maintenance points stay
    /// decoupled: the wallet moves through `credit_perp`/`debit_perp` on the account, this moves
    /// through `save_position` on a position, and neither has to know about the other.
    ///
    /// # What it buys
    ///
    /// `AccountBalanceChanged` publishes `totalWalletBalance`, which used to cost a walk of the
    /// user's market index (1 index load + 1 position load per member market, up to 17 `_ref` loads
    /// at [`MAX_USER_MARKETS`]). The emit path is now ONE account load and an addition — zero index
    /// loads, zero position loads. See `margin_view::index_account_wallet_balances`.
    ///
    /// # The risk, and the guard
    ///
    /// A derived quantity cannot drift; a stored one can. `save_position` is the SINGLE door for every
    /// `pos.margin` write (`TypedPerpStore::set_position` / `position_mut` are reached from nowhere
    /// else in the engine), so there is exactly one maintenance point — and
    /// `margin_view::index_account_scalars` re-walks the user's positions in `debug_assertions` builds
    /// and compares the walk against this field on every `getAccount` and every published snapshot.
    /// A missed maintenance point therefore panics in the test suite rather than silently mis-reporting
    /// a balance. Independently pinned after every operation of a randomised sequence by
    /// `trading::tests::user_market_index::stored_position_margin_is_exactly_the_sum_after_every_operation`.
    ///
    /// ⚠️ `#[serde(default)]` decodes a pre-existing (shorter) account blob as `0`, which is WRONG for
    /// an account that already holds positions — it would under-report `totalWalletBalance` by the
    /// whole silo. That is acceptable ONLY because this is a pre-production chain that is wiped on
    /// every layout change (`BLOCK_COMMITMENT_VERSION` is bumped alongside), so no such blob ever
    /// exists in practice. The `default` is kept for the same reason the three fields above have it:
    /// the positional codec needs the trailing fields to be optional for the type to round-trip a
    /// short array at all. Were such a blob ever loaded, the `debug_assertions` cross-check above WOULD
    /// catch it on the first `getAccount` — but only in a debug build; a release node would report the
    /// wrong balance silently.
    #[serde(rename = "PM", default)]
    pub total_position_margin: i64,
    // NOTE: the former "TC" (`total_perp_collateral`) aggregate is GONE. It was
    // `wallet + Σ_positions(margin + the since-deleted margin_reserved)` — fully derivable from state
    // that is already published, used by no protocol rule, yet incrementally maintained on the
    // hottest write paths (an extra account read + clone + write per order rest/cancel). Consumers
    // that want it compute it off-chain from `getAccount` + `getPosition`. (`total_position_margin`
    // above is NOT that field coming back — see its own note for why the recorded objection, which
    // was about the `margin_reserved` term, no longer applies.)
}

impl Default for UserAccount {
    fn default() -> Self {
        Self {
            usdc_balance: "0".into(),
            perp_wallet_balance: 0,
            maker_fee_bps: 0,
            taker_fee_bps: 0,
            nonce: 0,
            total_position_margin: 0,
        }
    }
}

impl UserAccount {
    /// The CROSS perp wallet clamped at 0 — **a test/diagnostic contrast only; NO published surface
    /// uses it any more.**
    ///
    /// It used to be the projection behind `AccountBalanceChanged.perpWalletBalance` (a `uint64`).
    /// That field is now `int64 totalCrossWalletBalance`, so nothing the precompile publishes
    /// clamps: both account views and the event all report the signed value. Binance clamps its
    /// analogous `availableBalance` — measured reporting `0.00000000` against a true `−0.00085981`
    /// (`misc/binance-v3-account-balance-field-reference.md`, the `availableBalance` row / R5) — and
    /// its own reference doc's verdict is that the field therefore cannot be used to detect
    /// under-coverage. We deliberately did not keep that.
    ///
    /// ⚠️ Do NOT reach for this in engine logic. The clamped view is exactly what breaks the custody
    /// identity: summing wallets through this function over-states the protocol's liabilities by the
    /// size of every deficit
    /// (`trading::tests::a_maker_close_fee_is_the_only_remaining_way_to_a_negative_wallet` produces
    /// the one state where that still bites; `risk::tests::usdc_custody` asserts the two views now
    /// AGREE on the maker-fill path, which is what the M1 switch bought). Gates use
    /// `margin_view::derived_available_balance`, which is signed. What it is still FOR is pinning
    /// that contrast in tests — asserting that a clamped reading would have reported 0 where the
    /// honest surfaces report the deficit.
    #[inline]
    pub fn visible_perp_wallet_balance(&self) -> u64 {
        if self.perp_wallet_balance <= 0 {
            0
        } else {
            self.perp_wallet_balance as u64
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
