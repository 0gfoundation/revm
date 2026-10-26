//! Position and market types for the PerpDEX precompile.
use core::fmt;

use serde::{
    de::{self, SeqAccess, Visitor},
    ser::SerializeSeq,
    Deserialize, Deserializer, Serialize, Serializer,
};

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
    /// Margin (collateral) allocated to this position — Binance's `isolatedWallet`.
    ///
    /// **`margin < ROUND_UP(|N| / L)` (below the position's own initial margin) is NORMAL, not an
    /// anomaly, and nothing may assert otherwise.** Binance checks only the maintenance margin
    /// continuously; after placement IM is checked again only when margin is withdrawn (the THIRD
    /// case below), never continuously (`misc/binance-margin-verified-model.md` §1.2, and
    /// `misc/binance-flip-and-admission.md` §3.9: 「逐仓仓位**天生**就低于自己的 `IM`」 — run1's
    /// market open computed `PIM = 6.34041` while the silo received `6.30870795`, short by one
    /// commission, and 「照常存活」). Ours is below by the
    /// same mechanism: the opening fill funds the trading fee out of the margin it creates
    /// (`isolatedWallet = Ne/L − f·Ne`, see the `fee_reserved` note below). Funding charges, close
    /// legs and ADL drain it further with no IM floor.
    ///
    /// A SECOND, larger mechanism was added with model M1: a maker fill whose wallet cannot cover the
    /// opening margin funds this field with `min(requirement, cash at hand)` and leaves it short by
    /// the remainder, because that is what R11 measured Binance doing — silo
    /// `63.10632800 == W0 + realized` against an IM-implied `64.16451380`
    /// (`derived-ooim-plan.md` §3a, `trading::settlement::OpeningMarginFunding`). Unlike a fee-sized
    /// gap this one is unbounded, so treating "short of IM" as an invariant violation is not merely
    /// pedantic — it would fire in production.
    ///
    /// A THIRD: `removePositionMargin` (roadmap 2b) lets a position in profit withdraw down to a
    /// balance below `PIM`, the difference carried by unrealized profit — Binance's cap
    /// `max(0, min(iw − MM, iw + uPnL − PIM))`, whose equity term R21 measured (balance 8.0952 left
    /// against PIM 8.3676; the `iw − MM` term is documented, never measured). Ours adds a
    /// liquidation guard (equity ≥ `MM`) on top. That is a check AT WITHDRAWAL TIME, not a
    /// continuous one: it holds equity ≥ `PIM` and this field ≥ `MM` at the instant of the
    /// withdrawal, and nothing re-checks either afterwards.
    ///
    /// A FOURTH, and the only one that goes below zero: a funding charge lands on this field WHOLE,
    /// so it can turn NEGATIVE — a debt the position owes (`perp_engine::funding`). The debt lowers
    /// equity, so the maintenance check below sees it; a close releases this field signed and nets
    /// the closed slice's share of the debt against its PnL; only a shortfall at close reaches the
    /// insurance fund. A FLAT position always holds exactly 0 (a full close releases all of it),
    /// which `storage::save_position` debug-asserts.
    ///
    /// So does a position CROSSED in force ([`MarginType`], cross margin P2), at every size: its
    /// collateral is the shared cross wallet, its initial margin is derived rather than moved, and
    /// its funding and PnL settle against the wallet — there is no silo. `storage::save_position`
    /// debug-asserts that too, and every account fold ignores this field for a cross position.
    ///
    /// The only continuous requirement gate in the engine is
    /// [`crate::math::is_above_maintenance_margin`], and it reads THIS field, so a short silo
    /// correctly prices its own liquidation. Pinned by
    /// `a_position_naturally_below_its_own_initial_margin_survives_normally` and
    /// `trading::tests::a_short_silo_lowers_the_maintenance_buffer_it_is_measured_against`.
    #[serde(rename = "m")]
    pub margin: i64,
    // NOTE: the former open-order margin ESCROW is GONE — the six fields "mr"
    // (`margin_reserved`), "mrn", "br", "brn", "sr", "srn" were deleted with it. Placement used to
    // compute the flip-aware worst-case reservation `max(S + B', B + S')`, store it here, and
    // physically `debit_perp` the wallet by its delta (credited back at cancel/fill). Binance has
    // no such bucket: the open-order requirement is DERIVED on demand from `(N, Bid, Ask, L)` as
    // `ooIM = ROUND_UP(max(|N + Bid|, |N − Ask|) / L) − ROUND_UP(|N| / L)`
    // ([`crate::math::open_order_margin`]) and merely SUBTRACTED from the wallet at the admission
    // gate, never debited. `Bid`/`Ask` are exactly `total_buy_notional`/`total_sell_notional`
    // below, so everything the escrow used to store is reconstructible from what remains.
    // NOTE: the former "fr" (`fee_reserved`) escrow is GONE. Placement used to withhold the
    // order's prospective maker fee from the wallet on top of the margin reservation and release
    // it at fill/cancel. Binance has no such bucket: the trading fee is charged out of the margin
    // the fill itself funds (`isolatedWallet = Ne/L − f·Ne`), so `availableBalance` is not reduced
    // by a fee that may never be paid. Every fill site now applies
    // `fee_from_margin = min(fee, opening_margin)` / `fee_from_wallet = fee − fee_from_margin`.
    /// Current leverage setting. Bounded by the market's tier-0 `max_leverage`
    /// (see [`MarginTiers`]) at `setLeverage` time; 1 when unset.
    #[serde(rename = "lv")]
    pub leverage: u64,
    /// Cumulative funding index at this position's last funding settlement.
    /// Funding owed = `amount × (market cumulative_funding_index − this)`,
    /// settled lazily on every position-touching op. See the `funding` module.
    #[serde(default, rename = "fi")]
    pub last_funding_index: i128,
    // ── Per-side resting-order aggregates = Binance's `Bid` / `Ask` (catalog #A) ─────────────
    // Maintained mirrors of the resting-order lists. `*_notional` is the SUM OF PER-ORDER
    // `OrderEntry::margin_notional()` — `calc_value(assuming_price, amount)` at each order's FROZEN
    // Assuming Price (each floored exactly as a fold over the list produces it) → maintainable
    // ± one term with zero floor-composition error. Kept in sync at every order-list mutation
    // (place/cancel incrementally; fills/liquidation by recompute-from-list). Derivable from the
    // lists via `math::sum_side_totals`, so a genesis/default 0 is correct only for an empty book.
    //
    // **These two ARE Binance's `bidNotional` / `askNotional`, outright.** ONE rule covers both
    // sides — `Σ calc_value(entry.assuming_price, entry.amount)` — because the per-order Assuming
    // Price already carries the side asymmetry: a BUY's is its own limit price (no markup), a SELL's
    // is `max(max(⌈last × 1.0015⌉, mark), limit)` resolved AT PLACEMENT. There is no second basis
    // and no read-time re-pricing pass; a resting order's contribution never moves again except by
    // being partially filled (which shrinks `amount`, and the term with it). Measured: R12,
    // `misc/binance-flip-and-admission.md` §3.13 — see `OrderEntry::assuming_price`.
    //
    // Both are proven equal to the resting-order fold after every transition by
    // `side_aggregates_are_exactly_bid_and_ask_after_every_operation`.
    //
    // ⚠️ FROZEN aggregates do NOT make `ooIM` frozen. It also contains `N = |amount| × mark`, which
    // is recomputed at every read, so the requirement still moves with the mark whenever the
    // position is non-flat (R10). Only at `N = 0` does `ooIM` degenerate to a pure constant.
    //
    // Together with `amount`, `leverage` and the mark, these are the inputs to the DERIVED
    // open-order requirement that replaced the escrow — see the note where the escrow fields used
    // to be.
    /// Σ resting BUY order amounts (base units).
    #[serde(default, rename = "tbq")]
    pub total_buy_qty: u64,
    /// `Bid` — Σ `calc_value(assuming_price, amount)` over resting BUY orders (quote units).
    #[serde(default, rename = "tbn")]
    pub total_buy_notional: u64,
    /// Σ resting SELL order amounts (base units).
    #[serde(default, rename = "tsq")]
    pub total_sell_qty: u64,
    /// `Ask` — Σ `calc_value(assuming_price, amount)` over resting SELL orders (quote units).
    #[serde(default, rename = "tsn")]
    pub total_sell_notional: u64,
    /// `cr` — **LIFETIME** cumulative realised PnL for this `(user, market)`: the running sum of
    /// every closing leg's `realized_pnl`, i.e. Binance's `ACCOUNT_UPDATE.a.P[].cr`.
    ///
    /// # It is a LEVEL, and it is the one field that OUTLIVES the position
    ///
    /// Every other field here describes the position that is open right now. This one describes
    /// its whole history, so **it is deliberately NOT zeroed when the position closes** and must
    /// survive the position going flat and being reopened. That is the entire reason it has to be
    /// stored: `realized_pnl` on `PositionChanged` is a per-event DELTA, and nothing else in
    /// `(amount, v_quote_balance, margin)` retains a closed round's PnL — a consumer that missed
    /// one event could never recover the total from state.
    ///
    /// ⚠️ **Do NOT add it to `storage::save_position`'s `pos.amount != 0 || pos.margin == 0`
    /// `debug_assert`.** A flat position carrying a non-zero `cr` is the FEATURE, not a leak; that
    /// assertion is about MONEY (`Σ pos.margin` is a published balance) and this field is not
    /// money — it is a statistic and no balance identity reads it. Pinned by
    /// `trading::tests::position_changed_derived_fields::cumulative_realized_pnl_survives_a_close_and_a_reopen`.
    ///
    /// # What is and is not in it
    ///
    /// The sum of exactly the quantity `PositionChanged.realizedPnl` reports — gross close PnL,
    /// **excluding** released margin, trading fees and funding, matching what R15 measured on
    /// mainnet (`cr` was the rolling sum of `rp` over 10 fills with no residual). Every accumulation
    /// site is therefore a place a closing leg's PnL is computed, and there is exactly ONE:
    /// `trading::settlement::apply_position_fill` (taker, maker, and both ADL legs). An opening leg
    /// contributes 0, so a flip only accrues its closing half. (A second site existed while a
    /// solvent residual could be closed at mark with no counterparty; every residual is an ADL fill
    /// now, so it is gone.)
    ///
    /// Accumulated with `checked_add`, never saturating: a silently clamped lifetime total is a
    /// number that reads as data and is not, and reverting is the honest failure. `i64` is ample —
    /// see the overflow note in `apply_position_fill`.
    #[serde(default, rename = "cr")]
    pub cumulative_realized_pnl: i64,
    /// `g` — which LIFE of this `(user, market)` position is open: bumped by one each time a
    /// non-zero position ENDS, i.e. its `amount` goes to 0 or changes sign. APPENDED LAST —
    /// positional msgpack.
    ///
    /// It exists for position-BOUND conditional orders (`types::Order::is_position_bound` — a
    /// `closePosition` stop / take-profit): one records this value at placement
    /// (`Order::position_generation`) and, when its trigger fires, is spent with
    /// `CancelReason::PositionClosed` if the two differ. That is Binance's `GTE_GTC` — the order
    /// expires with its position — done LAZILY: closing a position touches no order, the check
    /// costs nothing until the order would fire, and it still refuses the dangerous case of an old
    /// stop closing a NEW position opened the same way after the bound one was closed.
    ///
    /// # Owned by `storage::save_position`, and by nothing else
    ///
    /// That function is the single door for every `amount` write (the two narrower routes beside
    /// it `debug_assert` `amount` unchanged), so the zero-crossing it already detects for the
    /// position registry is exactly where a life ends. It sets this field from the STORED value it
    /// already reads, ignoring whatever the caller's struct carries, so no call site has to
    /// maintain it and a stale copy cannot roll it back. Like `cumulative_realized_pnl`, it
    /// deliberately SURVIVES the position going flat — a flat position record is never deleted.
    ///
    /// `u64`, so it cannot wrap in practice: every increment is a close or flip that a
    /// transaction paid for.
    #[serde(default, rename = "g")]
    pub generation: u64,
    /// `mt` — the margin type the owner chose for this `(user, market)`: [`MarginType`]'s wire
    /// code, `0` = CROSSED, `1` = ISOLATED. Binance's `marginType`, set per symbol by
    /// `POST /fapi/v1/marginType`. APPENDED LAST — positional msgpack.
    ///
    /// # It is the STORED choice, not the type in force
    ///
    /// Read it through `perp_engine::margin_type::effective_margin_type`, never directly: while
    /// [`CROSS_MARGIN_ENABLED`] is `false` every position is ISOLATED whatever this byte says, and
    /// that function is the one place the rule lives. A default (zero) byte is CROSSED because
    /// that is Binance's default ("All contracts and positions are defaulted to the Cross Margin
    /// mode") — so the day the gate flips, every position that never chose becomes CROSSED.
    ///
    /// # Kept across flat, like `leverage`
    ///
    /// A setting, not position state: it survives the position going flat and being reopened, and
    /// no fill, funding settlement or liquidation writes it. Its ONE writer is `setMarginType`,
    /// through `storage::save_position_margin_type_only`, and only while the position is flat and
    /// the market holds none of the owner's orders — so a non-zero position never changes type
    /// under itself. Every other route stores it back verbatim from the blob it loaded.
    #[serde(default, rename = "mt")]
    pub margin_type: u8,
}

