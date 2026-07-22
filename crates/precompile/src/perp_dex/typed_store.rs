//! Stage A of the live-struct + dirty-set co-design
//! (see `docs/perpstate-livestruct-codesign-shadow-plan.md`).
//!
//! A strongly-typed, per-execution off-trie store — the eventual replacement for the type-erased
//! `dyn Any` overlay that today lives in `revm-context`'s `PerpSection`. Reads/writes hit a typed
//! sub-map directly (no per-blob `downcast`, no 4-probe overlay dance), and block-end
//! [`TypedPerpStore::take_delta`] re-derives the canonical `(B256 key, bytes)` pairs from a dirty
//! set using the SAME [`encode`](crate::perp_dex::storage::encode) the current `save_*` path uses —
//! so the pairs are byte-identical to the overlay drain and the block commitment
//! (`compute_block_commitment`) is unchanged.
//!
//! ## Staging
//!
//! Stage A.1 (this file) is **not yet wired into the journal** (that is Stage A.2). It carries the
//! three representative key-shape patterns — per-user (`accounts`), per-market (`markets`), and
//! per-(user, market) (`positions`) — with round-trip + byte-identity unit tests. The remaining
//! off-trie namespaces (orders, price levels, index/funding state, roles, insurance fund, replay
//! guards, registries) extend the exact same pattern in Stage B, ahead of the storage-layer cutover.
//!
//! The design invariant that makes the cutover golden-neutral: a mutable/set/remove touch marks the
//! entity's canonical key dirty (conservative — any `&mut` is a potential write, matching the
//! current `get_struct_mut` write-count semantics), and `take_delta` reads each dirty entity's FINAL
//! value straight from its sub-map (absent = removed = empty bytes = the delete convention).

use primitives::{Address, HashMap, B256};
use std::vec::Vec;

use crate::perp_dex::storage::{encode, keys};
use crate::perp_dex::types::{Market, PerpPosition, UserAccount};
use crate::PrecompileError;

/// Identifies which typed sub-map + identity a dirty `B256` key refers to, so [`TypedPerpStore::take_delta`]
/// can read the entity's current value back out (or detect its removal → tombstone) without
/// re-parsing the packed key. One variant per off-trie namespace (Stage A.1 = the three
/// representative ones; Stage B adds the rest).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreSlot {
    Account(Address),
    Market(u64),
    Position(Address, u64),
}

/// Strongly-typed off-trie store (Stage A). See the module docs.
#[derive(Debug, Default)]
pub struct TypedPerpStore {
    accounts: HashMap<Address, UserAccount>,
    markets: HashMap<u64, Market>,
    positions: HashMap<(Address, u64), PerpPosition>,
    // Stage B extends with the remaining namespaces, same patterns:
    //   orders / bid_levels / ask_levels / bid_prices / ask_prices / market_hot /
    //   market_fee_total / trade_count / position_registry / api_keys / api_key_ids /
    //   index_price / index_history / basis_window / funding_state / premium_accumulator /
    //   roles (admin/oracle/market_manager) / insurance_fund / seen_sig / seen_bucket
    /// Keys written this block → the sub-map slot to read their final value from at block end.
    /// Deduplicated by `B256` (a key written N times keeps one slot; the value read at block end is
    /// the last-written one). Mirrors the phase-1 `dirty_keys` list, but carries the typed slot so
    /// `take_delta` needs no packed-key parsing. Drained by [`TypedPerpStore::take_delta`].
    dirty: HashMap<B256, StoreSlot>,
}

impl TypedPerpStore {
    // ── per-user account ───────────────────────────────────────────────────────
    /// Shared read of a user's account (`None` = not resident this block).
    pub fn account(&self, user: Address) -> Option<&UserAccount> {
        self.accounts.get(&user)
    }

    /// `&mut` access marks the key dirty (conservative: any mutable borrow is a potential write,
    /// matching the current `get_struct_mut` write-count semantics).
    pub fn account_mut(&mut self, user: Address) -> Option<&mut UserAccount> {
        if self.accounts.contains_key(&user) {
            self.dirty
                .insert(keys::account_key(user), StoreSlot::Account(user));
        }
        self.accounts.get_mut(&user)
    }

