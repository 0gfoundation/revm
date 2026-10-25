//! User account stored in the PerpDEX precompile.
use primitives::{keccak256, Address};
use serde::{Deserialize, Serialize};

use crate::{as_bin::AsBinStr, error::{perp_err, PerpError}};

/// Maximum number of sub-accounts one master may create. A fixed protocol constant, deliberately
/// NOT admin-tunable: it bounds the fan-out of every per-family walk (`getSubAccounts`, the
/// `setUserFeeRates` push-down), which is what lets those selectors keep a FLAT gas price.
///
/// A `u8` because it is the type of [`UserAccount::sub_count`] and of the `index` fed to
/// [`derive_sub_account`].
pub const MAX_SUB_ACCOUNTS: u8 = 10;

/// Domain prefix of the sub-account address derivation. Versioned so a future derivation can never
/// collide with this one.
pub const SUB_ACCOUNT_DOMAIN: &[u8] = b"perpdex_subaccount_v1";

/// **The sub-account address of `(master, index)`** — Hyperliquid-style derived identity:
///
/// ```text
/// sub = address( keccak256( "perpdex_subaccount_v1" ‖ master(20) ‖ index(1) )[12..32] )
/// ```
///
/// i.e. the low 20 bytes of the keccak of a 42-byte preimage, exactly like an Ethereum address is
/// the low 20 bytes of a public key's keccak. Pure and dependency-free so a client or harness can
/// reproduce it byte for byte (no RLP, no ABI encoding — a plain concatenation).
///
/// A derived address has NO private key: nobody can sign as it, so it can only ever act through
/// its master (`asSubAccount`). That is the whole security model of a sub-account, and it rests on
/// keccak preimage resistance — finding a key whose address equals a given derived address is a
/// 2^160 search. (An attacker grinding BOTH its own masters and its own keys meets a collision at
/// ~2^80, the CREATE2 bound; the prize is only a key for its OWN sub, i.e. bypassing the family
/// rules for its own funds.)
///
/// `index` is assigned sequentially by `createSubAccount` (0, 1, 2, …); a master's subs are
/// therefore exactly `derive_sub_account(master, i)` for `i < sub_count`, which is why no list of
/// them is stored.
pub fn derive_sub_account(master: Address, index: u8) -> Address {
    const D: usize = SUB_ACCOUNT_DOMAIN.len(); // 21
    let mut preimage = [0u8; D + 20 + 1];
    preimage[..D].copy_from_slice(SUB_ACCOUNT_DOMAIN);
    preimage[D..D + 20].copy_from_slice(master.as_slice());
    preimage[D + 20] = index;
    Address::from_slice(&keccak256(preimage)[12..])
}

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

    /// Perp trading wallet — the CROSS wallet (Binance's `crossWalletBalance`, `W` in the cross
    /// notation): ISOLATED position margin has been physically moved out of it, the derived
    /// open-order requirement has NOT, and every position CROSSED in force settles into it.
    ///
    /// **Signed, and negative is a NORMAL state** (cross margin P2). A CROSSED position has no silo
    /// (`pos.margin == 0`), so its realized PnL, its funding and its fees all land HERE, whatever
    /// their sign (`perp_engine::trading::settlement::apply_position_fill` under `Cross`,
    /// `perp_engine::funding::compute_funding_settlement`): a cross close that loses more than the
    /// wallet holds takes it below zero, and there is no per-fill bad debt to stop it (cross report
    /// G2). That is Binance's cross model — and what makes a negative wallet NOT a fault is that it
    /// is not the number solvency is judged on. The account's cross margin balance
    /// `E = cWB + Σ_C uPnL` is (`cWB = W + Σ_C pending funding`): the other cross positions'
    /// unrealized profit backs a negative `W`, and the account is liquidatable exactly when
    /// `E < Σ_C MM` (`perp_engine::margin_view::cross_fold`; acted on by cross P3).
    ///
    /// The ISOLATED producer from before cross margin is still here, unchanged and still bounded: a
    /// maker fee the fill's (M1-capped) opening margin could not absorb — `fee_from_wallet` in
    /// `settle_maker_fill_core`, chiefly on a pure close whose own proceeds an insolvent close ate
    /// (pinned by `trading::tests::a_maker_close_fee_is_the_only_remaining_way_to_a_negative_wallet`
    /// for an isolated-only account). An ISOLATED loss never reaches the wallet: it is contained in
    /// the silo, and what exceeds the silo is bad debt for the insurance fund. (Binance's R11
    /// measurement of exactly that — a negative appearing only at the commission, `−0.25717240`,
    /// cleared by the fund three seconds later — is where the isolated half of this rule comes
    /// from; `derived-ooim-plan.md` §3a.)
    ///
    /// # What a negative value means, and what holds it in place
    ///
    /// A RECEIVABLE: value the account owes the system, held as one signed field. The machinery is:
    ///
    /// * **Nothing is over-promised.** The deficit enters the custody identity as a NEGATIVE claim,
    ///   so the sum of what everyone can withdraw is still ≤ what the DEX custodies. Clamping the
    ///   field at 0 is what breaks the identity — by exactly the deficit. The negative sign is
    ///   load-bearing accounting, not a placeholder.
    /// * **It cannot be walked away from.** Every money-out gate (`transferFromPerp`,
    ///   `internalTransfer` from the perp wallet, `depositInsuranceFund`, `addPositionMargin`) reads
    ///   the withdrawable balance `X = max(0, min(cWB − Σ_I ooIM − Σ_C MM, A))`, whose first term
    ///   holds no unrealized profit at all: a negative cross wallet withdraws NOTHING, however much
    ///   profit stands behind it. Every risk-increasing admission reads `A = cWB + Σ_C (uPnL − PIM) −
    ///   Σ ooIM`, which the deficit lowers one for one; an ISOLATED open must moreover find its silo's
    ///   cash in the wallet itself (J6), so a deficit is never papered over with a silo.
    /// * **It is collected.** The next deposit, the next profitable close, a funding credit and an
    ///   isolated position's close credit all NET against it — it is one signed field, so nothing
    ///   lands in a fresh spendable bucket.
    /// * **It is NOT double-counted with the Insurance Fund.** Disjoint slices: the fund absorbs only
    ///   ISOLATED bad debt (what a realized loss exceeded its own silo by — `apply_position_fill`
    ///   never debits a wallet for an isolated loss), while a cross loss is never bad debt per fill
    ///   and lives here.
    ///
    /// # When it is cleared — and when it must NOT be
    ///
    /// The insurance fund clears a negative wallet ONLY when the account holds no position at all,
    /// cross or isolated (`open_position_count == 0`; Binance's rule, owner decision Q8 — cross P3).
    /// Until then it stays negative: an isolated silo still open will return its close credit to
    /// this wallet, and the cross positions still open may yet close in profit, so the protocol can
    /// still collect it.
    ///
    /// ## ⛔ Do NOT "fix" a negative value by absorbing it from the Insurance Fund any earlier
    ///
    /// It is the change this shape invites, and it is strictly worse than leaving it alone. Custody
    /// would still balance (the fund falls, the wallet rises), so the conservation gates would not
    /// object — but it FORGIVES a debt the protocol can still collect, turning a receivable into a
    /// socialised write-off, and while the account still holds an isolated silo it hands the user
    /// the fund's money to keep (cross report J3). The same applies to the fill-time silo shortfall
    /// of an isolated maker: the position may yet close in profit, and the fund already absorbs the
    /// beyond-silo slice if it does not. Assertions in `usdc_custody` fail on purpose if anyone wires
    /// up the latter. Leave them failing.
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
    /// Cross margin needed NO new term in it: a cross close moves value between `v_quote` and this
    /// field exactly as an isolated close moves it between `v_quote`, the silo and this field, and
    /// a cross funding settle moves it between counterparties' wallets (pending funding, counted
    /// at its settlement value, closes the identity between settles).
    /// `cross_settlement_tests::Ledger` re-checks it after every step of every cross money path —
    /// open, close, flip, fee, funding, ADL, a wallet driven negative.
    ///
    /// ## Observability
    ///
    /// The true signed value is readable through EVERY published surface — there is no clamped one
    /// left. Both account views (`getAccount(address)`, index-driven, and
    /// `getAccountMargin(address, uint64[])`) return `int64 totalCrossWalletBalance` and
    /// `int64 availableBalance` unclamped (for an account holding cross positions `getAccount`'s
    /// `totalCrossWalletBalance` is `cWB`, i.e. this field plus the cross funding accrued and not yet
    /// settled), AND so does the `AccountBalanceChanged` event, whose `int64 totalCrossWalletBalance`
    /// is this field verbatim. (The event carries only the three BALANCES — the account-level margin
    /// totals around them are `getAccount`-only, the way Binance keeps them on REST rather than on
    /// `ACCOUNT_UPDATE`.) An operator watching only the EVENT stream can therefore see a deficit
    /// appear. (The event used to project the wallet through a `uint64` floored at 0, via
    /// [`UserAccount::visible_perp_wallet_balance`], now test-only — the way Binance's own clamped
    /// `availableBalance` does, `misc/binance-v3-account-balance-field-reference.md`, R5: computed
    /// `−0.00085981`, reported `0.00000000`.)
    #[serde(rename = "PB")]
    pub perp_wallet_balance: i64,

    // ── Folded per-user scalars (were separate off-trie keys) ──────────────────
    // Maker/taker fee rates + the order-id nonce are per-USER (like the account) and are read
    // TOGETHER with the account on the hot placement path, so they ride in the account blob — one
    // probe/decode instead of three. Appended positionally (`#[serde(default)]` so a shorter blob
    // still decodes); read via `load_account_ref` (Arc, no usdc_balance String clone).
    /// Maker fee rate in ppm of notional, SIGNED (snapshotted onto each resting order's entry).
    #[serde(rename = "MF", default)]
    pub maker_fee_ppm: i64,
    /// Taker fee rate in ppm of notional.
    #[serde(rename = "TF", default)]
    pub taker_fee_ppm: u64,
    /// Monotonic per-user nonce used to derive order ids (`keccak(account ‖ nonce)`).
    #[serde(rename = "NO", default)]
    pub nonce: u64,

    /// `Σ pos.margin` over ALL of this user's positions — Binance's
    /// `totalWalletBalance − totalCrossWalletBalance`, i.e. the money physically sitting in the
    /// per-position silos, net of any funding DEBT a silo carries (a position's margin may be
    /// negative — see `perp_engine::funding`), so it, and `totalWalletBalance` with it, can fall
    /// below zero or below the cross wallet.
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

    // ── Sub-account family (P1) ────────────────────────────────────────────────
    // Appended positionally, like every field above them; `default` lets a shorter blob decode.
    /// Set ONLY on a sub-account: the master that created it (and the only address that may act for
    /// it, through `asSubAccount`). `None` on every other account — a master and an ordinary user
    /// look identical here, and an address with `master == None` is by definition a master
    /// candidate.
    ///
    /// `Option<Address>` rather than "zero address = none" because it is the CHEAPER encoding for
    /// the common case: `None` is one msgpack `nil` byte, while an `Address` is always a 22-byte bin
    /// (2-byte header + 20). Every non-sub account — i.e. almost all of them — pays 1 byte instead of
    /// 22; a sub pays 22 either way.
    ///
    /// Written once, at `createSubAccount`, and never again: no delete, no re-parenting.
    #[serde(rename = "MA", default, with = "crate::as_bin::opt_address_bin")]
    pub master: Option<Address>,
    /// Set ONLY on a master: how many sub-accounts it has created, ≤ [`MAX_SUB_ACCOUNTS`]. Its subs
    /// are `derive_sub_account(self, i)` for `i < sub_count` — the list is derived, not stored.
    /// Always 0 on a sub (a sub cannot create subs).
    #[serde(rename = "SC", default)]
    pub sub_count: u8,

    // ── Cross margin (P2) ───────────────────────────────────────────────────────
    // Appended positionally, like every field above them; `default` lets a shorter blob decode.
    /// Account-level liquidation flags — bit 0 is [`LIQ_FLAG_IN_LIQUIDATION`], Binance's
    /// "User in liquidation mode now" (`-2023`). Every other bit is reserved and zero.
    ///
    /// No path sets it yet (cross P2), so every account reads `0`. Cross liquidation (P3) owns it —
    /// set when a cross liquidation episode starts, cleared when the account is healthy again. The
    /// read side is live since P2c and keys off [`Self::in_liquidation`]: `setMarginType`'s
    /// `-2023`, every placement entrypoint's `-2023` for an order that is not reduce-only, and a
    /// resting order of a flagged maker cancelled instead of filled.
    ///
    /// Like [`Self::total_position_margin`] it is NOT a field a whole-blob `storage::save_account`
    /// may write: that function carries the STORED value over whatever its caller's copy holds, so
    /// a path that loaded the account before the flag moved cannot clear (or set) it by writing
    /// the copy back. Its writer goes through `storage::mutate_account`.
    #[serde(rename = "LF", default)]
    pub liq_flags: u8,
    /// How many of this user's markets hold a NON-ZERO position, cross AND isolated — the
    /// "no open positions (cross or isolated)" condition Binance's negative-balance clearance
    /// (`INSURANCE_CLEAR`) requires (cross P3; owner decision Q8).
    ///
    /// # Owned by `storage::save_position`, and by nothing else
    ///
    /// Maintained at the one place every `amount` write passes through, from the SAME zero-crossing
    /// the open-position registry and the per-user market index already detect: `+1` when a
    /// position opens from flat, `−1` when it closes to flat (a flip crosses no zero and moves
    /// nothing). That read is already paid for, so the count costs no load — only an account write
    /// on an open or a close, which those paths make anyway. `storage::save_account` carries the
    /// stored value over its caller's copy, exactly as it does for `total_position_margin`.
    ///
    /// A `u8`: it is bounded by [`MAX_USER_MARKETS`] (16), because a non-zero position keeps its
    /// market in the per-user index. `margin_view::index_account_scalars` cross-checks it against
    /// a fresh walk of the index in `debug_assertions` builds.
    #[serde(rename = "OP", default)]
    pub open_position_count: u8,
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
            maker_fee_ppm: 0,
            taker_fee_ppm: 0,
            nonce: 0,
            total_position_margin: 0,
            master: None,
            sub_count: 0,
            liq_flags: 0,
            open_position_count: 0,
        }
    }
}