impl PerpPosition {
    /// Zero both sides' resting-order aggregates — the state after every order in this market has
    /// left the book (liquidation's cancel-all). `Bid = Ask = 0` ⇒ the derived open-order
    /// requirement for this market is 0.
    #[inline]
    pub fn clear_side_aggregates(&mut self) {
        self.total_buy_qty = 0;
        self.total_buy_notional = 0;
        self.total_sell_qty = 0;
        self.total_sell_notional = 0;
    }
}

impl Default for PerpPosition {
    fn default() -> Self {
        Self {
            amount: 0,
            v_quote_balance: 0,
            margin: 0,
            leverage: 1,
            last_funding_index: 0,
            total_buy_qty: 0,
            total_buy_notional: 0,
            total_sell_qty: 0,
            total_sell_notional: 0,
            // A position that never existed has realised nothing. The only default that is NOT
            // trivially right is `leverage`, which is 1 rather than 0.
            cumulative_realized_pnl: 0,
            generation: 0,
            // Binance's default. Not in force while `CROSS_MARGIN_ENABLED` is off.
            margin_type: MarginType::Crossed as u8,
        }
    }
}

// ── Margin type ───────────────────────────────────────────────────────────

/// **Is CROSS margin mode live?** `false` through the cross-margin series' P1–P2 and most of P3;
/// flipped to `true` as the LAST step of P3, once cross accounting and cross liquidation exist.
///
/// While `false`:
/// * every position is ISOLATED in force, whatever its stored [`PerpPosition::margin_type`] says
///   ([`MarginType::effective`]), so every margin rule, view and event behaves exactly as it did
///   before the field existed;
/// * `setMarginType(CROSSED)` is refused, and `setMarginType(ISOLATED)` is `-4046` (it already is).
///
/// A plain `const`, deliberately: consensus code must not read a runtime switch. The engine's
/// tests enable it per thread through a `#[cfg(test)]`-only override in
/// `perp_engine::margin_type`, which compiles out of every non-test build.
pub const CROSS_MARGIN_ENABLED: bool = false;