    /// Inserts/overwrites a user's account and marks its key dirty.
    pub fn set_account(&mut self, user: Address, value: UserAccount) {
        self.dirty
            .insert(keys::account_key(user), StoreSlot::Account(user));
        self.accounts.insert(user, value);
    }

    /// Removes a user's account (block-end delta emits an empty-bytes tombstone).
    pub fn remove_account(&mut self, user: Address) {
        self.dirty
            .insert(keys::account_key(user), StoreSlot::Account(user));
        self.accounts.remove(&user);
    }

    // ── per-market config ────────────────────────────────────────────────────
    /// Shared read of a market's config (`None` = not resident this block).
    pub fn market(&self, market_id: u64) -> Option<&Market> {
        self.markets.get(&market_id)
    }

    /// Mutable market access; marks its key dirty (see [`Self::account_mut`]).
    pub fn market_mut(&mut self, market_id: u64) -> Option<&mut Market> {
        if self.markets.contains_key(&market_id) {
            self.dirty
                .insert(keys::market_key(market_id), StoreSlot::Market(market_id));
        }
        self.markets.get_mut(&market_id)
    }

    /// Inserts/overwrites a market and marks its key dirty.
    pub fn set_market(&mut self, market_id: u64, value: Market) {
        self.dirty
            .insert(keys::market_key(market_id), StoreSlot::Market(market_id));
        self.markets.insert(market_id, value);
    }

    // ── per-(user, market) position ──────────────────────────────────────────
    /// Shared read of a user's position in a market (`None` = not resident this block).
    pub fn position(&self, user: Address, market_id: u64) -> Option<&PerpPosition> {
        self.positions.get(&(user, market_id))
    }

    /// Mutable position access; marks its key dirty (see [`Self::account_mut`]).
    pub fn position_mut(&mut self, user: Address, market_id: u64) -> Option<&mut PerpPosition> {
        if self.positions.contains_key(&(user, market_id)) {
            self.dirty.insert(
                keys::position_key(user, market_id),
                StoreSlot::Position(user, market_id),
            );
        }
        self.positions.get_mut(&(user, market_id))
    }

    /// Inserts/overwrites a position and marks its key dirty.
    pub fn set_position(&mut self, user: Address, market_id: u64, value: PerpPosition) {
        self.dirty.insert(
            keys::position_key(user, market_id),
            StoreSlot::Position(user, market_id),
        );
        self.positions.insert((user, market_id), value);
    }

    /// Removes a position (block-end delta emits an empty-bytes tombstone).
    pub fn remove_position(&mut self, user: Address, market_id: u64) {
        self.dirty.insert(
            keys::position_key(user, market_id),
            StoreSlot::Position(user, market_id),
        );
        self.positions.remove(&(user, market_id));
    }

    /// Number of keys written this block (dirty-set size). Diagnostic / test hook.
    pub fn dirty_len(&self) -> usize {
        self.dirty.len()
    }

    /// Drains the block's dirty set into canonical `(key, bytes)` pairs — the input
    /// `compute_block_commitment` folds and the persistence layer writes. An entity that is absent
    /// from its sub-map (removed this block) yields EMPTY bytes (the delete convention). Bytes come
    /// from the SAME [`encode`](crate::perp_dex::storage::encode) as `save_*`, so the stream is
    /// byte-identical to the current overlay drain. Returned sorted by key (deterministic; the
    /// commitment sorts regardless).
    pub fn take_delta(&mut self) -> Result<Vec<(B256, Vec<u8>)>, PrecompileError> {
        let mut out: Vec<(B256, Vec<u8>)> = Vec::with_capacity(self.dirty.len());
        // Disjoint field borrows: draining `self.dirty` while reading the typed sub-maps.
        for (key, slot) in self.dirty.drain() {
            let bytes = match slot {
                StoreSlot::Account(u) => match self.accounts.get(&u) {
                    Some(v) => encode(v)?,
                    None => Vec::new(),
                },
                StoreSlot::Market(m) => match self.markets.get(&m) {
                    Some(v) => encode(v)?,
                    None => Vec::new(),
                },
                StoreSlot::Position(u, m) => match self.positions.get(&(u, m)) {
                    Some(v) => encode(v)?,
                    None => Vec::new(),
                },
            };
            out.push((key, bytes));
        }
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perp_dex::storage::decode;

    fn sample_market(id: u64) -> Market {
        Market {
            market_id: id,
            base_decimals: 8,
            price_decimals: 2,
            tick_size: 1,
            step_size: 1,
            min_quantity: 1,
            max_quantity: 1_000_000_000,
            max_price: 1_000_000_000,
            price_update_interval: 5,
            active: true,
            funding_interval: 28_800,
            interest_rate: 100,
            liquidation_fee_rate_bps: 50,
            price_band_bps: 500,
            mark_price: 65_000_00,
        }
    }

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }

