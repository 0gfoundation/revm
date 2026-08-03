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
    pub price: u64,
    /// Remaining (unfilled) amount tracked for margin purposes.
    pub amount: u64,
    /// Maker fee rate snapshotted when the order started resting.
    #[serde(default, rename = "MFB")]
    pub maker_fee_bps: u64,
}