/// A position's margin mode — Binance's `marginType`. One-way mode only: one type per
/// `(user, market)`, chosen per market (Binance: "Switching the margin mode will only apply to the
/// selected contract").
///
/// The `u8` is a wire code (stored on [`PerpPosition::margin_type`], carried by
/// `MarginTypeChanged`, `PositionChanged`, `AccountPosition` and `getSymbolConfig`), pinned
/// literally by a test. `0 = CROSSED` so a zero-initialised position has Binance's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MarginType {
    /// Binance `CROSSED`: the position draws on the shared cross wallet.
    Crossed = 0,
    /// Binance `ISOLATED`: the position carries its own margin silo (`PerpPosition::margin`).
    Isolated = 1,
}

impl MarginType {
    /// The wire code.
    #[inline]
    pub fn code(self) -> u8 {
        self as u8
    }

    /// The inverse of [`Self::code`]; `None` for any other byte — no catch-all, so an ABI caller
    /// sending `2` is refused rather than mapped onto a neighbour.
    #[inline]
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Crossed),
            1 => Some(Self::Isolated),
            _ => None,
        }
    }

    /// **The type IN FORCE** for a stored code, given whether cross margin is enabled — the one
    /// implementation of the gate. ISOLATED whenever `cross_enabled` is false.
    ///
    /// TOTAL, on purpose: it sits under views, events and (from P2) the liquidation sweep, whose
    /// error paths must not exist (the sweep panics on any `Err`). A stored byte other than 0/1
    /// cannot be written — `setMarginType` validates through [`Self::from_code`] and is the only
    /// writer — and if one appeared anyway it reads as ISOLATED, the mode that keeps a position
    /// off the shared wallet.
    #[inline]
    pub fn effective(stored: u8, cross_enabled: bool) -> Self {
        if cross_enabled && stored == Self::Crossed as u8 {
            Self::Crossed
        } else {
            Self::Isolated
        }
    }
}

// ── Margin tiers ──────────────────────────────────────────────────────────

