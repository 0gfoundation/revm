//! The account's CROSS LEGS (cross margin P4): per user, the state of every position CROSSED in
//! force — the summary the cross health check is priced from, stored under its own key
//! ([`crate::keys::cross_legs_key`]).
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{perp_err, PerpError};
use crate::types::{MarginType, PerpPosition, MAX_USER_MARKETS};

/// One entry of [`CrossLegs`]: a CROSSED, non-zero position's state — exactly the fields the cross
/// health terms are priced from, with the types [`PerpPosition`] gives them: `N = trunc(amount ·
/// mark)`, `uPnL = N + v_quote_balance`, `MM` at `|N|`, and the pending funding
/// `amount × (index − last_funding_index)`.
///
/// Stored only inside a [`CrossLegs`] blob, in its packed layout (see there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossLegState {
    /// The market.
    pub market_id: u64,
    /// [`PerpPosition::amount`] — never 0 on a stored entry.
    pub amount: i64,
    /// [`PerpPosition::v_quote_balance`].
    pub v_quote_balance: i64,
    /// [`PerpPosition::last_funding_index`].
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
/// `save_position_reservation_only` (an order resting, a cancel) takes only the per-side aggregates
/// and so cannot move any of the three fields: a placement and a cancel never touch this list; its
/// one sibling that may move one — `save_position_reservation_only_after_funding`, a match flush of
/// a user who published nothing after a ZERO-payment funding settle re-anchored
/// `last_funding_index` — mirrors it; the two settings routes (`setLeverage`, `setMarginType`)
/// assert the three fields unchanged, and `setMarginType` only switches a FLAT position, which has
/// no entry either way.
///
/// It is POSITION STATE, not a mark-dependent sum: nothing here moves when a mark or a funding
/// index moves, so it does not cache what R13 forbids caching (the health terms are re-priced at
/// every read — ADL's call-scoped memo, `crate::typed_store::CrossHealthMemo`, answers again only
/// while these legs, the wallet and every `Market` are unchanged). Checked against a fresh walk of
/// the positions on every `getAccount` and every published snapshot
/// (`perp_engine::margin_view::index_account_scalars`), and every `cross_health` result is
/// re-derived by the walk it replaces (`cross_fold`), both in `debug_assertions` builds.
///
/// # Why its own key, not a field of `UserAccount`
///
/// The account blob is written on every placement (the order nonce) and every fill, and every
/// written blob is re-encoded and hashed into the block commitment at the end of the block.
/// Embedded in the account (the first design, cross P4 D1/D2), the legs grew it from 21 to 126 /
/// 440 bytes at 4 / 16 cross positions and slowed a single-call block's `placeOrder` by 4.8% /
/// 8.4% — a path that never moves a leg. Here a placement writes no leg, and only a call that
/// moves a cross position (which writes that position anyway) writes this blob.
///
/// # The blob: packed, not positional msgpack
///
/// A cross fill rewrites this blob (it moves `amount` and `v_quote_balance`), and every written
/// blob is encoded and hashed into the block commitment at the end of the block — so its codec is
/// on every cross fill's path. Positional msgpack spent ~45 ns per entry in serde's per-field
/// machinery and wrote `last_funding_index` as a 16-byte `bin` (rmp's `i128`): 419 bytes and
/// ~0.7 µs to encode at 16 legs (P4 review, finding 5). The blob is instead ONE msgpack `bin`
/// whose payload is the entries back to back, each
///
/// ```text
/// uvarint(market_id) · zvarint(amount) · zvarint(v_quote_balance) · zvarint(last_funding_index)
/// ```
///
/// — `uvarint` LEB128 (7 bits per byte, low group first, the high bit set on every byte but the
/// last), `zvarint` the zigzag map (`0, −1, 1, −2, … → 0, 1, 2, 3, …`) then `uvarint`, minimal
/// length — in ascending market order. Canonical: one list, one byte string. Decoding refuses a
/// truncated varint (an incomplete last entry included), one wider than its field or not of
/// minimal length, and entries not strictly ascending by market (a `PerpError` — the storage
/// layer's, J4's one allowed kind); nothing else is checked there, the writer being
/// [`CrossLegs::set`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrossLegs(pub Vec<CrossLegState>);

/// The longest an entry packs to: `uvarint(u64)` and two `zvarint(i64)` at 10 bytes each,
/// `zvarint(i128)` at 19.
const MAX_PACKED_LEG: usize = 10 + 10 + 10 + 19;