/// [`UserAccount::liq_flags`] bit 0: a cross liquidation episode is in progress for this account —
/// Binance's `-2023 User in liquidation mode now.` Set and cleared by cross liquidation (P3).
pub const LIQ_FLAG_IN_LIQUIDATION: u8 = 1;

impl UserAccount {
    /// Whether [`LIQ_FLAG_IN_LIQUIDATION`] is set — the ONE reader every `-2023` check goes
    /// through. Total (a bit test).
    #[inline]
    pub fn in_liquidation(&self) -> bool {
        self.liq_flags & LIQ_FLAG_IN_LIQUIDATION != 0
    }

    /// The root of this account's family: its master if it is a sub, itself otherwise. Two accounts
    /// are in the same family iff their roots are equal (master ↔ own sub, sub ↔ sibling sub, and an
    /// account with itself).
    #[inline]
    pub fn family_root(&self, own_address: Address) -> Address {
        self.master.unwrap_or(own_address)
    }

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
    /// size of every deficit — and since cross margin (P2) a negative cross wallet is a normal
    /// state, not a corner
    /// (`trading::tests::a_maker_close_fee_is_the_only_remaining_way_to_a_negative_wallet`
    /// produces the one ISOLATED state where it bites; `risk::tests::usdc_custody` asserts the two
    /// views AGREE on the isolated maker-fill path, which is what the M1 switch bought). Gates use
    /// `margin_view::derived_available_balance` (orders) and
    /// `margin_view::derived_withdrawable_balance` (money out), both signed at the source. What
    /// this is still FOR is pinning that contrast in tests — asserting that a clamped reading would
    /// have reported 0 where the honest surfaces report the deficit.
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