/// Hard ceiling on the number of tiers one market's table may hold.
///
/// The table is stored INLINE in [`Market`] (see [`MarginTiers`]), so this bounds the
/// blob size and the per-`placeOrder` `Market` memcpy. Raising it later cannot change
/// existing blobs: the codec emits exactly `len` elements, never the padding.
///
/// 16, raised from 8 so a Binance USDⓈ-M bracket table loads row for row: BTCUSDT's
/// `leverageBracket` has 12 brackets (live capture, 2026-10-14). A [`MarginTier`] is 16 bytes
/// (the `u32` rate column sits in what was the `u64`/`u32` pair's padding), so the inline array
/// is 256 bytes.
pub const MAX_MARGIN_TIERS: usize = 16;

/// Denominator of [`MarginTier::maint_margin_rate_ppm`]: parts per million, so
/// `1_000_000` is a 100% maintenance-margin rate and Binance's `maintMarginRatio = 0.004`
/// is `4_000`.
pub const MMR_PPM_ONE: u32 = 1_000_000;

/// Absolute ceiling accepted by `setMarginTiers` for any tier's `max_leverage`.
/// Not a per-market cap — tier 0's `max_leverage` is the per-market `setLeverage` cap.
///
/// 150: Binance USDⓈ-M's highest `initialLeverage`, BTCUSDT's first bracket (150x at a 0.4%
/// maintenance rate in the live `leverageBracket` capture of 2026-10-14), so a Binance table
/// loads as published. A sanity bound against a fat-fingered table, nothing more: no arithmetic
/// depends on it (every use of a tier's leverage is a divisor or an `i128`/`u128` product), and
/// the risk a tier carries is bounded by its own IM > MM check, not by this.
pub const MAX_LEVERAGE_HARD_CAP: u32 = 150;

/// `max_leverage` of the single tier every market is created with.
/// Its maintenance rate `1 / (2 * 3) = 1/6` is the historical hardcoded rate.
pub const DEFAULT_MAX_LEVERAGE: u32 = 3;

/// One row of a market's margin-tier table — one Binance `leverageBracket` row.
///
/// A tier covers notional `[lower_bound_notional, next tier's lower_bound_notional)`
/// and carries the maximum leverage allowed while the position's notional sits in that
/// band, and the band's maintenance-margin RATE: its own `maint_margin_rate_ppm` column
/// when set, else derived as `mmr(n) = 1 / (2 * max_leverage(n))` — see
/// [`crate::math::maintenance_margin`]. Binance's `cum` is not stored; it is a function
/// of the bounds and rates ([`crate::math::leverage_brackets`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarginTier {
    /// Inclusive lower bound of this tier's notional band, in quote units.
    /// Tier 0 is always `0`; the table is strictly increasing in this field.
    pub lower_bound_notional: u64,
    /// Maximum leverage permitted while notional lies in this tier.
    /// Non-increasing across the table (bigger positions get less leverage).
    pub max_leverage: u32,
    /// Binance's `maintMarginRatio` for this band, in parts per million ([`MMR_PPM_ONE`]).
    /// **`0` means DERIVED**: the rate is `1 / (2 * max_leverage)`, the rule every table used
    /// before this column existed, so an all-zero column computes exactly what it always did.
    ///
    /// `setMarginTiers` admits a table only if every tier's effective rate is strictly below
    /// its initial-margin rate `1 / max_leverage` (IM > MM in every tier) and the effective
    /// rates are non-decreasing across the table. Appended last (positional msgpack) with a
    /// serde default, so a two-element row decodes as derived.
    #[serde(default)]
    pub maint_margin_rate_ppm: u32,
}

/// A market's margin-tier table: a FIXED inline array plus a live length, and the table's
/// terminal `notionalCap`.
///
/// Deliberately NOT a `Vec`: [`Market`] is otherwise all scalars, so its clone is a
/// memcpy, and `validate_place_order` clones an owned `Market` on every `placeOrder`.
/// A heap table would put a malloc on the hot order path. Staying `Copy` keeps that
/// property mechanically enforced.
///
/// Serialisation emits exactly `len` elements as a msgpack sequence (never the
/// padding), so raising [`MAX_MARGIN_TIERS`] later cannot change any existing blob.
/// The invariant `1 <= len <= MAX_MARGIN_TIERS` is upheld by every constructor and by
/// the deserialiser, so [`as_slice`](Self::as_slice) and `[0]` indexing cannot panic.
#[derive(Debug, Clone, Copy, Eq)]
pub struct MarginTiers {
    tiers: [MarginTier; MAX_MARGIN_TIERS],
    len: u8,
    /// Binance's `notionalCap` of the LAST bracket — the largest position the table admits at
    /// ANY leverage (Binance refuses beyond it with `-2027`). Every other tier's cap is the next
    /// tier's floor, so this is the one cap that is not derivable from the rows. **`0` means
    /// UNBOUNDED** (the last band runs to infinity), which every table had before this existed.
    /// When set it is strictly above the last tier's `lower_bound_notional` (`setMarginTiers`
    /// checks it). On the wire it is a fourth element of the LAST row, present only when non-zero
    /// (see the `Serialize` impl), so an uncapped table's blob is unchanged.
    notional_cap: u64,
}

impl MarginTiers {
    /// The live tiers, in ascending `lower_bound_notional` order. Never empty.
    #[inline]
    pub fn as_slice(&self) -> &[MarginTier] {
        &self.tiers[..self.len as usize]
    }

    /// Number of live tiers (always `1..=MAX_MARGIN_TIERS`).
    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Always `false` — a table always holds at least tier 0. Present so clippy's
    /// `len_without_is_empty` stays satisfied and callers do not hand-roll the check.
    #[inline]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The last bracket's `notionalCap`: the largest `|notional|` the table admits at any
    /// leverage, exclusive. **`0` = unbounded.**
    #[inline]
    pub fn notional_cap(&self) -> u64 {
        self.notional_cap
    }