#[inline]
fn put_uvarint(out: &mut [u8], at: &mut usize, mut v: u128) {
    while v >= 0x80 {
        out[*at] = (v as u8) | 0x80;
        *at += 1;
        v >>= 7;
    }
    out[*at] = v as u8;
    *at += 1;
}

#[inline]
fn zigzag(v: i128) -> u128 {
    ((v << 1) ^ (v >> 127)) as u128
}

#[inline]
fn unzigzag(v: u128) -> i128 {
    ((v >> 1) as i128) ^ -((v & 1) as i128)
}

/// Reads a `uvarint` of at most `max_bits` significant bits from `buf[*at..]`.
#[inline]
fn get_uvarint(buf: &[u8], at: &mut usize, max_bits: u32) -> Result<u128, PerpError> {
    let mut v: u128 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *buf.get(*at).ok_or_else(|| perp_err("corrupt cross-legs blob (truncated)"))?;
        *at += 1;
        let bits = (byte & 0x7f) as u128;
        // The group must fit what is left of `max_bits`.
        if shift >= max_bits || (shift > 0 && bits >> (max_bits - shift).min(127) != 0) {
            return Err(perp_err("corrupt cross-legs blob (varint overflow)"));
        }
        v |= bits << shift;
        if byte & 0x80 == 0 {
            // A last group of 0 after another one is a non-minimal encoding of a shorter value.
            if byte == 0 && shift > 0 {
                return Err(perp_err("corrupt cross-legs blob (non-minimal varint)"));
            }
            return Ok(v);
        }
        shift += 7;
    }
}

impl CrossLegs {
    /// Packs the entries ([`CrossLegs`]' layout) into `out`, returning the length written. `out`
    /// holds at least `MAX_PACKED_LEG` bytes per entry.
    fn pack_into(&self, out: &mut [u8]) -> usize {
        let mut at = 0;
        for l in &self.0 {
            put_uvarint(out, &mut at, l.market_id as u128);
            put_uvarint(out, &mut at, zigzag(l.amount as i128));
            put_uvarint(out, &mut at, zigzag(l.v_quote_balance as i128));
            put_uvarint(out, &mut at, zigzag(l.last_funding_index));
        }
        at
    }

    /// Inverse of [`Self::pack_into`].
    fn unpack(buf: &[u8]) -> Result<Self, PerpError> {
        let mut legs = Vec::with_capacity((buf.len() / 4).min(MAX_USER_MARKETS));
        let mut at = 0;
        while at < buf.len() {
            let market_id = get_uvarint(buf, &mut at, 64)? as u64;
            let amount = unzigzag(get_uvarint(buf, &mut at, 64)?) as i64;
            let v_quote_balance = unzigzag(get_uvarint(buf, &mut at, 64)?) as i64;
            let last_funding_index = unzigzag(get_uvarint(buf, &mut at, 128)?);
            if legs.last().is_some_and(|l: &CrossLegState| l.market_id >= market_id) {
                return Err(perp_err("corrupt cross-legs blob (markets not ascending)"));
            }
            legs.push(CrossLegState { market_id, amount, v_quote_balance, last_funding_index });
        }
        Ok(Self(legs))
    }
}

impl Serialize for CrossLegs {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // On the stack at the cap (16 entries, 784 bytes); a longer list — which `set` asserts
        // cannot exist — on the heap.
        let mut stack = [0u8; MAX_USER_MARKETS * MAX_PACKED_LEG];
        let mut heap = Vec::new();
        let out: &mut [u8] = if self.0.len() <= MAX_USER_MARKETS {
            &mut stack
        } else {
            heap.resize(self.0.len() * MAX_PACKED_LEG, 0);
            &mut heap
        };
        let n = self.pack_into(out);
        serializer.serialize_bytes(&out[..n])
    }
}

