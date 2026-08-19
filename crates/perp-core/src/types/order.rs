//! Order types for the PerpDEX precompile.
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

// ── Enums ─────────────────────────────────────────────────────────────────
// Enums serialize as their `u8` discriminant (serde_repr) rather than the variant NAME string
// (P4/#20) — e.g. OrderStatus "PartiallyFilled" 16 B → 1 B in every Order blob.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum Side {
    Buy = 0,
    Sell = 1,
}

impl Side {
    #[inline]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Side::Buy),
            1 => Some(Side::Sell),
            _ => None,
        }
    }
    #[inline]
    pub fn opposite(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

/// Only Limit and Market are implemented; other types are reserved for future use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum OrderType {
    Limit = 0,
    Market = 1,
}

impl OrderType {
    #[inline]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(OrderType::Limit),
            1 => Some(OrderType::Market),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum TimeInForce {
    /// Good Till Cancel – resting order until manually cancelled.
    Gtc = 0,
    /// Immediate or Cancel – fill what you can, cancel the rest.
    Ioc = 1,
    /// Fill or Kill – fill entirely or cancel entirely.
    Fok = 2,
    /// Post-Only – reject if the order would immediately match.
    PostOnly = 3,
}

impl TimeInForce {
    #[inline]
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(TimeInForce::Gtc),
            1 => Some(TimeInForce::Ioc),
            2 => Some(TimeInForce::Fok),
            3 => Some(TimeInForce::PostOnly),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum OrderStatus {
    Open = 0,
    PartiallyFilled = 1,
    Filled = 2,
    Cancelled = 3,
    /// TIF-based system cancellation: IOC/FOK that could not be completely
    /// filled.  Mirrors Binance's EXPIRED status — distinct from Cancelled
    /// (user-initiated) so consumers can tell the two apart.
    Expired = 4,
}

impl OrderStatus {
    /// A terminal status = the order will never trade again. Under delete-on-terminal the order
    /// record is DELETED from storage the moment it reaches one of these (history is disposable —
    /// off-chain indexers reconstruct it from the OrderFilled/OrderCancelled event stream), so the
    /// order map holds only live (Open/PartiallyFilled) orders and never grows unbounded.
    #[inline]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Expired
        )
    }
}

// ── Structs ───────────────────────────────────────────────────────────────

/// Full on-chain order record, stored keyed by `order_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Order {
    /// Owner address (20 bytes). `serde_bytes` → msgpack bin (1 byte/byte) instead of an array of
    /// 20 integers (~2 bytes/byte) (P4/#20).
    #[serde(with = "serde_bytes")]
    pub owner: [u8; 20],
    pub market_id: u64,
    pub side: Side,
    /// Price in the market's configured `price_decimals` fixed-point units.
    /// Set to 0 for market orders.
    pub price: u64,
    /// Original total quantity (base-asset units with `base_decimals`).
    pub quantity: u64,
    /// Quantity filled so far.
    pub filled: u64,
    pub order_type: OrderType,
    pub tif: TimeInForce,
    pub status: OrderStatus,
}

/// Lightweight per-user per-market order entry used for margin-reserve calculation.
///
/// Mirrors `balance::order_entry::OrderEntry` from the offchain engine.
/// Buy entries are sorted by price DESC; sell entries by price ASC.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct OrderEntry {
    /// 32-byte order ID. `serde_bytes` → msgpack bin instead of a 32-integer array (P4/#20).
    #[serde(with = "serde_bytes")]
    pub order_id: [u8; 32],
    /// The order's LIMIT price, in the market's `price_decimals` units.
    ///
    /// **Everything execution touches keys on this field**: matching, price priority, the sorted
    /// insert position (buys DESC / sells ASC), the book's price levels, the fill price, the maker
    /// fee, and `find_entry_by_price_id`'s binary search. See [`Self::assuming_price`] for the one
    /// thing that does not.
    pub price: u64,
    /// Remaining (unfilled) amount tracked for margin purposes.
    pub amount: u64,
    /// Maker fee rate snapshotted when the order started resting.
    #[serde(default, rename = "MFB")]
    pub maker_fee_bps: u64,
    /// The order's **Assuming Price**, FROZEN at the instant it was placed — the price this entry's
    /// contribution to its side's margin aggregate (`Bid`/`Ask`) is valued at for as long as it
    /// rests. ONE rule covers both sides:
    ///
    /// ```text
    /// BUY   assuming_price = limit price                                  (no markup, measured)
    /// SELL  assuming_price = max(T, limit),  T = max(⌈last × 1.0015⌉, mark)   AT PLACEMENT TIME
    /// contribution         = calc_value(assuming_price, amount)
    /// ```
    ///
    /// The PRICE is frozen, not the contribution: a partial fill shrinks `amount`, so the term has
    /// to scale with what is left.
    ///
    /// # ⚠️ NEVER use this for matching, price priority, sorting, price levels or the fill price
    ///
    /// It is a MARGIN BASIS and nothing else. A sell resting below `T` carries an `assuming_price`
    /// ABOVE its own limit; keying the book on it would refuse fills the limit price accepts, put
    /// the entry at the wrong sorted position, and charge the wrong fee. Every one of those reads
    /// [`Self::price`], with no exception.
    ///
    /// # Why frozen, and not recomputed at read
    ///
    /// MEASURED. `misc/binance-flip-and-admission.md` §3.13 (R12): 3 trials × 30 frames, the
    /// reported `askNotional` of a resting sell never moved while `Last` walked 32.80 USD, with
    /// 9 consecutive frames below the `P_s / 1.0015` kink where a recomputed-at-read value has to
    /// plateau. `H_live` was refused by 1939 quanta — a shape-level refutation, not a slope-level
    /// one. The doc's instruction is explicit: 「我们自己的账必须存下单时的值,不能每次重算。」
    ///
    /// ⚠️ Scope. What is frozen is each RESTING ORDER's contribution. `ooIM` as a whole is NOT
    /// frozen: it still contains `N = |position| × mark`, recomputed at every read
    /// (`crate::math::open_order_margin`), independently measured by R10's 15 dense snapshots. The
    /// stronger-sounding claim「挂单的托管是个常数」holds only at `N = 0`, which is the branch R12
    /// measured — do not extrapolate it.
    #[serde(default, rename = "ap")]
    pub assuming_price: u64,
}

impl OrderEntry {
    /// This entry's contribution to its side's margin aggregate: `calc_value(assuming_price,
    /// amount)`. **The single definition of a resting order's margin term**, for both sides — the
    /// aggregate is exactly `Σ` of this over the side's list, and every maintenance site adds or
    /// subtracts this same per-order-floored value, so the maintained total stays byte-identical to
    /// a fresh fold.
    #[inline]
    pub fn margin_notional(
        &self,
        base_decimals: u32,
        price_decimals: u32,
    ) -> Result<u64, crate::error::PerpError> {
        crate::math::calc_value(
            self.assuming_price,
            self.amount,
            base_decimals,
            price_decimals,
        )
    }
}