    /// Build a table from an already-VALIDATED slice, uncapped. Returns `None` when the slice
    /// is empty or longer than [`MAX_MARGIN_TIERS`]; the ordering/leverage invariants are
    /// the caller's job (`run_set_margin_tiers` checks them, each with its own error).
    pub fn from_tiers(src: &[MarginTier]) -> Option<Self> {
        if src.is_empty() || src.len() > MAX_MARGIN_TIERS {
            return None;
        }
        let mut tiers = [MarginTier {
            lower_bound_notional: 0,
            max_leverage: 0,
            maint_margin_rate_ppm: 0,
        }; MAX_MARGIN_TIERS];
        tiers[..src.len()].copy_from_slice(src);
        Some(Self {
            tiers,
            len: src.len() as u8,
            notional_cap: 0,
        })
    }

    /// The same table with its terminal [`notional_cap`](Self::notional_cap) set (`0` =
    /// unbounded). Already-VALIDATED: `run_set_margin_tiers` checks a non-zero cap is above the
    /// last tier's floor.
    pub fn with_notional_cap(mut self, notional_cap: u64) -> Self {
        self.notional_cap = notional_cap;
        self
    }
}

impl Default for MarginTiers {
    /// One tier `{0, DEFAULT_MAX_LEVERAGE}` — maintenance rate `1/6`, the pre-tier
    /// hardcoded behaviour — and no cap.
    fn default() -> Self {
        Self::from_tiers(&[MarginTier {
            lower_bound_notional: 0,
            max_leverage: DEFAULT_MAX_LEVERAGE,
            maint_margin_rate_ppm: 0,
        }])
        .expect("one tier is within MAX_MARGIN_TIERS")
    }
}

/// Compares the LIVE tiers and the cap — the padding beyond `len` is not state.
impl PartialEq for MarginTiers {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice() && self.notional_cap == other.notional_cap
    }
}

/// Hand-rolled so the blob carries exactly `len` elements as a plain msgpack sequence
/// (`[[lb, lev, mmr_ppm], ...]`) — never the fixed-array padding. That is what decouples the
/// on-chain encoding from [`MAX_MARGIN_TIERS`].
///
/// A non-zero [`notional_cap`](MarginTiers::notional_cap) rides on the LAST row as a fourth,
/// positional element (`[lb, lev, mmr_ppm, cap]`) — the append-only rule applied to that row, and
/// where Binance itself puts it. A zero cap is not emitted at all, so an uncapped table encodes
/// exactly as it did before the cap existed.
impl Serialize for MarginTiers {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let live = self.as_slice();
        let last = live.len() - 1;
        let mut seq = serializer.serialize_seq(Some(live.len()))?;
        for (i, tier) in live.iter().enumerate() {
            if i == last && self.notional_cap != 0 {
                seq.serialize_element(&(
                    tier.lower_bound_notional,
                    tier.max_leverage,
                    tier.maint_margin_rate_ppm,
                    self.notional_cap,
                ))?;
            } else {
                seq.serialize_element(tier)?;
            }
        }
        seq.end()
    }
}

/// One row as the codec carries it: a [`MarginTier`] plus the optional trailing cap, every
/// element after the first two defaulted (positional msgpack), so `[lb, lev]`, `[lb, lev, mmr]`
/// and `[lb, lev, mmr, cap]` all decode.
#[derive(Deserialize)]
struct WireTier {
    lower_bound_notional: u64,
    max_leverage: u32,
    #[serde(default)]
    maint_margin_rate_ppm: u32,
    #[serde(default)]
    notional_cap: u64,
}

impl<'de> Deserialize<'de> for MarginTiers {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TiersVisitor;