    #[test]
    fn typed_reads_and_writes_round_trip() {
        let mut store = TypedPerpStore::default();
        let u = addr(0xAB);

        assert!(store.account(u).is_none());
        store.set_account(u, UserAccount::default());
        {
            let a = store.account_mut(u).expect("just set");
            a.nonce = 7;
            a.perp_wallet_balance = 123;
        }
        assert_eq!(store.account(u).unwrap().nonce, 7);

        store.set_market(1, sample_market(1));
        assert_eq!(store.market(1).unwrap().mark_price, 65_000_00);

        let mut pos = PerpPosition::default();
        pos.amount = -42;
        pos.leverage = 6;
        store.set_position(u, 1, pos.clone());
        assert_eq!(store.position(u, 1).unwrap().amount, -42);
    }

    #[test]
    fn take_delta_is_byte_identical_to_encode_and_keys() {
        let mut store = TypedPerpStore::default();
        let u = addr(0x11);

        let mut acct = UserAccount::default();
        acct.nonce = 3;
        store.set_account(u, acct.clone());

        let mkt = sample_market(2);
        store.set_market(2, mkt.clone());

        let mut pos = PerpPosition::default();
        pos.amount = 1000;
        store.set_position(u, 2, pos.clone());

        let delta = store.take_delta().unwrap();
        assert_eq!(delta.len(), 3, "three distinct keys written");
        assert_eq!(store.dirty_len(), 0, "dirty set drained");

        // Each (key, bytes) pair must match the canonical key derivation + `encode` exactly — this
        // is what makes the Stage-B cutover golden-neutral.
        let expect = |k: B256, bytes: Vec<u8>| {
            delta
                .iter()
                .find(|(dk, _)| *dk == k)
                .map(|(_, db)| assert_eq!(*db, bytes, "bytes mismatch for a key"))
                .expect("key present in delta");
        };
        expect(keys::account_key(u), encode(&acct).unwrap());
        expect(keys::market_key(2), encode(&mkt).unwrap());
        expect(keys::position_key(u, 2), encode(&pos).unwrap());

        // Round-trip: the canonical bytes decode back to the stored value.
        let acct_bytes = &delta
            .iter()
            .find(|(k, _)| *k == keys::account_key(u))
            .unwrap()
            .1;
        assert_eq!(decode::<UserAccount>(acct_bytes).unwrap(), acct);
    }

    #[test]
    fn removed_entity_emits_empty_bytes_tombstone() {
        let mut store = TypedPerpStore::default();
        let u = addr(0x22);

        store.set_position(u, 9, PerpPosition::default());
        // drain the set write so the next block starts clean
        let _ = store.take_delta().unwrap();

        // now remove it this block
        store.set_position(u, 9, PerpPosition::default());
        store.remove_position(u, 9);
        let delta = store.take_delta().unwrap();
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].0, keys::position_key(u, 9));
        assert!(
            delta[0].1.is_empty(),
            "removed entity → empty bytes (delete convention)"
        );
    }

    #[test]
    fn repeated_writes_coalesce_to_final_value() {
        let mut store = TypedPerpStore::default();
        let u = addr(0x33);

        let mut a = UserAccount::default();
        a.nonce = 1;
        store.set_account(u, a);
        let mut a2 = UserAccount::default();
        a2.nonce = 5;
        store.set_account(u, a2.clone());

        let delta = store.take_delta().unwrap();
        assert_eq!(delta.len(), 1, "same key written twice → one delta entry");
        assert_eq!(delta[0].1, encode(&a2).unwrap(), "last write wins");
    }
}
