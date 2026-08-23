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

/// What an order **is**, as ONE value — the engine's internal order model.
///
/// The wire carries `(orderType, tif)` as two independent `uint8`s, and their product contains
/// combinations that are not products at all:
///
/// * `Market + Fok` — all-or-nothing *at market*. **No exchange sells this**: Binance spot rejects
///   a `timeInForce` on a MARKET order outright, and USDⓈ-M bounds a market order with the
///   mark-anchored Price Cap/Floor Ratio, not with a TIF.
/// * `Market + PostOnly` — a "never take liquidity" flag on the one order type that does nothing
///   BUT take liquidity.
/// * `Market + Gtc` — "good till cancelled", whose remainder is discarded on the spot. The name
///   lies about the behaviour.
///
/// Collapsing the pair into this enum at the validation boundary ([`Self::from_parts`]) makes all
/// three UNREPRESENTABLE past that boundary: a market order has no TIF field to set, so no code
/// downstream can branch on one, and no future edit can accidentally grow a fourth meaning.
///
/// The mechanism this encodes: **a market order is an immediate-or-cancel with a price band.** It
/// matches from the best price inward and stops on (a) fully filled, (b) the exchange price band,
/// or (c) an exhausted book; whatever is left is discarded and never rests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    /// A market order: implicitly immediate-or-cancel, bounded by the price band. Carries NO
    /// time-in-force — that is the whole point of this type.
    Market,
    /// A limit order at the given time-in-force. All four TIFs are products here.
    Limit(TimeInForce),
}

impl OrderKind {
    /// The ONE mapping from the validated wire pair into the internal model.
    /// `None` = an illegal pair (a market order carrying a TIF that is not the unset placeholder);
    /// the caller turns that into a reject. Both inputs are already-decoded enums, so `None` has
    /// exactly one meaning.
    ///
    /// `Market + Gtc` is ACCEPTED and means `Market`: `Gtc` is discriminant 0, i.e. what a caller
    /// that leaves the field unset sends, and it is the placeholder Binance's own spot response
    /// echoes back on a market order. `Market + Ioc` is accepted as the explicit spelling of what a
    /// market order already is. `Fok` and `PostOnly` on a market order are the substantive rejects —
    /// each names a product that does not exist.
    #[inline]
    pub fn from_parts(order_type: OrderType, tif: TimeInForce) -> Option<Self> {
        // Deliberately exhaustive (no `_` arm): a new OrderType or TimeInForce variant must come
        // back here and state its legality instead of silently inheriting one.
        match (order_type, tif) {
            (OrderType::Limit, t) => Some(OrderKind::Limit(t)),
            (OrderType::Market, TimeInForce::Gtc | TimeInForce::Ioc) => Some(OrderKind::Market),
            (OrderType::Market, TimeInForce::Fok | TimeInForce::PostOnly) => None,
        }
    }

    /// The `orderType` this kind reports on the wire (the `Order` record + `OrderPlaced`).
    #[inline]
    pub fn order_type(self) -> OrderType {
        match self {
            OrderKind::Market => OrderType::Market,
            OrderKind::Limit(_) => OrderType::Limit,
        }
    }

    /// The `tif` this kind reports on the wire. A market order reports `Ioc`, because that is what
    /// it is — the placeholder `Gtc` a caller may have sent is not echoed back as a promise the
    /// engine does not keep.
    #[inline]
    pub fn tif(self) -> TimeInForce {
        match self {
            OrderKind::Market => TimeInForce::Ioc,
            OrderKind::Limit(t) => t,
        }
    }

    /// Does this kind honour a limit price while walking the book? Only a limit order does; a
    /// market order walks until the band or the book stops it.
    #[inline]
    pub fn is_limit(self) -> bool {
        matches!(self, OrderKind::Limit(_))
    }

    /// All-or-nothing: the match must be reverted unless it fills completely.
    #[inline]
    pub fn is_fok(self) -> bool {
        matches!(self, OrderKind::Limit(TimeInForce::Fok))
    }