    /// Debits perp wallet balance, allowing the internal value to go negative — a normal state for
    /// a cross account (see [`Self::perp_wallet_balance`]). No affordability rule lives here: the
    /// caller's gate (`A` for orders, `X` for money out) decides whether the debit may happen.
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

/// **Why this account update happened** — byte 4 of `AccountBalanceChanged`, and Binance's
/// `ACCOUNT_UPDATE.a.m`.
///
/// Without it the header says only *that* an account moved. A consumer had to infer the cause from
/// what SURROUNDS the group — "was there a `Trade` just before it, a `FundingSettled`, nothing at
/// all" — which is exactly the kind of positional inference that breaks the first time a stream
/// position changes. This field is the attribution, and it is a REQUIRED parameter at every publish
/// site: there is deliberately no `Default`, no `UNKNOWN`, and no fallback, so a new emit site
/// cannot compile without choosing.
///
/// Numbering follows [`crate::types::CancelReason`]/[`crate::types::OrderStatus`] style — a stable
/// `u8` wire code, never a string. The codes 0..=7 are the Binance vocabulary values we can
/// actually produce; 8 ([`Self::InternalTransfer`]) is ours — appended after them for an operation
/// Binance's vocabulary has no value for; 9 ([`Self::MarginTypeChange`]) is Binance's again, and
/// the one code no row carries (see below); `255` is [`Self::Multiple`], also ours and the one
/// value that is not a single cause (see below), parked at the top of the range so the low block
/// stays contiguous and extensible.
///
/// # The mapping to Binance's `a.m` vocabulary
///
/// | this | Binance | what produces it here |
/// |---|---|---|
/// | [`Self::Order`] | `ORDER` | a matched order moved money: the taker's settlement, each maker fill, the trading fee credited to the fee recipient, and the book leg of a liquidation close (which really is an order — it goes through `match_order`). |
/// | [`Self::FundingFee`] | `FUNDING_FEE` | a funding settlement moved `pos.margin`. |
/// | [`Self::Deposit`] | `DEPOSIT` | value entered the venue: ERC-20 USDC pulled into custody. |
/// | [`Self::Withdraw`] | `WITHDRAW` | value left the venue: USDC returned to the caller. |
/// | [`Self::MarginTransfer`] | `MARGIN_TRANSFER` | an ISOLATED POSITION's margin moved against the wallet — `addPositionMargin` / `removePositionMargin`. This is what the Binance value means (their *Modify Isolated Position Margin*), and we have exactly that operation, so it is reserved for it. |
/// | [`Self::AssetTransfer`] | `ASSET_TRANSFER` | an internal wallet-to-wallet move of the same asset that touches no position: `transferToPerp` / `transferFromPerp` (spot USDC ledger ↔ perp wallet). |
/// | [`Self::Adjustment`] | `ADJUSTMENT` | the protocol moved a position or a balance with NO order of the user's and no transfer: the liquidation residual closed at mark, both ADL legs, and the post-close dust write-off. |
/// | [`Self::InsuranceClear`] | `INSURANCE_CLEAR` | the counterparty of the wallet move is the INSURANCE FUND: the liquidation clearance fee, and the admin's `depositInsuranceFund` / `withdrawInsuranceFund`. |
/// | [`Self::InternalTransfer`] | — (none; ours) | `internalTransfer`: value moved between TWO accounts (family or external) — both legs, sender and receiver, carry it. |
/// | [`Self::MarginTypeChange`] | `MARGIN_TYPE_CHANGE` | **nothing — no `AccountBalanceChanged` row carries it.** See below. |
///
/// ## `MARGIN_TYPE_CHANGE`: defined, and produced by no row
///
/// It was deliberately absent while the venue was isolated-only, as a value no path could produce.
/// Cross margin (P1) added the switch — `setMarginType` — but the switch STILL publishes no
/// `AccountBalanceChanged`: it is refused unless the position is flat and the market holds none of
/// the owner's orders, so none of the three balances the row carries can move, and a row with no
/// change in it has no truthful reason (the argument `setLeverage`'s silence rests on — see
/// `perp_engine::storage::save_position_margin_type_only`). The switch is published as its own
/// `MarginTypeChanged` log instead. The code is defined anyway, at Binance's name, because that
/// log IS Binance's `ACCOUNT_UPDATE m=MARGIN_TYPE_CHANGE`: an indexer synthesising Binance's push
/// from it needs the canonical byte, and it must be the same byte this enum would use if a future
/// path ever did publish a row for it. Switch on the field with a default branch regardless.
///
/// ## `transferToPerp` / `transferFromPerp`: `ASSET_TRANSFER`, not `MARGIN_TRANSFER`
///
/// Both Binance values describe a transfer, so the question is *which two pots*. Binance's
/// `MARGIN_TRANSFER` is the isolated-position leg — money moving between the futures wallet and one
/// position's isolated margin. Our `addPositionMargin` / `removePositionMargin` IS that operation,
/// field for field, so giving `transferToPerp` the same code would make the one value that has an
/// exact analogue here ambiguous. `transferToPerp` moves between two WALLETS (the spot USDC ledger
/// and the perp wallet) and touches no position, which is the `ASSET_TRANSFER` shape. `DEPOSIT` /
/// `WITHDRAW` are then free to mean what they say — value crossing the venue boundary — which is
/// the only distinction a consumer of `usdcBalance` actually needs.
///
/// ## `internalTransfer`: its own code, NOT `ASSET_TRANSFER`
///
/// `ASSET_TRANSFER` keeps its meaning — one account, two of ITS wallets. `internalTransfer` moves
/// value between two DIFFERENT accounts (a master and its sub, two siblings, or two unrelated
/// users), so the receiver's balance rises with no action of its own. A consumer reconciling a
/// single account's ledger must be able to tell "I moved my own money" from "someone sent me money"
/// without inspecting the surrounding logs, which is the whole purpose of this field. The rare
/// `from == to` shape (one account, two wallets, through `internalTransfer` rather than
/// `transferToPerp`/`transferFromPerp`) carries it too — the code names the SELECTOR's operation,
/// one label per entry point, never a value-diff guess.
///
/// # ⚠️ [`Self::Multiple`] is not a default, and not "unknown"
///
/// The end-of-call drain publishes ONE row per user for the whole call, so two DIFFERENT causes can
/// legitimately fold into a single row (see `perp_engine::storage::mark_account_snapshot_dirty`).
/// Labelling such a row with either cause would be a lie about the other, and a consumer will trust
/// this field — so a genuine disagreement collapses to `Multiple` via [`Self::merge`], which reads
/// *"more than one distinct cause moved this account in this call; the payload is the settled
/// after-image of all of them"*. It is never chosen by a publish site, only reached by merging two
/// different ones, and merging is COMMUTATIVE and IDEMPOTENT so the result does not depend on write
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum AccountUpdateReason {
    /// Binance `ORDER`: a matched order moved money.
    Order = 0,
    /// Binance `FUNDING_FEE`: a funding settlement.
    FundingFee = 1,
    /// Binance `DEPOSIT`: value entered the venue.
    Deposit = 2,
    /// Binance `WITHDRAW`: value left the venue.
    Withdraw = 3,
    /// Binance `MARGIN_TRANSFER`: an isolated position's margin moved against the wallet.
    MarginTransfer = 4,
    /// Binance `ASSET_TRANSFER`: an internal wallet-to-wallet move touching no position.
    AssetTransfer = 5,
    /// Binance `ADJUSTMENT`: a protocol-forced position/balance move with no order and no transfer.
    Adjustment = 6,
    /// Binance `INSURANCE_CLEAR`: the move settles against the insurance fund.
    InsuranceClear = 7,
    /// `internalTransfer`: value moved between two accounts (sub-account family or external). Both
    /// the sender's and the receiver's after-image carry it. Deliberately NOT [`Self::AssetTransfer`]
    /// — see the type docs.
    InternalTransfer = 8,
    /// Binance `MARGIN_TYPE_CHANGE`: the margin type of a position changed. **Carried by no
    /// `AccountBalanceChanged` row** — the switch moves no balance and publishes only its
    /// `MarginTypeChanged` log; the code exists for that log's Binance mapping. See the type docs.
    MarginTypeChange = 9,
    /// **OURS, not Binance's.** Two or more distinct causes coalesced into one drained row. See the
    /// type docs — never chosen by a publish site, only produced by [`Self::merge`].
    Multiple = 255,
}

impl AccountUpdateReason {
    /// The wire code, for the `uint8 reason` topic.
    #[inline]
    pub fn code(self) -> u8 {
        self as u8
    }

