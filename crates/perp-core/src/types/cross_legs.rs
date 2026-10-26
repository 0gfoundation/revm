//! The account's CROSS LEGS (cross margin P4): per user, the state of every position CROSSED in
//! force — the summary the cross health check is priced from, stored under its own key
//! ([`crate::keys::cross_legs_key`]).
use serde::{Deserialize, Serialize};

use crate::types::{MarginType, PerpPosition, MAX_USER_MARKETS};

/// One entry of [`CrossLegs`]: a CROSSED, non-zero position's state — exactly the fields the cross
/// health terms are priced from, with the types [`PerpPosition`] gives them: `N = trunc(amount ·
/// mark)`, `uPnL = N + v_quote_balance`, `MM` at `|N|`, and the pending funding
/// `amount × (index − last_funding_index)`.
///
/// Positional msgpack like every blob type: field ORDER is layout-significant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossLegState {
    /// The market.
    #[serde(rename = "k")]
    pub market_id: u64,
    /// [`PerpPosition::amount`] — never 0 on a stored entry.
    #[serde(rename = "a")]
    pub amount: i64,
    /// [`PerpPosition::v_quote_balance`].
    #[serde(rename = "v")]
    pub v_quote_balance: i64,
    /// [`PerpPosition::last_funding_index`].
    #[serde(rename = "fi")]
    pub last_funding_index: i128,
}

impl CrossLegState {
    /// The entry the stored position `pos` of `(user, market_id)` contributes to the user's
    /// [`CrossLegs`]: `Some` iff it is CROSSED IN FORCE — [`MarginType::effective`] of its stored
    /// byte under `cross_enabled`, the gate (`perp_engine::margin_type` passes
    /// `cross_margin_enabled()`) — and its `amount` is non-zero: the invariant documented on
    /// [`CrossLegs`]. Total.
    #[inline]
    pub fn of(market_id: u64, pos: &PerpPosition, cross_enabled: bool) -> Option<Self> {
        let crossed = MarginType::effective(pos.margin_type, cross_enabled) == MarginType::Crossed;
        (crossed && pos.amount != 0).then_some(Self {
            market_id,
            amount: pos.amount,
            v_quote_balance: pos.v_quote_balance,
            last_funding_index: pos.last_funding_index,
        })
    }
}

/// **A user's cross legs** — one [`CrossLegState`] per market whose position is CROSSED in force
/// and non-zero, so that judging the account (`E = cWB + Σ_C uPnL` against `Σ_C MM`) costs the
/// account blob, this blob and the legs' `Market` blobs — no per-user market index and no position
/// loads (`perp_engine::margin_view::cross_health`, cross P4). Stored under its own per-user key,
/// [`crate::keys::cross_legs_key`]; an EMPTY list is not stored (the key is deleted, the per-user
/// market index's convention), so an account that holds no cross position has no such key.
///
/// # The invariant
///
/// For every market `k`, the list holds an entry for `k` **iff** the stored position of
/// `(user, k)` is CROSSED IN FORCE and has `amount != 0` ([`CrossLegState::of`]), and that entry's
/// `amount`, `v_quote_balance` and `last_funding_index` EQUAL the stored position's. Sorted by
/// `market_id` ascending, at most one entry per market — so at most [`MAX_USER_MARKETS`] entries,
/// because a non-zero position keeps its market in the per-user index.
///
/// "In force" is the stored margin type under the gate (`CROSS_MARGIN_ENABLED`), a `const` in every
/// non-test build — `true` since the P3 flip — so it never moves under a stored position. Only a
/// TEST can flip it (`perp_engine::margin_type::test_gate`): entries written on one side of such a
/// flip are not re-keyed on the other, and the reader applies the gate it runs under (gated off,
/// nothing is CROSSED in force, whatever the list holds).
///
/// # Owned by `storage::save_position`, and by nothing else
///
/// Every write of `amount` / `v_quote_balance` / `last_funding_index` comes through
/// `perp_engine::storage::save_position` — fills on either side, liquidation closes, both ADL legs,
/// funding settlements, closePosition and every fired conditional order, the sub-account and batch
/// routes, and every test fixture — which sets the entry from the position it stores whenever the
/// entry it implies moved. The narrower routes beside it keep the invariant too:
/// `save_position_reservation_only` (an order resting, a cancel) moves none of the three fields
/// (asserted), so a placement and a cancel never touch this list; its one sibling that may move
/// one — `save_position_reservation_only_after_funding`, a match flush of a user who published
/// nothing after a ZERO-payment funding settle re-anchored `last_funding_index` — mirrors it; the
/// two settings routes (`setLeverage`, `setMarginType`) assert the three fields unchanged, and
/// `setMarginType` only switches a FLAT position, which has no entry either way.
///
/// It is POSITION STATE, not a mark-dependent sum: nothing here moves when a mark or a funding index
/// moves, so it does not cache what R13 forbids caching (the health terms are re-priced at every
/// read). Checked against a fresh walk of the positions on every `getAccount` and every published
/// snapshot (`perp_engine::margin_view::index_account_scalars`), and every `cross_health` result is
/// re-derived by the walk it replaces (`cross_fold`), both in `debug_assertions` builds.
///
/// # Why its own key, not a field of `UserAccount`
///
/// The account blob is written on every placement (the order nonce) and every fill, and every
/// written blob is re-encoded and hashed into the block commitment at the end of the block. Embedded
/// in the account (the first design, cross P4 D1/D2), the legs grew it from 21 to 126 / 440 bytes
/// at 4 / 16 cross positions and slowed a single-call block's `placeOrder` by 4.8% / 8.4% — a path
/// that never moves a leg. Here a placement writes no leg, and only a call that moves a cross
/// position (which writes that position anyway) writes this blob.
///
/// `#[serde(transparent)]`: the blob is the bare msgpack array of entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CrossLegs(pub Vec<CrossLegState>);