impl<'de> Deserialize<'de> for CrossLegs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Packed;
        impl serde::de::Visitor<'_> for Packed {
            type Value = CrossLegs;
            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("a packed cross-legs byte string")
            }
            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<CrossLegs, E> {
                CrossLegs::unpack(v).map_err(|e| E::custom(format!("{e:?}")))
            }
        }
        deserializer.deserialize_bytes(Packed)
    }
}

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

    /// The blob is ONE msgpack `bin` of the packed entries: the exact bytes of a one-entry list
    /// (`uvarint` market, then the zigzag `uvarint`s of amount, vQuote and funding index), and a
    /// full list at the cap — every field at its extremes — survives the codec.
    #[test]
    fn the_blob_is_one_bin_of_packed_entries() {
        let one = CrossLegs(vec![CrossLegState {
            market_id: 3,
            amount: 1,
            v_quote_balance: -2,
            last_funding_index: 0,
        }]);
        let one_bytes = encode(&one).unwrap();
        assert_eq!(
            one_bytes,
            [0xc4, 0x04, 0x03, 0x02, 0x03, 0x00],
            "bin8 · 3 · z(1) · z(−2) · z(0)"
        );
        assert_eq!(decode::<CrossLegs>(&one_bytes).unwrap(), one);
        let two = CrossLegs(vec![CrossLegState {
            market_id: 300,
            amount: -64,
            v_quote_balance: 64,
            last_funding_index: -1,
        }]);
        assert_eq!(
            encode(&two).unwrap(),
            [0xc4, 0x06, 0xac, 0x02, 0x7f, 0x80, 0x01, 0x01],
            "300 = 0xac 0x02; z(−64) = 127; z(64) = 128 = 0x80 0x01; z(−1) = 1"
        );
        let mut l = CrossLegs::default();
        let extremes = [
            (i64::MIN, i64::MAX, i128::MIN),
            (i64::MAX, i64::MIN, i128::MAX),
            (-1, 0, -(1 << 70)),
            (1, -1, 1 << 100),
        ];
        for k in (1..=MAX_USER_MARKETS as u64).rev() {
            let (a, v, fi) = extremes[k as usize % 4];
            let market_id = if k == MAX_USER_MARKETS as u64 { u64::MAX } else { k };
            l.set(market_id, Some(CrossLegState {
                market_id,
                amount: a,
                v_quote_balance: v,
                last_funding_index: fi,
            }));
        }
        let bytes = encode(&l).unwrap();
        assert_eq!(bytes[0], 0xc5, "bin16: past 255 bytes at the extremes");
        assert_eq!(decode::<CrossLegs>(&bytes).unwrap(), l);
    }

    /// The decoder refuses every byte string the encoder cannot produce that would otherwise
    /// decode: a truncated entry, a varint wider than its field or not minimal, markets out of
    /// order — and a msgpack array (the earlier layout).
    #[test]
    fn the_decoder_refuses_what_the_encoder_cannot_write() {
        let bin = |payload: &[u8]| {
            let mut b = vec![0xc4, payload.len() as u8];
            b.extend_from_slice(payload);
            b
        };
        assert!(decode::<CrossLegs>(&bin(&[0x03, 0x02, 0x03, 0x00])).is_ok());
        for (payload, why) in [
            (&[0x03, 0x02, 0x03][..], "a missing field"),
            (&[0x03, 0x02, 0x03, 0x80][..], "a varint cut mid-way"),
            (&[0x03, 0x02, 0x03, 0x00, 0x04][..], "a trailing partial entry"),
            (&[0x83, 0x00, 0x02, 0x03, 0x00][..], "a non-minimal market id"),
            (
                &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02, 0x02, 0x03, 0x00][..],
                "a market id past 64 bits",
            ),
            (&[0x05, 0x02, 0x03, 0x00, 0x05, 0x02, 0x03, 0x00][..], "a market twice"),
            (&[0x05, 0x02, 0x03, 0x00, 0x04, 0x02, 0x03, 0x00][..], "markets descending"),
        ] {
            assert!(decode::<CrossLegs>(&bin(payload)).is_err(), "{why}");
        }
        let fi_past_128 = [&[0x03, 0x02, 0x03][..], &[0xff; 18], &[0x04]].concat();
        assert!(decode::<CrossLegs>(&bin(&fi_past_128)).is_err(), "a funding index past 128 bits");
        let fi_at_128 = [&[0x03, 0x02, 0x03][..], &[0xff; 18], &[0x03]].concat();
        assert_eq!(
            decode::<CrossLegs>(&bin(&fi_at_128)).unwrap().0[0].last_funding_index,
            i128::MIN,
            "z(i128::MIN) = u128::MAX: eighteen full groups and a last of 0b11"
        );
        assert!(decode::<CrossLegs>(&[0x91, 0x94, 0x03, 0x01, 0xfe, 0x00]).is_err(), "an array");
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