        impl<'de> Visitor<'de> for TiersVisitor {
            type Value = MarginTiers;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a sequence of 1..={MAX_MARGIN_TIERS} margin tiers")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<MarginTiers, A::Error> {
                let mut tiers = [MarginTier {
                    lower_bound_notional: 0,
                    max_leverage: 0,
                    maint_margin_rate_ppm: 0,
                }; MAX_MARGIN_TIERS];
                let mut len = 0usize;
                let mut notional_cap = 0u64;
                while let Some(row) = seq.next_element::<WireTier>()? {
                    if len == MAX_MARGIN_TIERS {
                        return Err(de::Error::custom("margin tiers: too many tiers"));
                    }
                    // Only the LAST row may carry a cap: one followed by another row is not a
                    // blob this codec writes.
                    if notional_cap != 0 {
                        return Err(de::Error::custom(
                            "margin tiers: notionalCap on a non-final tier",
                        ));
                    }
                    tiers[len] = MarginTier {
                        lower_bound_notional: row.lower_bound_notional,
                        max_leverage: row.max_leverage,
                        maint_margin_rate_ppm: row.maint_margin_rate_ppm,
                    };
                    notional_cap = row.notional_cap;
                    len += 1;
                }
                // A corrupt/truncated blob must NOT yield an empty table: every reader
                // indexes `[0]` (the setLeverage cap) and walks from tier 0.
                if len == 0 {
                    return Err(de::Error::custom("margin tiers: table must not be empty"));
                }
                Ok(MarginTiers {
                    tiers,
                    len: len as u8,
                    notional_cap,
                })
            }
        }

        deserializer.deserialize_seq(TiersVisitor)
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
    /// Liquidation clearance fee in basis points (1 bps = 0.01%) of the NOTIONAL a liquidation
    /// closes (valued at the mark), for both margin types — Binance's `liquidationFee` (125 bps on
    /// BTCUSDT; cross margin P3, owner decision Q3). Charged from the liquidated wallet, capped at
    /// what the close returned to it (isolated) or at `max(0, E_after)` (cross), so a bankrupt
    /// liquidation pays nothing; credited to the Insurance Fund. 0 = no fee.
    #[serde(default, rename = "lf")]
    pub liquidation_fee_rate_bps: u32,
    /// Price band half-width in basis points (1 bps = 0.01%). A limit order is
    /// rejected at placement if its price lies outside `mark ± price_band_bps`.
    /// `0` means "use `DEFAULT_PRICE_BAND_BPS`"; a large value (e.g. `>= 10_000`)
    /// effectively disables the band. Resolve via `crate::math::effective_price_band_bps`.
    #[serde(default, rename = "pb")]
    pub price_band_bps: u32,
    /// Mark price (oracle-driven, in `price_decimals` units). Lives in the Market blob — NOT in
    /// `MarketHot` — because it is WRITE-RARE (only `updateIndexPrice`/`addMarket` write it, at
    /// oracle cadence, never per-trade) yet READ-HOT and always co-read with the market config
    /// (band check, maker settlement). Keeping it here means a single `load_market` yields config +
    /// mark, and callers already holding `&Market` (validate, the match walk) read `market.mark_price`
    /// with zero extra probe. `updateMarket` is load-modify-save so it preserves this field.
    #[serde(default, rename = "mp")]
    pub mark_price: u64,
    /// Margin-tier table: the per-market maintenance-margin rate schedule and the
    /// per-notional leverage caps. APPENDED LAST — the codec is positional msgpack, so
    /// fields may only ever be appended. Written by `setMarginTiers` (never by
    /// `updateMarket`, so retuning tick/step can never reset the risk table); read by
    /// [`crate::math::maintenance_margin`] / `max_leverage_for_notional`.
    /// Deliberately NOT in [`MarketHot`]: that blob is re-serialised into the block
    /// delta on every fill, and this is write-rare config.
    #[serde(default, rename = "mt")]
    pub tiers: MarginTiers,
    /// Which basis mechanism feeds Price 2 of the mark. APPENDED LAST — positional msgpack.
    ///
    /// [`BASIS_MODE_WINDOW`] (the default, and what every existing market keeps) is the
    /// 30-second time-weighted average over [`crate::types::PriceBasisWindow`], sampled on every
    /// best-quote change. The USD-M markets are aligned to Binance on this and do not move.
    ///
    /// [`BASIS_MODE_EWMA`] is the oracle-cadence EWMA of `mid − index`. It exists for markets
    /// whose index can be derived from their own book (`updateIndexPriceFromBook`), where the
    /// window form's index-velocity bias and 30-second reach are both wrong.
    ///
    /// Per MARKET, deliberately not per selector: an equity market uses the external path on a
    /// weekday and the book path at the weekend, and switching basis formula between them would
    /// jump price2 at exactly the handover the index ramp exists to smooth.
    #[serde(default, rename = "bm")]
    pub basis_mode: u8,
    /// `1` when the last sweep over this market STOPPED ON A BUDGET and left work behind.
    /// APPENDED LAST — positional msgpack.
    ///
    /// It is a hint for `drainDeferredWork`, not a ledger. Nothing is enqueued anywhere: both
    /// sweeps re-derive their candidates from scratch every run (the position registry, the
    /// out-of-band price prefix), so this bit only answers "is it worth looking", never "look
    /// here".
    ///
    /// # It is deliberately CONSERVATIVE IN ONE DIRECTION ONLY
    ///
    /// `1` can be stale-high: a budget consumed exactly as the work finished sets it, and the next
    /// drain finds nothing and clears it. One wasted call, no harm.
    ///
    /// `0` can be stale-low, and that is the case that matters. Work appears WITHOUT any budget
    /// break — funding accrual alone pushes a position under maintenance between oracle ticks, with
    /// no mark move anywhere. So a `0` is NOT a proof that there is nothing to do.
    ///
    /// That asymmetry is only acceptable because the drain is ADDITIVE: the sweeps still run inside
    /// every `updateIndexPrice`, so a false `0` degrades to exactly today's behaviour — the work
    /// waits for the next oracle tick — rather than being dropped. **Do not make this bit the sole
    /// trigger.**
    ///
    /// Written ONLY on transition. The oracle path rewrites `Market` every update anyway
    /// (`save_mark_price` goes through `mutate_market`), so the saving is on the drain path, where
    /// a no-op call must cost one read and nothing else.
    #[serde(default, rename = "dw")]
    pub deferred_work: u8,
    /// `fx` — a MIRROR of this market's [`crate::types::FundingState::cumulative_funding_index`].
    /// APPENDED LAST — positional msgpack; serde default `0`, which is also the index every market
    /// starts from.
    ///
    /// # Why the index is copied here (cross margin P2, graft 2 of the design report)
    ///
    /// A CROSS account's balance includes the funding every cross position has accrued but not yet
    /// settled (`cWB = W + Σ_C PF_k`, and funding settles lazily — see `perp_engine::funding`), so
    /// every account-level fold needs each market's cumulative index. The account walk already
    /// loads `{Market, position}` per market; reading the index off `FundingState` would add a
    /// THIRD load per market to every fold, on the admission gates and (from P3) on every cross
    /// candidate of the liquidation sweep. The `Market` blob is rewritten on every oracle tick
    /// anyway (`storage::save_mark_price`), and the index only moves on an epoch boundary of that
    /// same tick, so mirroring it costs no additional key in any block's delta.
    ///
    /// # `FundingState` stays the source of truth
    ///
    /// The ONE writer is `storage::save_funding_state` — every write of the index goes through it,
    /// and it copies the new value here in the same breath. Funding SETTLEMENT still reads
    /// `FundingState` itself; only the read-only folds read this copy. A `debug_assertions`
    /// cross-check compares the two wherever both are in hand
    /// (`funding::compute_funding_settlement`, and the cross fold in `margin_view`).
    #[serde(default, rename = "fx")]
    pub cumulative_funding_index: i128,
}