impl CrossLegs {
    /// The entry of `market_id`, if any. A binary search (the list is sorted).
    #[inline]
    pub fn get(&self, market_id: u64) -> Option<&CrossLegState> {
        self.0.binary_search_by_key(&market_id, |l| l.market_id).ok().map(|i| &self.0[i])
    }

    /// Sets (`Some`) or removes (`None`) the entry of `market_id`, keeping the list sorted with at
    /// most one entry per market. Total — it is reached from `storage::save_position`, which the
    /// liquidation sweep reaches (and the sweep panics on any `Err`). The list cannot outgrow
    /// [`MAX_USER_MARKETS`] — every entry is a non-zero position whose market the per-user index
    /// holds — and a `debug_assert` says so.
    pub fn set(&mut self, market_id: u64, leg: Option<CrossLegState>) {
        debug_assert!(
            leg.is_none_or(|l| l.market_id == market_id && l.amount != 0),
            "CrossLegs::set: entry {leg:?} for market {market_id}"
        );
        match (self.0.binary_search_by_key(&market_id, |l| l.market_id), leg) {
            (Ok(i), Some(l)) => self.0[i] = l,
            (Ok(i), None) => {
                self.0.remove(i);
            }
            (Err(i), Some(l)) => {
                self.0.insert(i, l);
                debug_assert!(
                    self.0.len() <= MAX_USER_MARKETS,
                    "CrossLegs::set: {} cross legs, past MAX_USER_MARKETS",
                    self.0.len()
                );
            }
            (Err(_), None) => {}
        }
    }

    /// The entries, in market order.
    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'_, CrossLegState> {
        self.0.iter()
    }

    /// How many cross legs.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// No cross leg — the account holds no position CROSSED in force.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode};

    fn leg(k: u64, a: i64) -> CrossLegState {
        CrossLegState {
            market_id: k,
            amount: a,
            v_quote_balance: -a * 100,
            last_funding_index: k as i128,
        }
    }

    /// `set` keeps the list sorted by market with at most one entry each: an insert in any order
    /// lands in place, a second write replaces, `None` removes, and removing an absent entry is a
    /// no-op. `get` finds exactly what was set.
    #[test]
    fn set_keeps_one_sorted_entry_per_market() {
        let mut l = CrossLegs::default();
        for k in [5, 1, 9, 3] {
            l.set(k, Some(leg(k, 1)));
        }
        let ids = |l: &CrossLegs| l.iter().map(|l| l.market_id).collect::<Vec<_>>();
        assert_eq!(ids(&l), vec![1, 3, 5, 9]);
        l.set(3, Some(leg(3, -7)));
        assert_eq!(ids(&l), vec![1, 3, 5, 9], "a second write replaces, never duplicates");
        assert_eq!(l.get(3), Some(&leg(3, -7)));
        l.set(5, None);
        l.set(4, None);
        assert_eq!(ids(&l), vec![1, 3, 9]);
        assert_eq!((l.get(5), l.get(9)), (None, Some(&leg(9, 1))));
        assert_eq!(l.len(), 3);
        for k in [1, 3, 9] {
            l.set(k, None);
        }
        assert!(l.is_empty());
    }

    /// The blob is the bare array of entries (`transparent`), each a 4-element array in field
    /// order; a full list at the cap survives the codec.
    #[test]
    fn the_blob_is_a_bare_array_of_positional_entries() {
        let mut l = CrossLegs::default();
        for k in (1..=MAX_USER_MARKETS as u64).rev() {
            let e = CrossLegState {
                market_id: k,
                amount: -(k as i64),
                v_quote_balance: 1_000_000 * k as i64,
                last_funding_index: -(k as i128) << 70,
            };
            l.set(k, Some(e));
        }
        let bytes = encode(&l).unwrap();
        assert_eq!(bytes[0], 0xdc, "array16 header: 16 entries");
        assert_eq!(decode::<CrossLegs>(&bytes).unwrap(), l);
        let one = CrossLegs(vec![CrossLegState {
            market_id: 3,
            amount: 1,
            v_quote_balance: -2,
            last_funding_index: 0,
        }]);
        let one_bytes = encode(&one).unwrap();
        assert_eq!(one_bytes[..5], [0x91, 0x94, 0x03, 0x01, 0xfe], "[[k, a, v, fi]]");
        assert_eq!(decode::<CrossLegs>(&one_bytes).unwrap(), one);
    }

    /// `CrossLegState::of` is the invariant's predicate: a non-zero position CROSSED in force
    /// contributes its three state fields; a flat one, an ISOLATED one, or a CROSSED byte with the
    /// gate off (ISOLATED in force) contributes nothing — whatever else they hold.
    #[test]
    fn a_leg_exists_exactly_for_a_non_zero_position_crossed_in_force() {
        let pos = PerpPosition {
            amount: -4,
            v_quote_balance: 400,
            margin: 0,
            last_funding_index: -12,
            ..PerpPosition::default()
        };
        assert_eq!(pos.margin_type, MarginType::Crossed.code(), "Binance's default");
        let want = CrossLegState {
            market_id: 7,
            amount: -4,
            v_quote_balance: 400,
            last_funding_index: -12,
        };
        assert_eq!(CrossLegState::of(7, &pos, true), Some(want));
        assert_eq!(CrossLegState::of(7, &PerpPosition { amount: 0, ..pos.clone() }, true), None);
        assert_eq!(CrossLegState::of(7, &pos, false), None, "gated off: ISOLATED in force");
        let isolated = PerpPosition { margin_type: MarginType::Isolated.code(), ..pos };
        assert_eq!(CrossLegState::of(7, &isolated, true), None);
    }
}