    /// The inverse of [`Self::code`] — a decoder's view of the topic. `None` for any code this
    /// build does not define, deliberately: there is no catch-all variant to fall back to, and
    /// silently mapping an unknown byte onto an existing reason is the one failure this field exists
    /// to prevent.
    #[inline]
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Order,
            1 => Self::FundingFee,
            2 => Self::Deposit,
            3 => Self::Withdraw,
            4 => Self::MarginTransfer,
            5 => Self::AssetTransfer,
            6 => Self::Adjustment,
            7 => Self::InsuranceClear,
            8 => Self::InternalTransfer,
            9 => Self::MarginTypeChange,
            255 => Self::Multiple,
            _ => return None,
        })
    }

    /// Joins two reasons recorded for the SAME user in the same call: the same reason twice is
    /// itself, two different ones are [`Self::Multiple`].
    ///
    /// Commutative, associative and idempotent, which is the property that matters here — the drain
    /// must publish the same label regardless of the order the writes happened to mark in, and
    /// "last wins" / "first wins" would both make the published label a function of write ordering
    /// that a later refactor could silently change.
    #[inline]
    pub fn merge(self, other: Self) -> Self {
        if self == other {
            self
        } else {
            Self::Multiple
        }
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
    /// The wire codes are a CONTRACT (they are in an indexed log topic), so they are pinned
    /// literally rather than derived from the declaration order they happen to have.
    #[test]
    fn the_account_update_reason_wire_codes_round_trip() {
        use AccountUpdateReason::*;
        for (r, code) in [
            (Order, 0u8),
            (FundingFee, 1),
            (Deposit, 2),
            (Withdraw, 3),
            (MarginTransfer, 4),
            (AssetTransfer, 5),
            (Adjustment, 6),
            (InsuranceClear, 7),
            (InternalTransfer, 8),
            (MarginTypeChange, 9),
            (Multiple, 255),
        ] {
            assert_eq!(r.code(), code, "{r:?}");
            assert_eq!(AccountUpdateReason::from_code(code), Some(r));
        }
        // No catch-all: an undefined byte decodes to `None`, never onto a neighbouring reason.
        for code in 10..=254 {
            assert_eq!(AccountUpdateReason::from_code(code), None, "code {code}");
        }
    }

    /// The drain's conflict rule. The properties that matter are that it is IDEMPOTENT (the common
    /// case — a user marked twice by the same cause — keeps its truthful label) and COMMUTATIVE (the
    /// published label cannot depend on the order two writes happened to mark in, which is what
    /// first-wins/last-wins could not offer).
    #[test]
    fn merging_two_reasons_is_a_commutative_join_that_only_collapses_on_disagreement() {
        use AccountUpdateReason::*;
        let all = [
            Order,
            FundingFee,
            Deposit,
            Withdraw,
            MarginTransfer,
            AssetTransfer,
            Adjustment,
            InsuranceClear,
            InternalTransfer,
            MarginTypeChange,
            Multiple,
        ];
        for a in all {
            assert_eq!(a.merge(a), a, "idempotent: {a:?}");
            for b in all {
                assert_eq!(a.merge(b), b.merge(a), "commutative: {a:?} / {b:?}");
                if a != b {
                    assert_eq!(a.merge(b), Multiple, "{a:?} vs {b:?} must not pick a side");
                }
                // Associative, over the triples that a call can actually accumulate.
                for c in all {
                    assert_eq!(a.merge(b).merge(c), a.merge(b.merge(c)));
                }
            }
        }
        // The real conflict shapes, spelled out: the liquidated user's dust write-off meeting her
        // clearance fee, and the fee recipient who is also a maker (whose two marks AGREE, so the
        // merge is a no-op and that row keeps a truthful `Order`).
        assert_eq!(Adjustment.merge(InsuranceClear), Multiple);
        assert_eq!(FundingFee.merge(InsuranceClear), Multiple);
        assert_eq!(Order.merge(Order), Order);
    }

    /// The derivation against vectors computed OUTSIDE this codebase (ethers v6:
    /// `getAddress(dataSlice(keccak256(concat([toUtf8Bytes("perpdex_subaccount_v1"), master,
    /// Uint8Array([index])])), 12))`), so a client reproducing the documented formula gets these
    /// exact addresses. A self-referential test (derive == derive) would pin nothing.
    #[test]
    fn the_sub_account_derivation_matches_an_independent_vector() {
        use primitives::address;
        let m1 = address!("1111111111111111111111111111111111111111");
        let m2 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        for (master, index, expected) in [
            (m1, 0u8, address!("24eC69c992D5c4DB42c24451181Bb2956af63E3f")),
            (m1, 1, address!("123832A262863ceCa86046F21D24853587D01FC9")),
            (m1, 9, address!("67CA0A43deDAb1Af20f8c7B45EaC8a4e468593Ca")),
            (m2, 0, address!("68Ec85D317b90572c24467d735786f25476c8Cd1")),
            (m2, 1, address!("f502C903E921900b354f38326E34b2D676AB82Cf")),
            (m2, 9, address!("e5BC8A2e872Cba851C6877Fb8470a23230754a91")),
        ] {
            assert_eq!(derive_sub_account(master, index), expected, "{master} #{index}");
        }
        assert_eq!(SUB_ACCOUNT_DOMAIN.len(), 21, "the documented preimage is 21 + 20 + 1 bytes");
    }

    /// Deterministic, and distinct across every `(master, index)` a family can reach — including
    /// across two masters, and never equal to either master.
    #[test]
    fn derived_sub_accounts_are_deterministic_and_distinct() {
        use primitives::address;
        let masters = [
            address!("1111111111111111111111111111111111111111"),
            address!("2222222222222222222222222222222222222222"),
        ];
        let mut seen = std::collections::BTreeSet::new();
        for m in masters {
            seen.insert(m);
        }
        for m in masters {
            for i in 0..MAX_SUB_ACCOUNTS {
                let a = derive_sub_account(m, i);
                assert_eq!(a, derive_sub_account(m, i), "deterministic");
                assert!(seen.insert(a), "collision at {m} #{i}");
            }
        }
        assert_eq!(seen.len(), 2 + 2 * MAX_SUB_ACCOUNTS as usize);
    }

    /// The two family fields are APPENDED (positional msgpack) and cost an ordinary account exactly
    /// two bytes — `nil` for `master`, fixint `0` for `sub_count` — while a sub's `master` is a
    /// 22-byte bin. Also pins that a pre-P1 (shorter) blob still decodes, with both fields defaulted.
    #[test]
    fn the_family_fields_are_appended_and_cheap_for_ordinary_accounts() {
        use crate::codec::{decode, encode};
        let plain = UserAccount::default();
        let plain_bytes = encode(&plain).unwrap();
        let sub = UserAccount {
            master: Some(Address::repeat_byte(0x11)),
            ..UserAccount::default()
        };
        let sub_bytes = encode(&sub).unwrap();
        assert_eq!(sub_bytes.len(), plain_bytes.len() + 21, "Some(addr) = 22 bytes vs nil = 1");
        // The family pair, then the two cross-P2 fields behind it (see the next test).
        let family = &plain_bytes[plain_bytes.len() - 4..plain_bytes.len() - 2];
        assert_eq!(family, &[0xc0, 0x00], "nil, then fixint 0");
        assert_eq!(decode::<UserAccount>(&sub_bytes).unwrap(), sub);

        // A blob written before the family fields existed: the same array with its last FOUR
        // elements dropped (the family pair and the two later cross fields; msgpack fixarray header
        // 0x9a = 10 elements → 0x96 = 6).
        assert_eq!(plain_bytes[0], 0x9a, "10-element fixarray");
        let mut old = plain_bytes[..plain_bytes.len() - 4].to_vec();
        old[0] = 0x96;
        assert_eq!(decode::<UserAccount>(&old).unwrap(), plain);
    }

    /// The two cross-P2 fields (`liq_flags`, `open_position_count`) are APPENDED after the family
    /// pair and cost an ordinary account two bytes (two fixint 0s); a blob written before them
    /// decodes with both defaulted, and non-default values survive the codec.
    #[test]
    fn the_cross_account_fields_are_appended_and_default_to_zero() {
        use crate::codec::{decode, encode};
        let plain = UserAccount::default();
        let plain_bytes = encode(&plain).unwrap();
        assert_eq!(&plain_bytes[plain_bytes.len() - 2..], &[0x00, 0x00], "two fixint 0s, LAST");
        let mut old = plain_bytes[..plain_bytes.len() - 2].to_vec();
        old[0] -= 2;
        assert_eq!(decode::<UserAccount>(&old).unwrap(), plain, "pre-P2 blob decodes, defaulted");

        let flagged = UserAccount {
            liq_flags: LIQ_FLAG_IN_LIQUIDATION,
            open_position_count: 16,
            ..UserAccount::default()
        };
        let bytes = encode(&flagged).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[0x01, 0x10]);
        assert_eq!(decode::<UserAccount>(&bytes).unwrap(), flagged);
        assert!(flagged.in_liquidation());
        assert!(!plain.in_liquidation());
        // Only bit 0 means anything; a reserved bit alone is not "in liquidation".
        assert!(!UserAccount { liq_flags: 0b10, ..UserAccount::default() }.in_liquidation());
    }

    #[test]
    fn the_family_root_is_the_master_for_a_sub_and_self_otherwise() {
        let me = Address::repeat_byte(0x22);
        let boss = Address::repeat_byte(0x11);
        assert_eq!(UserAccount::default().family_root(me), me);
        let sub = UserAccount {
            master: Some(boss),
            ..UserAccount::default()
        };
        assert_eq!(sub.family_root(me), boss);
    }

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
