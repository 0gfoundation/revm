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
    /// Margin (collateral) allocated to this position.
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
    // `calc_value(price, amount)` at each order's LIMIT price (each floored exactly as a fold
    // over the list produces it) → maintainable ± one term with zero floor-composition error.
    // Kept in sync at every order-list mutation (place/cancel incrementally; fills/liquidation by
    // recompute-from-list). Derivable from the lists via `math::sum_side_totals`, so a
    // genesis/default 0 is correct only for an empty book.
    //
    // `total_buy_notional` IS Binance's `bidNotional`: a LONG order's Assuming Price is its own
    // limit price, so the limit-price fold is the requirement basis outright.
    //
    // ⚠️ `total_sell_notional` is NOT `askNotional`. A SHORT order is priced at
    // `max(ROUND_UP(lastTraded × 1.0015), mark, limit)`, so this field is only the BASELINE the
    // Assuming-Price uplift is added to; the requirement's `Ask` is re-folded from the sell LIST at
    // the current floor on every evaluation (`margin_view::stored_ask_assuming`) and, unlike this
    // field, moves with the mark. Both are proven equal to the resting-order fold after every
    // transition by `side_aggregates_are_exactly_bid_and_ask_after_every_operation`.
    //
    // Together with `amount`, `leverage`, the mark, the last traded price and the sell list, these
    // are the inputs to the DERIVED open-order requirement that replaced the escrow — see the note
    // where the escrow fields used to be.
    /// Σ resting BUY order amounts (base units).
    #[serde(default, rename = "tbq")]
    pub total_buy_qty: u64,
    /// Σ `calc_value(price, amount)` over resting BUY orders (quote units).
    #[serde(default, rename = "tbn")]
    pub total_buy_notional: u64,
    /// Σ resting SELL order amounts (base units).
    #[serde(default, rename = "tsq")]
    pub total_sell_qty: u64,
    /// Σ `calc_value(price, amount)` over resting SELL orders (quote units).
    #[serde(default, rename = "tsn")]
    pub total_sell_notional: u64,
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
        }
    }
}

// ── Margin tiers ──────────────────────────────────────────────────────────

/// Hard ceiling on the number of tiers one market's table may hold.
///
/// The table is stored INLINE in [`Market`] (see [`MarginTiers`]), so this bounds the
/// blob size and the per-`placeOrder` `Market` memcpy. Raising it later cannot change
/// existing blobs: the codec emits exactly `len` elements, never the padding.
pub const MAX_MARGIN_TIERS: usize = 8;

/// Absolute ceiling accepted by `setMarginTiers` for any tier's `max_leverage`.
/// Not a per-market cap — tier 0's `max_leverage` is the per-market `setLeverage` cap.
pub const MAX_LEVERAGE_HARD_CAP: u32 = 100;

/// `max_leverage` of the single tier every market is created with.
/// Its maintenance rate `1 / (2 * 3) = 1/6` is the historical hardcoded rate.
pub const DEFAULT_MAX_LEVERAGE: u32 = 3;

/// One row of a market's margin-tier table.
///
/// A tier covers notional `[lower_bound_notional, next tier's lower_bound_notional)`
/// and carries the maximum leverage allowed while the position's notional sits in that
/// band. The maintenance-margin RATE of a tier is derived from it:
/// `mmr(n) = 1 / (2 * max_leverage(n))` — see [`crate::math::maintenance_margin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarginTier {
    /// Inclusive lower bound of this tier's notional band, in quote units.
    /// Tier 0 is always `0`; the table is strictly increasing in this field.
    pub lower_bound_notional: u64,
    /// Maximum leverage permitted while notional lies in this tier.
    /// Non-increasing across the table (bigger positions get less leverage).
    pub max_leverage: u32,
}

/// A market's margin-tier table: a FIXED inline array plus a live length.
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

    /// Build a table from an already-VALIDATED slice. Returns `None` when the slice is
    /// empty or longer than [`MAX_MARGIN_TIERS`]; the ordering/leverage invariants are
    /// the caller's job (`run_set_margin_tiers` checks them, each with its own error).
    pub fn from_tiers(src: &[MarginTier]) -> Option<Self> {
        if src.is_empty() || src.len() > MAX_MARGIN_TIERS {
            return None;
        }
        let mut tiers = [MarginTier {
            lower_bound_notional: 0,
            max_leverage: 0,
        }; MAX_MARGIN_TIERS];
        tiers[..src.len()].copy_from_slice(src);
        Some(Self {
            tiers,
            len: src.len() as u8,
        })
    }
}

impl Default for MarginTiers {
    /// One tier `{0, DEFAULT_MAX_LEVERAGE}` — maintenance rate `1/6`, the pre-tier
    /// hardcoded behaviour.
    fn default() -> Self {
        Self::from_tiers(&[MarginTier {
            lower_bound_notional: 0,
            max_leverage: DEFAULT_MAX_LEVERAGE,
        }])
        .expect("one tier is within MAX_MARGIN_TIERS")
    }
}

/// Compares only the LIVE tiers — the padding beyond `len` is not state.
impl PartialEq for MarginTiers {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

/// Hand-rolled so the blob carries exactly `len` elements as a plain msgpack sequence
/// (`[[lb, lev], ...]`) — never the fixed-array padding. That is what decouples the
/// on-chain encoding from [`MAX_MARGIN_TIERS`].
impl Serialize for MarginTiers {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let live = self.as_slice();
        let mut seq = serializer.serialize_seq(Some(live.len()))?;
        for tier in live {
            seq.serialize_element(tier)?;
        }
        seq.end()
    }
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
                }; MAX_MARGIN_TIERS];
                let mut len = 0usize;
                while let Some(tier) = seq.next_element::<MarginTier>()? {
                    if len == MAX_MARGIN_TIERS {
                        return Err(de::Error::custom("margin tiers: too many tiers"));
                    }
                    tiers[len] = tier;
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
    /// Liquidation clearance fee in basis points (1 bps = 0.01%).
    /// Charged from remaining margin on solvent liquidations; credited to Insurance Fund.
    /// 0 = no fee.
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
}

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

    // ── MarginTiers codec ─────────────────────────────────────────────────

    fn tier(lower_bound_notional: u64, max_leverage: u32) -> MarginTier {
        MarginTier {
            lower_bound_notional,
            max_leverage,
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
        };
        m.tiers =
            MarginTiers::from_tiers(&[tier(0, 3), tier(50_000, 2), tier(250_000, 1)]).unwrap();
        let bytes = crate::codec::encode(&m).unwrap();
        let back: Market = crate::codec::decode(&bytes).unwrap();
        assert_eq!(back, m);
    }
}