/// [`Market::basis_mode`]: the legacy 30-second time-weighted window. Default.
pub const BASIS_MODE_WINDOW: u8 = 0;
/// [`Market::basis_mode`]: oracle-cadence EWMA of `mid − index`.
pub const BASIS_MODE_EWMA: u8 = 1;

/// Per-market HOT scalars that change PER-TRADE, grouped into ONE off-trie blob so co-accessing
/// them costs a SINGLE probe/decode/Arc sharing one cache line (was four separate keys). All `Copy`
/// u64s → whole-struct clone is a trivial memcpy. Grouping coarsens the block-delta to per-market
/// (any field write re-emits the blob) — fine here since these all move together on a trade.
/// (Mark price is deliberately NOT here — it is write-rare + co-read with config, so it lives in
/// [`Market`]; see its `mark_price` field.)
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct MarketHot {
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
    fn clear_side_aggregates_zeroes_bid_and_ask() {
        let mut p = PerpPosition {
            total_buy_qty: 7,
            total_buy_notional: 1000,
            total_sell_qty: 3,
            total_sell_notional: 400,
            amount: 5,
            margin: 100,
            ..PerpPosition::default()
        };
        p.clear_side_aggregates();
        assert_eq!(
            (
                p.total_buy_qty,
                p.total_buy_notional,
                p.total_sell_qty,
                p.total_sell_notional
            ),
            (0, 0, 0, 0)
        );
        // Only the order-book aggregates move — the position itself is untouched.
        assert_eq!((p.amount, p.margin), (5, 100));
    }

    // ── Margin type ───────────────────────────────────────────────────────

    /// The wire codes are a CONTRACT (stored on every position, carried by three ABI surfaces), so
    /// they are pinned literally; anything else decodes to `None`, never onto a neighbour.
    #[test]
    fn the_margin_type_wire_codes_round_trip() {
        for (t, code) in [(MarginType::Crossed, 0u8), (MarginType::Isolated, 1)] {
            assert_eq!(t.code(), code, "{t:?}");
            assert_eq!(MarginType::from_code(code), Some(t));
        }
        for code in 2..=u8::MAX {
            assert_eq!(MarginType::from_code(code), None, "code {code}");
        }
        // A position that never chose is CROSSED — Binance's default — in STORAGE.
        assert_eq!(PerpPosition::default().margin_type, MarginType::Crossed.code());
    }

    /// The gate: disabled ⇒ ISOLATED whatever is stored; enabled ⇒ the stored choice, with an
    /// unwritable byte reading as ISOLATED (total, no error path).
    #[test]
    fn the_effective_margin_type_is_isolated_unless_cross_is_enabled() {
        for stored in 0..=u8::MAX {
            assert_eq!(MarginType::effective(stored, false), MarginType::Isolated, "{stored}");
        }
        assert_eq!(MarginType::effective(0, true), MarginType::Crossed);
        assert_eq!(MarginType::effective(1, true), MarginType::Isolated);
        for stored in 2..=u8::MAX {
            assert_eq!(MarginType::effective(stored, true), MarginType::Isolated, "{stored}");
        }
    }

    /// Appended LAST and defaulted: a blob written before the field existed decodes as CROSSED
    /// (stored), and a non-default value survives the codec.
    #[test]
    fn the_margin_type_is_appended_and_round_trips() {
        let p = PerpPosition {
            amount: 3,
            margin_type: MarginType::Isolated.code(),
            ..PerpPosition::default()
        };
        let bytes = crate::codec::encode(&p).unwrap();
        let back: PerpPosition = crate::codec::decode(&bytes).unwrap();
        assert_eq!(back, p);
        // The pre-P1 layout is this one minus its last element: drop the trailing fixint and fix
        // the array header (positional msgpack: 0x9N fixarray).
        let mut old = bytes.clone();
        assert_eq!(old.pop(), Some(MarginType::Isolated.code()), "the type byte is LAST");
        old[0] -= 1;
        let back: PerpPosition = crate::codec::decode(&old).unwrap();
        assert_eq!(back.margin_type, MarginType::Crossed.code());
        assert_eq!(back.amount, 3);
    }

    // ── MarginTiers codec ─────────────────────────────────────────────────

    fn tier(lower_bound_notional: u64, max_leverage: u32) -> MarginTier {
        MarginTier {
            lower_bound_notional,
            max_leverage,
            maint_margin_rate_ppm: 0,
        }
    }

    #[test]
    fn margin_tiers_default_is_the_single_legacy_tier() {
        let d = MarginTiers::default();
        assert_eq!(d.len(), 1);
        assert_eq!(d.as_slice(), &[tier(0, DEFAULT_MAX_LEVERAGE)]);
    }

    #[test]
    fn margin_tiers_round_trip_at_len_one_and_len_max() {
        for rows in [
            vec![tier(0, 3)],
            (0..MAX_MARGIN_TIERS)
                .map(|i| tier(i as u64 * 1_000_000, (MAX_MARGIN_TIERS - i) as u32))
                .collect::<Vec<_>>(),
        ] {
            let t = MarginTiers::from_tiers(&rows).unwrap();
            let bytes = crate::codec::encode(&t).unwrap();
            let back: MarginTiers = crate::codec::decode(&bytes).unwrap();
            assert_eq!(back.as_slice(), rows.as_slice());
            assert_eq!(back, t);
            // The blob carries exactly `len` elements — never the fixed-array padding,
            // so raising MAX_MARGIN_TIERS cannot change an existing blob.
            let as_vec: Vec<MarginTier> = crate::codec::decode(&bytes).unwrap();
            assert_eq!(as_vec, rows);
        }
    }

    /// The rate column is a positional TAIL field: a row carries it as its third element, and a
    /// two-element row — the encoding before the column existed — still decodes, as DERIVED (0).
    #[test]
    fn margin_tier_rate_column_is_an_appended_defaulted_field() {
        let rated = MarginTier {
            lower_bound_notional: 50_000_000_000,
            max_leverage: 100,
            maint_margin_rate_ppm: 5_000,
        };
        let bytes = crate::codec::encode(&rated).unwrap();
        let (lb, lev, ppm): (u64, u32, u32) = crate::codec::decode(&bytes).unwrap();
        assert_eq!((lb, lev, ppm), (50_000_000_000, 100, 5_000));
        assert_eq!(crate::codec::decode::<MarginTier>(&bytes).unwrap(), rated);

        let legacy = crate::codec::encode(&(1_000u64, 3u32)).unwrap();
        assert_eq!(
            crate::codec::decode::<MarginTier>(&legacy).unwrap(),
            tier(1_000, 3),
            "a two-element row is a derived tier"
        );
        let legacy_table = crate::codec::encode(&vec![(0u64, 5u32), (1_000, 3)]).unwrap();
        let t: MarginTiers = crate::codec::decode(&legacy_table).unwrap();
        assert_eq!(t.as_slice(), &[tier(0, 5), tier(1_000, 3)]);
    }

    #[test]
    fn margin_tiers_decoder_rejects_empty_and_oversized_tables() {
        let empty = crate::codec::encode(&Vec::<MarginTier>::new()).unwrap();
        assert!(
            crate::codec::decode::<MarginTiers>(&empty).is_err(),
            "len == 0 must be rejected — every reader indexes [0]"
        );

        let too_many: Vec<MarginTier> = (0..MAX_MARGIN_TIERS + 1)
            .map(|i| tier(i as u64, 1))
            .collect();
        let bytes = crate::codec::encode(&too_many).unwrap();
        assert!(crate::codec::decode::<MarginTiers>(&bytes).is_err());
    }

    #[test]
    fn margin_tiers_from_tiers_rejects_out_of_range_lengths() {
        assert!(MarginTiers::from_tiers(&[]).is_none());
        let too_many: Vec<MarginTier> = (0..MAX_MARGIN_TIERS + 1)
            .map(|i| tier(i as u64, 1))
            .collect();
        assert!(MarginTiers::from_tiers(&too_many).is_none());
        assert!(MarginTiers::from_tiers(&too_many[..MAX_MARGIN_TIERS]).is_some());
    }

    #[test]
    fn margin_tiers_equality_ignores_the_unused_padding() {
        let a = MarginTiers::from_tiers(&[tier(0, 3)]).unwrap();
        let mut b = MarginTiers::from_tiers(&[tier(0, 3), tier(9, 1)]).unwrap();
        b.len = 1; // shrink: the stale second row must not affect equality
        assert_eq!(a, b);
    }

    #[test]
    fn market_blob_round_trips_the_tier_table() {
        let mut m = Market {
            market_id: 1,
            base_decimals: 8,
            price_decimals: 2,
            tick_size: 1,
            step_size: 1,
            min_quantity: 1,
            max_quantity: 1_000,
            max_price: 1_000,
            price_update_interval: 5,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 0,
            mark_price: 100,
            tiers: MarginTiers::default(),
            basis_mode: 0,
            // Non-zero deliberately: the positional codec appends this LAST, and a `0` here would
            // round-trip even if the field were dropped from the encoding entirely.
            deferred_work: 1,
            // Non-zero and beyond i64 for the same reason: the mirrored funding index is an i128.
            cumulative_funding_index: -12_345_678_901_234_567_890,
        };
        m.tiers =
            MarginTiers::from_tiers(&[tier(0, 3), tier(50_000, 2), tier(250_000, 1)]).unwrap();
        let bytes = crate::codec::encode(&m).unwrap();
        let back: Market = crate::codec::decode(&bytes).unwrap();
        assert_eq!(back, m);

        // …and its terminal cap, which rides on the table's last row.
        m.tiers = m.tiers.with_notional_cap(1_800_000);
        let bytes = crate::codec::encode(&m).unwrap();
        let back: Market = crate::codec::decode(&bytes).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.tiers.notional_cap(), 1_800_000);
    }

    /// The terminal `notionalCap` is a fourth positional element of the LAST row only, emitted only
    /// when set: an uncapped table encodes byte for byte as a plain sequence of rows (so no blob
    /// written before the cap existed changes), a capped one round-trips, and a cap on any other
    /// row is not a blob this codec writes — refused.
    #[test]
    fn margin_tiers_carry_the_notional_cap_on_the_last_row_only() {
        let rows = [tier(0, 150), tier(300_000, 100), tier(1_200_000, 1)];
        let uncapped = MarginTiers::from_tiers(&rows).unwrap();
        assert_eq!(uncapped.notional_cap(), 0, "a table is unbounded unless capped");
        assert_eq!(
            crate::codec::encode(&uncapped).unwrap(),
            crate::codec::encode(&rows.to_vec()).unwrap(),
            "an uncapped table is exactly its rows"
        );

        let capped = uncapped.with_notional_cap(1_800_000);
        assert_ne!(capped, uncapped, "the cap is state");
        let bytes = crate::codec::encode(&capped).unwrap();
        let back: MarginTiers = crate::codec::decode(&bytes).unwrap();
        assert_eq!(back, capped);
        assert_eq!(back.notional_cap(), 1_800_000);
        let wire: (MarginTier, MarginTier, (u64, u32, u32, u64)) =
            crate::codec::decode(&bytes).unwrap();
        assert_eq!(wire.0, rows[0]);
        assert_eq!(wire.1, rows[1]);
        assert_eq!(wire.2, (1_200_000, 1, 0, 1_800_000), "the cap is the last row's 4th element");

        let misplaced = crate::codec::encode(&(
            (0u64, 150u32, 0u32, 1_800_000u64),
            (300_000u64, 100u32, 0u32),
        ))
        .unwrap();
        assert!(
            crate::codec::decode::<MarginTiers>(&misplaced).is_err(),
            "a cap on a non-final row must be refused"
        );
    }
}