    /// Does an unmatched remainder REST in the book? Only a GTC limit order rests: IOC, FOK and
    /// market remainders are discarded (`Expired`) and PostOnly never matched in the first place.
    /// This is the derivation that replaced `match_order`'s `rest_remainder` bool.
    #[inline]
    pub fn rests_remainder(self) -> bool {
        matches!(self, OrderKind::Limit(TimeInForce::Gtc))
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

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod order_kind_tests {
    use super::*;

    const ALL_TIFS: [TimeInForce; 4] = [
        TimeInForce::Gtc,
        TimeInForce::Ioc,
        TimeInForce::Fok,
        TimeInForce::PostOnly,
    ];

    /// The full 2 × 4 wire matrix, pinned. This is the legality table; anything that widens or
    /// narrows it changes this test.
    #[test]
    fn from_parts_pins_the_whole_legal_matrix() {
        // Every TIF is a product on a LIMIT order, and each maps to itself.
        for tif in ALL_TIFS {
            assert_eq!(
                OrderKind::from_parts(OrderType::Limit, tif),
                Some(OrderKind::Limit(tif)),
                "Limit + {tif:?} must be legal and preserve its TIF"
            );
        }
        // A MARKET order accepts only the unset placeholder (Gtc = discriminant 0) and the explicit
        // Ioc, and both collapse to the SAME value — no TIF survives into the model.
        assert_eq!(
            OrderKind::from_parts(OrderType::Market, TimeInForce::Gtc),
            Some(OrderKind::Market)
        );
        assert_eq!(
            OrderKind::from_parts(OrderType::Market, TimeInForce::Ioc),
            Some(OrderKind::Market)
        );
        // The two substantive rejects: neither product exists on any exchange.
        assert_eq!(
            OrderKind::from_parts(OrderType::Market, TimeInForce::Fok),
            None,
            "Market + FOK is not a product"
        );
        assert_eq!(
            OrderKind::from_parts(OrderType::Market, TimeInForce::PostOnly),
            None,
            "Market + PostOnly is not a product"
        );
    }

    /// **Anti-re-widening guard.** The `OrderKind::Market =>` arm below is a UNIT-variant pattern
    /// and the match has no `_` arm, so this test stops COMPILING if anyone gives `Market` a
    /// payload (`Market(TimeInForce)`) or adds a variant. That is the enforcement this whole change
    /// exists for: "a market order has a time-in-force" must be a compile error, not a runtime
    /// check someone can forget to call.
    #[test]
    fn market_carries_no_tif_and_the_type_forbids_one() {
        let mut kinds = vec![OrderKind::Market];
        kinds.extend(ALL_TIFS.map(OrderKind::Limit));
        for kind in kinds {
            match kind {
                OrderKind::Market => {
                    // Reported as IOC, because that is what a market order is.
                    assert_eq!(kind.order_type(), OrderType::Market);
                    assert_eq!(kind.tif(), TimeInForce::Ioc);
                    assert!(!kind.is_limit());
                    assert!(!kind.is_fok(), "a market order can never be all-or-nothing");
                    assert!(!kind.rests_remainder(), "a market order never rests");
                }
                OrderKind::Limit(tif) => {
                    assert_eq!(kind.order_type(), OrderType::Limit);
                    assert_eq!(kind.tif(), tif);
                    assert!(kind.is_limit());
                    assert_eq!(kind.is_fok(), tif == TimeInForce::Fok);
                    assert_eq!(kind.rests_remainder(), tif == TimeInForce::Gtc);
                }
            }
        }
    }

    /// `rests_remainder` is the derivation that replaced `match_order`'s `rest_remainder` bool:
    /// exactly ONE kind rests.
    #[test]
    fn exactly_one_kind_rests_its_remainder() {
        let resting: Vec<OrderKind> = core::iter::once(OrderKind::Market)
            .chain(ALL_TIFS.map(OrderKind::Limit))
            .filter(|k| k.rests_remainder())
            .collect();
        assert_eq!(resting, vec![OrderKind::Limit(TimeInForce::Gtc)]);
    }
}
