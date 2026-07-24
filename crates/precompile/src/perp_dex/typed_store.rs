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

use context::journaled_state::{PerpBlob, PerpDelta, PerpDeltaEntry, PerpStore};
use primitives::{Address, HashMap, B256};
use std::sync::Arc;
use std::vec::Vec;

use crate::perp_dex::storage::{encode, keys, pack_level, LevelBlob};
use crate::perp_dex::types::{
    Market, MarketHot, Order, OrderEntry, PerpPosition, PublicAccountBalance, UserAccount,
};
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
    MarketHot(u64),
    BuyOrders(Address, u64),
    SellOrders(Address, u64),
    Order([u8; 32]),
    BidPrices(u64),
    AskPrices(u64),
    BidLevel(u64, u64),
    AskLevel(u64, u64),
}

/// Three-state read result from a typed sub-map.
///
/// The distinction between `Deleted` and `Miss` is a CORRECTNESS requirement, not an optimization:
/// a key removed THIS block must read as definitively-absent (`Deleted`), never fall through to the
/// committed cross-block store — which still holds the pre-delete value until the block-end delta
/// lands. (The overlay encodes the same thing today as a resident `Bytes(empty)` entry.) `Miss`
/// alone sends the reader to the cold path.
#[derive(Debug, PartialEq, Eq)]
pub enum Resident<'a, T> {
    /// Not resident this block — the caller falls through to the cold/cross-block path.
    Miss,
    /// Removed this block — definitively absent; do NOT fall through.
    Deleted,
    /// Resident value.
    Hit(&'a T),
}

impl<'a, T> Resident<'a, T> {
    /// `Hit` payload as an `Option` (test/diagnostic convenience; production readers must match
    /// all three states — collapsing `Deleted` into `None` is exactly the fall-through bug).
    pub fn hit(self) -> Option<&'a T> {
        match self {
            Resident::Hit(v) => Some(v),
            _ => None,
        }
    }
}

/// Sub-map entry: `None` = removed THIS block (a resident tombstone — reads report
/// [`Resident::Deleted`] instead of falling through to the stale committed store; the block delta
/// emits empty bytes). `Some` = live value.
type Slot<T> = Option<Arc<T>>;

/// Strongly-typed off-trie store (Stage A). See the module docs.
///
/// Sub-maps hold `Arc<T>`, not owned `T` — this is what keeps every current perf property when the
/// cold path is wired (Stage B): a cross-block fill from the committed decoded store
/// (`Database::perp_load_arc` hands an `Arc`) stores the SAME Arc (zero clone); `load_*_ref`
/// zero-clone reads stay a refcount bump; and mutation goes through [`std::sync::Arc::make_mut`] —
/// clone IF shared (exactly the first-touch CoW lift), in-place thereafter. `take_delta` hands the
/// Arc back to the cross-block store without cloning.
#[derive(Clone, Debug, Default)]
pub struct TypedPerpStore {
    accounts: HashMap<Address, Slot<UserAccount>>,
    markets: HashMap<u64, Slot<Market>>,
    positions: HashMap<(Address, u64), Slot<PerpPosition>>,
    market_hots: HashMap<u64, Slot<MarketHot>>,
    buy_orders: HashMap<(Address, u64), Slot<Vec<OrderEntry>>>,
    sell_orders: HashMap<(Address, u64), Slot<Vec<OrderEntry>>>,
    orders: HashMap<[u8; 32], Slot<Order>>,
    bid_prices: HashMap<u64, Slot<Vec<u64>>>,
    ask_prices: HashMap<u64, Slot<Vec<u64>>>,
    bid_levels: HashMap<(u64, u64), Slot<LevelBlob>>,
    ask_levels: HashMap<(u64, u64), Slot<LevelBlob>>,
    // Stage B extends with the remaining namespaces, same patterns:
    //   bid_levels / ask_levels / bid_prices / ask_prices /
    //   market_fee_total / trade_count / position_registry / api_keys / api_key_ids /
    //   index_price / index_history / basis_window / funding_state / premium_accumulator /
    //   roles (admin/oracle/market_manager) / insurance_fund / seen_sig / seen_bucket
    /// Keys written this block → the sub-map slot to read their final value from at block end.
    /// Deduplicated by `B256` (a key written N times keeps one slot; the value read at block end is
    /// the last-written one). Mirrors the phase-1 `dirty_keys` list, but carries the typed slot so
    /// `take_delta` needs no packed-key parsing. Drained by [`TypedPerpStore::take_delta`].
    dirty: HashMap<B256, StoreSlot>,
    /// Monotonic write counter — the typed-store half of the commit-only #23 tripwire (summed with
    /// the overlay's counter by the journal). Cutover namespaces would otherwise be invisible to
    /// the write-then-revert detector.
    write_count: u64,
    /// Whether the CURRENT transaction wrote this store (reset at tx boundaries by the journal) —
    /// the typed-store half of the `discard_tx` corruption guard.
    tx_dirty: bool,
    /// Initial public balances for accounts touched by the current top-level
    /// precompile call. `None` keeps direct storage tests and internal helpers
    /// outside the ABI entry point free of event bookkeeping.
    balance_tracking: Option<HashMap<Address, PublicAccountBalance>>,
}

impl TypedPerpStore {
    pub fn begin_balance_tracking(&mut self) {
        self.balance_tracking = Some(HashMap::new());
    }

    pub fn track_initial_balance(&mut self, user: Address, balance: PublicAccountBalance) {
        if let Some(initial) = self.balance_tracking.as_mut() {
            initial.entry(user).or_insert(balance);
        }
    }

    pub fn take_balance_tracking(&mut self) -> Vec<(Address, PublicAccountBalance)> {
        self.balance_tracking
            .take()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    pub fn discard_balance_tracking(&mut self) {
        self.balance_tracking = None;
    }

    /// Single write-bookkeeping choke point: dirty-set entry (delta membership) + tripwire
    /// counters. Every state-changing accessor funnels through here — the typed-store analogue of
    /// the overlay's `store_struct`/`store_bytes` bookkeeping.
    fn mark(&mut self, key: B256, slot: StoreSlot) {
        self.dirty.insert(key, slot);
        self.write_count += 1;
        self.tx_dirty = true;
    }

    // ── per-user account ───────────────────────────────────────────────────────
    /// Three-state read of a user's account (see [`Resident`]).
    pub fn account(&self, user: Address) -> Resident<'_, UserAccount> {
        match self.accounts.get(&user) {
            None => Resident::Miss,
            Some(None) => Resident::Deleted,
            Some(Some(a)) => Resident::Hit(a.as_ref()),
        }
    }

    /// `&mut` access marks the key dirty (conservative: any mutable borrow is a potential write,
    /// matching the current `get_struct_mut` write-count semantics). CoW: clones the value iff
    /// the Arc is shared (first write after a cold fill), in-place thereafter. `None` for a
    /// missing OR deleted entry (mutating either is a caller bug; callers materialize first).
    pub fn account_mut(&mut self, user: Address) -> Option<&mut UserAccount> {
        match self.accounts.get_mut(&user) {
            Some(Some(a)) => {
                // Inlined `mark` (disjoint-field borrows: `a` holds `accounts`).
                self.dirty
                    .insert(keys::account_key(user), StoreSlot::Account(user));
                self.write_count += 1;
                self.tx_dirty = true;
                Some(Arc::make_mut(a))
            }
            _ => None,
        }
    }

    /// Zero-clone shared read for the `load_*_ref` path: hands back the sub-map's Arc (refcount
    /// bump). Same three states as [`Self::account`], flattened: `Some(arc)` = hit; `None` covers
    /// BOTH deleted and miss — callers needing the distinction use [`Self::account`] first.
    pub fn account_arc(&self, user: Address) -> Option<Arc<UserAccount>> {
        self.accounts.get(&user).and_then(|s| s.clone())
    }

    /// Cold-fill from the cross-block committed store: stores the SAME Arc (zero clone) WITHOUT
    /// marking dirty — a fill is a cache event, not a write (exactly `cache_put` semantics; a
    /// dirty mark here would inject a spurious key into the block delta = commitment fork).
    /// `None` caches ABSENCE (repeated missing-key loads stop re-probing the DB; not a tombstone —
    /// no dirty mark, so nothing is emitted at block end). Never overwrites an existing entry
    /// (write-wins, mirroring `cache_put`).
    pub fn fill_account(&mut self, user: Address, value: Option<Arc<UserAccount>>) {
        self.accounts.entry(user).or_insert(value);
    }

    /// Inserts/overwrites a user's account and marks its key dirty.
    pub fn set_account(&mut self, user: Address, value: UserAccount) {
        self.mark(keys::account_key(user), StoreSlot::Account(user));
        self.accounts.insert(user, Some(Arc::new(value)));
    }

    /// Removes a user's account: leaves a resident tombstone (reads → [`Resident::Deleted`]) and
    /// marks the key dirty (block-end delta emits empty bytes).
    pub fn remove_account(&mut self, user: Address) {
        self.mark(keys::account_key(user), StoreSlot::Account(user));
        self.accounts.insert(user, None);
    }

    // ── per-market config ────────────────────────────────────────────────────
    /// Three-state read of a market's config (see [`Resident`]).
    pub fn market(&self, market_id: u64) -> Resident<'_, Market> {
        match self.markets.get(&market_id) {
            None => Resident::Miss,
            Some(None) => Resident::Deleted,
            Some(Some(m)) => Resident::Hit(m.as_ref()),
        }
    }

    /// Mutable market access; marks its key dirty (see [`Self::account_mut`]).
    pub fn market_mut(&mut self, market_id: u64) -> Option<&mut Market> {
        match self.markets.get_mut(&market_id) {
            Some(Some(m)) => {
                // Inlined `mark` (disjoint-field borrows: `m` holds `markets`).
                self.dirty
                    .insert(keys::market_key(market_id), StoreSlot::Market(market_id));
                self.write_count += 1;
                self.tx_dirty = true;
                Some(Arc::make_mut(m))
            }
            _ => None,
        }
    }

    /// Zero-clone shared read (Arc bump); `None` covers deleted AND miss (see [`Self::account_arc`]).
    pub fn market_arc(&self, market_id: u64) -> Option<Arc<Market>> {
        self.markets.get(&market_id).and_then(|s| s.clone())
    }

    /// Cold-fill (cache semantics, no dirty mark; see [`Self::fill_account`]).
    pub fn fill_market(&mut self, market_id: u64, value: Option<Arc<Market>>) {
        self.markets.entry(market_id).or_insert(value);
    }

    /// Inserts/overwrites a market and marks its key dirty.
    pub fn set_market(&mut self, market_id: u64, value: Market) {
        self.mark(keys::market_key(market_id), StoreSlot::Market(market_id));
        self.markets.insert(market_id, Some(Arc::new(value)));
    }

    // ── per-(user, market) position ──────────────────────────────────────────
    /// Three-state read of a user's position in a market (see [`Resident`]).
    pub fn position(&self, user: Address, market_id: u64) -> Resident<'_, PerpPosition> {
        match self.positions.get(&(user, market_id)) {
            None => Resident::Miss,
            Some(None) => Resident::Deleted,
            Some(Some(p)) => Resident::Hit(p.as_ref()),
        }
    }

    /// Mutable position access; marks its key dirty (see [`Self::account_mut`]).
    pub fn position_mut(&mut self, user: Address, market_id: u64) -> Option<&mut PerpPosition> {
        match self.positions.get_mut(&(user, market_id)) {
            Some(Some(p)) => {
                // Inlined `mark` (disjoint-field borrows: `p` holds `positions`).
                self.dirty.insert(
                    keys::position_key(user, market_id),
                    StoreSlot::Position(user, market_id),
                );
                self.write_count += 1;
                self.tx_dirty = true;
                Some(Arc::make_mut(p))
            }
            _ => None,
        }
    }

    /// Zero-clone shared read (Arc bump); `None` covers deleted AND miss (see [`Self::account_arc`]).
    pub fn position_arc(&self, user: Address, market_id: u64) -> Option<Arc<PerpPosition>> {
        self.positions.get(&(user, market_id)).and_then(|s| s.clone())
    }

    /// Cold-fill (cache semantics, no dirty mark; see [`Self::fill_account`]).
    pub fn fill_position(
        &mut self,
        user: Address,
        market_id: u64,
        value: Option<Arc<PerpPosition>>,
    ) {
        self.positions.entry((user, market_id)).or_insert(value);
    }

    /// Inserts/overwrites a position and marks its key dirty.
    pub fn set_position(&mut self, user: Address, market_id: u64, value: PerpPosition) {
        self.mark(
            keys::position_key(user, market_id),
            StoreSlot::Position(user, market_id),
            );
        self.positions.insert((user, market_id), Some(Arc::new(value)));
    }

    /// Removes a position: resident tombstone + dirty mark (delta emits empty bytes).
    pub fn remove_position(&mut self, user: Address, market_id: u64) {
        self.mark(
            keys::position_key(user, market_id),
            StoreSlot::Position(user, market_id),
            );
        self.positions.insert((user, market_id), None);
    }

    // ── per-market hot scalars (MarketHot) ──────────────────────────────────
    /// Three-state read (see [`Resident`]).
    pub fn market_hot(&self, market_id: u64) -> Resident<'_, MarketHot> {
        match self.market_hots.get(&market_id) {
            None => Resident::Miss,
            Some(None) => Resident::Deleted,
            Some(Some(h)) => Resident::Hit(h.as_ref()),
        }
    }

    /// Mutable access; marks dirty (see [`Self::account_mut`]).
    pub fn market_hot_mut(&mut self, market_id: u64) -> Option<&mut MarketHot> {
        match self.market_hots.get_mut(&market_id) {
            Some(Some(h)) => {
                self.dirty.insert(
                    keys::market_hot_key(market_id),
                    StoreSlot::MarketHot(market_id),
                );
                self.write_count += 1;
                self.tx_dirty = true;
                Some(Arc::make_mut(h))
            }
            _ => None,
        }
    }

    /// Inserts/overwrites and marks dirty.
    pub fn set_market_hot(&mut self, market_id: u64, value: MarketHot) {
        self.mark(
            keys::market_hot_key(market_id),
            StoreSlot::MarketHot(market_id),
        );
        self.market_hots.insert(market_id, Some(Arc::new(value)));
    }

    /// Cold-fill (cache semantics, no dirty mark; see [`Self::fill_account`]).
    pub fn fill_market_hot(&mut self, market_id: u64, value: Option<Arc<MarketHot>>) {
        self.market_hots.entry(market_id).or_insert(value);
    }

    // ── per-(user, market) order-entry lists (bord / sord) ──────────────────
    /// Three-state read of the buy-order list (see [`Resident`]).
    pub fn buy_orders(&self, user: Address, market_id: u64) -> Resident<'_, Vec<OrderEntry>> {
        match self.buy_orders.get(&(user, market_id)) {
            None => Resident::Miss,
            Some(None) => Resident::Deleted,
            Some(Some(v)) => Resident::Hit(v.as_ref()),
        }
    }

    /// Zero-clone shared read (Arc bump); `None` covers deleted AND miss.
    pub fn buy_orders_arc(&self, user: Address, market_id: u64) -> Option<Arc<Vec<OrderEntry>>> {
        self.buy_orders.get(&(user, market_id)).and_then(|s| s.clone())
    }

    /// Mutable access; marks dirty (see [`Self::account_mut`]).
    pub fn buy_orders_mut(&mut self, user: Address, market_id: u64) -> Option<&mut Vec<OrderEntry>> {
        match self.buy_orders.get_mut(&(user, market_id)) {
            Some(Some(v)) => {
                self.dirty.insert(
                    keys::user_buy_orders_key(user, market_id),
                    StoreSlot::BuyOrders(user, market_id),
                );
                self.write_count += 1;
                self.tx_dirty = true;
                Some(Arc::make_mut(v))
            }
            _ => None,
        }
    }

    /// Inserts/overwrites and marks dirty. NOTE: an EMPTY list is a legitimate stored value
    /// (encodes to msgpack `0x90`, key stays present) — never converted to a delete.
    pub fn set_buy_orders(&mut self, user: Address, market_id: u64, value: Vec<OrderEntry>) {
        self.mark(
            keys::user_buy_orders_key(user, market_id),
            StoreSlot::BuyOrders(user, market_id),
        );
        self.buy_orders.insert((user, market_id), Some(Arc::new(value)));
    }

    /// Cold-fill (cache semantics, no dirty mark).
    pub fn fill_buy_orders(
        &mut self,
        user: Address,
        market_id: u64,
        value: Option<Arc<Vec<OrderEntry>>>,
    ) {
        self.buy_orders.entry((user, market_id)).or_insert(value);
    }

    /// Three-state read of the sell-order list (see [`Resident`]).
    pub fn sell_orders(&self, user: Address, market_id: u64) -> Resident<'_, Vec<OrderEntry>> {
        match self.sell_orders.get(&(user, market_id)) {
            None => Resident::Miss,
            Some(None) => Resident::Deleted,
            Some(Some(v)) => Resident::Hit(v.as_ref()),
        }
    }

    /// Zero-clone shared read (Arc bump); `None` covers deleted AND miss.
    pub fn sell_orders_arc(&self, user: Address, market_id: u64) -> Option<Arc<Vec<OrderEntry>>> {
        self.sell_orders.get(&(user, market_id)).and_then(|s| s.clone())
    }

    /// Mutable access; marks dirty (see [`Self::account_mut`]).
    pub fn sell_orders_mut(
        &mut self,
        user: Address,
        market_id: u64,
    ) -> Option<&mut Vec<OrderEntry>> {
        match self.sell_orders.get_mut(&(user, market_id)) {
            Some(Some(v)) => {
                self.dirty.insert(
                    keys::user_sell_orders_key(user, market_id),
                    StoreSlot::SellOrders(user, market_id),
                );
                self.write_count += 1;
                self.tx_dirty = true;
                Some(Arc::make_mut(v))
            }
            _ => None,
        }
    }

    /// Inserts/overwrites and marks dirty (empty list stays a stored `0x90`, see buy side).
    pub fn set_sell_orders(&mut self, user: Address, market_id: u64, value: Vec<OrderEntry>) {
        self.mark(
            keys::user_sell_orders_key(user, market_id),
            StoreSlot::SellOrders(user, market_id),
        );
        self.sell_orders.insert((user, market_id), Some(Arc::new(value)));
    }

    /// Cold-fill (cache semantics, no dirty mark).
    pub fn fill_sell_orders(
        &mut self,
        user: Address,
        market_id: u64,
        value: Option<Arc<Vec<OrderEntry>>>,
    ) {
        self.sell_orders.entry((user, market_id)).or_insert(value);
    }

    // ── per-order records (delete-on-terminal) ──────────────────────────────
    /// Three-state read (see [`Resident`]). `Deleted` is load-bearing here: delete-on-terminal
    /// removes the record in-block, and getOrder must see not-found, not the stale committed row.
    pub fn order(&self, order_id: &[u8; 32]) -> Resident<'_, Order> {
        match self.orders.get(order_id) {
            None => Resident::Miss,
            Some(None) => Resident::Deleted,
            Some(Some(o)) => Resident::Hit(o.as_ref()),
        }
    }

    /// Zero-clone shared read (Arc bump); `None` covers deleted AND miss.
    pub fn order_arc(&self, order_id: &[u8; 32]) -> Option<Arc<Order>> {
        self.orders.get(order_id).and_then(|s| s.clone())
    }

    /// Inserts/overwrites and marks dirty.
    pub fn set_order(&mut self, order_id: &[u8; 32], value: Order) {
        self.mark(keys::order_key(order_id), StoreSlot::Order(*order_id));
        self.orders.insert(*order_id, Some(Arc::new(value)));
    }

    /// Deletes the record (delete-on-terminal): resident tombstone + dirty mark → the delta emits
    /// empty bytes (the store DELETE convention).
    pub fn remove_order(&mut self, order_id: &[u8; 32]) {
        self.mark(keys::order_key(order_id), StoreSlot::Order(*order_id));
        self.orders.insert(*order_id, None);
    }

    /// Cold-fill (cache semantics, no dirty mark).
    pub fn fill_order(&mut self, order_id: &[u8; 32], value: Option<Arc<Order>>) {
        self.orders.entry(*order_id).or_insert(value);
    }

    // ── per-market active price levels (bidp / askp), sorted Vec<u64> ────────
    /// Three-state read of the bid price index (see [`Resident`]).
    pub fn bid_prices(&self, market_id: u64) -> Resident<'_, Vec<u64>> {
        match self.bid_prices.get(&market_id) {
            None => Resident::Miss,
            Some(None) => Resident::Deleted,
            Some(Some(v)) => Resident::Hit(v.as_ref()),
        }
    }

    /// Zero-clone shared read (Arc bump); `None` covers deleted AND miss.
    pub fn bid_prices_arc(&self, market_id: u64) -> Option<Arc<Vec<u64>>> {
        self.bid_prices.get(&market_id).and_then(|s| s.clone())
    }

    /// Inserts/overwrites the whole index and marks dirty.
    pub fn set_bid_prices(&mut self, market_id: u64, value: Vec<u64>) {
        self.mark(keys::bid_prices_key(market_id), StoreSlot::BidPrices(market_id));
        self.bid_prices.insert(market_id, Some(Arc::new(value)));
    }

    /// Cold-fill (cache semantics, no dirty mark).
    pub fn fill_bid_prices(&mut self, market_id: u64, value: Option<Arc<Vec<u64>>>) {
        self.bid_prices.entry(market_id).or_insert(value);
    }

    /// `&mut` to the resident index (materialize empty if deleted/absent), AUTO-marking dirty —
    /// for the UNCONDITIONAL mutate (remove_*_price / mutate_bid_prices, which always write).
    pub fn bid_prices_mut(&mut self, market_id: u64) -> &mut Vec<u64> {
        self.dirty
            .insert(keys::bid_prices_key(market_id), StoreSlot::BidPrices(market_id));
        self.write_count += 1;
        self.tx_dirty = true;
        Arc::make_mut(
            self.bid_prices
                .entry(market_id)
                .or_insert_with(|| Some(Arc::new(Vec::new())))
                .get_or_insert_with(|| Arc::new(Vec::new())),
        )
    }

    /// `&mut` to the resident index WITHOUT marking — the caller marks (via [`Self::mark_bid_prices`])
    /// only if it actually changed, preserving `insert_*_price`'s conditional-write delta semantics.
    pub fn bid_prices_mut_nomark(&mut self, market_id: u64) -> &mut Vec<u64> {
        Arc::make_mut(
            self.bid_prices
                .entry(market_id)
                .or_insert_with(|| Some(Arc::new(Vec::new())))
                .get_or_insert_with(|| Arc::new(Vec::new())),
        )
    }

    /// Marks the bid-price index dirty (delta membership). Pair with [`Self::bid_prices_mut_nomark`].
    pub fn mark_bid_prices(&mut self, market_id: u64) {
        self.mark(keys::bid_prices_key(market_id), StoreSlot::BidPrices(market_id));
    }

    /// Three-state read of the ask price index (see [`Resident`]).
    pub fn ask_prices(&self, market_id: u64) -> Resident<'_, Vec<u64>> {
        match self.ask_prices.get(&market_id) {
            None => Resident::Miss,
            Some(None) => Resident::Deleted,
            Some(Some(v)) => Resident::Hit(v.as_ref()),
        }
    }

    /// Zero-clone shared read (Arc bump); `None` covers deleted AND miss.
    pub fn ask_prices_arc(&self, market_id: u64) -> Option<Arc<Vec<u64>>> {
        self.ask_prices.get(&market_id).and_then(|s| s.clone())
    }

    /// Inserts/overwrites the whole index and marks dirty.
    pub fn set_ask_prices(&mut self, market_id: u64, value: Vec<u64>) {
        self.mark(keys::ask_prices_key(market_id), StoreSlot::AskPrices(market_id));
        self.ask_prices.insert(market_id, Some(Arc::new(value)));
    }

    /// Cold-fill (cache semantics, no dirty mark).
    pub fn fill_ask_prices(&mut self, market_id: u64, value: Option<Arc<Vec<u64>>>) {
        self.ask_prices.entry(market_id).or_insert(value);
    }

    /// `&mut` to the resident index, AUTO-marking dirty (unconditional mutate). See bid side.
    pub fn ask_prices_mut(&mut self, market_id: u64) -> &mut Vec<u64> {
        self.dirty
            .insert(keys::ask_prices_key(market_id), StoreSlot::AskPrices(market_id));
        self.write_count += 1;
        self.tx_dirty = true;
        Arc::make_mut(
            self.ask_prices
                .entry(market_id)
                .or_insert_with(|| Some(Arc::new(Vec::new())))
                .get_or_insert_with(|| Arc::new(Vec::new())),
        )
    }

    /// `&mut` WITHOUT marking (caller marks conditionally). See bid side.
    pub fn ask_prices_mut_nomark(&mut self, market_id: u64) -> &mut Vec<u64> {
        Arc::make_mut(
            self.ask_prices
                .entry(market_id)
                .or_insert_with(|| Some(Arc::new(Vec::new())))
                .get_or_insert_with(|| Arc::new(Vec::new())),
        )
    }

    /// Marks the ask-price index dirty. Pair with [`Self::ask_prices_mut_nomark`].
    pub fn mark_ask_prices(&mut self, market_id: u64) {
        self.mark(keys::ask_prices_key(market_id), StoreSlot::AskPrices(market_id));
    }

    // ── per-(market, price) level FIFO blobs (bidl / askl), RAW pack_level codec ──
    // NOTE: LevelBlob is NOT msgpack — take_delta serializes it via `pack_level`, where
    // `count == 0` packs to EMPTY bytes = the delete convention. So a level is emptied by setting
    // count=0 (ids cleared), NOT by a None tombstone; the slot stays `Some(LevelBlob{count:0})` and
    // the delete manifests only in the delta bytes (byte-identical to the old ser_level path).
    // A cold-absent level (fill None) reads as an empty default blob, same as `unpack_level("")`.

    /// Resident level blob (`None` = not resident this block → cold path).
    pub fn bid_level(&self, market_id: u64, price: u64) -> Option<&LevelBlob> {
        self.bid_levels
            .get(&(market_id, price))
            .and_then(|s| s.as_deref())
    }

    /// Zero-clone shared read (Arc bump); `None` = not resident (miss/cold-absent).
    pub fn bid_level_arc(&self, market_id: u64, price: u64) -> Option<Arc<LevelBlob>> {
        self.bid_levels.get(&(market_id, price)).and_then(|s| s.clone())
    }

    /// Whether the key is resident this block (Some slot present), regardless of live/absent.
    pub fn bid_level_resident(&self, market_id: u64, price: u64) -> bool {
        self.bid_levels.contains_key(&(market_id, price))
    }

    /// Inserts/overwrites the level blob and marks dirty.
    pub fn set_bid_level(&mut self, market_id: u64, price: u64, value: LevelBlob) {
        self.mark(keys::bid_level_key(market_id, price), StoreSlot::BidLevel(market_id, price));
        self.bid_levels.insert((market_id, price), Some(Arc::new(value)));
    }

    /// Cold-fill (cache semantics, no dirty mark).
    pub fn fill_bid_level(&mut self, market_id: u64, price: u64, value: Option<Arc<LevelBlob>>) {
        self.bid_levels.entry((market_id, price)).or_insert(value);
    }

    /// `&mut` to the resident blob (materialize empty default if deleted/absent), AUTO-marking dirty
    /// — for the in-place mutators (push / decr). CoW: clone iff shared.
    pub fn bid_level_mut(&mut self, market_id: u64, price: u64) -> &mut LevelBlob {
        self.dirty
            .insert(keys::bid_level_key(market_id, price), StoreSlot::BidLevel(market_id, price));
        self.write_count += 1;
        self.tx_dirty = true;
        Arc::make_mut(
            self.bid_levels
                .entry((market_id, price))
                .or_insert_with(|| Some(Arc::new(LevelBlob::default())))
                .get_or_insert_with(|| Arc::new(LevelBlob::default())),
        )
    }

    /// Resident ask level blob (`None` = not resident). See bid side.
    pub fn ask_level(&self, market_id: u64, price: u64) -> Option<&LevelBlob> {
        self.ask_levels
            .get(&(market_id, price))
            .and_then(|s| s.as_deref())
    }

    /// Zero-clone shared read (Arc bump). See bid side.
    pub fn ask_level_arc(&self, market_id: u64, price: u64) -> Option<Arc<LevelBlob>> {
        self.ask_levels.get(&(market_id, price)).and_then(|s| s.clone())
    }

    /// Whether the ask level key is resident this block. See bid side.
    pub fn ask_level_resident(&self, market_id: u64, price: u64) -> bool {
        self.ask_levels.contains_key(&(market_id, price))
    }

    /// Inserts/overwrites and marks dirty. See bid side.
    pub fn set_ask_level(&mut self, market_id: u64, price: u64, value: LevelBlob) {
        self.mark(keys::ask_level_key(market_id, price), StoreSlot::AskLevel(market_id, price));
        self.ask_levels.insert((market_id, price), Some(Arc::new(value)));
    }

    /// Cold-fill (cache semantics, no dirty mark). See bid side.
    pub fn fill_ask_level(&mut self, market_id: u64, price: u64, value: Option<Arc<LevelBlob>>) {
        self.ask_levels.entry((market_id, price)).or_insert(value);
    }

    /// `&mut` (materialize empty default), AUTO-marking dirty. See bid side.
    pub fn ask_level_mut(&mut self, market_id: u64, price: u64) -> &mut LevelBlob {
        self.dirty
            .insert(keys::ask_level_key(market_id, price), StoreSlot::AskLevel(market_id, price));
        self.write_count += 1;
        self.tx_dirty = true;
        Arc::make_mut(
            self.ask_levels
                .entry((market_id, price))
                .or_insert_with(|| Some(Arc::new(LevelBlob::default())))
                .get_or_insert_with(|| Arc::new(LevelBlob::default())),
        )
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
            // Absent-from-map and resident-tombstone both lower to empty bytes (delete).
            let bytes = match slot {
                StoreSlot::Account(u) => match self.accounts.get(&u) {
                    Some(Some(v)) => encode(v.as_ref())?,
                    _ => Vec::new(),
                },
                StoreSlot::Market(m) => match self.markets.get(&m) {
                    Some(Some(v)) => encode(v.as_ref())?,
                    _ => Vec::new(),
                },
                StoreSlot::Position(u, m) => match self.positions.get(&(u, m)) {
                    Some(Some(v)) => encode(v.as_ref())?,
                    _ => Vec::new(),
                },
                StoreSlot::MarketHot(m) => match self.market_hots.get(&m) {
                    Some(Some(v)) => encode(v.as_ref())?,
                    _ => Vec::new(),
                },
                StoreSlot::BuyOrders(u, m) => match self.buy_orders.get(&(u, m)) {
                    Some(Some(v)) => encode(v.as_ref())?,
                    _ => Vec::new(),
                },
                StoreSlot::SellOrders(u, m) => match self.sell_orders.get(&(u, m)) {
                    Some(Some(v)) => encode(v.as_ref())?,
                    _ => Vec::new(),
                },
                StoreSlot::Order(id) => match self.orders.get(&id) {
                    Some(Some(v)) => encode(v.as_ref())?,
                    _ => Vec::new(),
                },
                StoreSlot::BidPrices(m) => match self.bid_prices.get(&m) {
                    Some(Some(v)) => encode(v.as_ref())?,
                    _ => Vec::new(),
                },
                StoreSlot::AskPrices(m) => match self.ask_prices.get(&m) {
                    Some(Some(v)) => encode(v.as_ref())?,
                    _ => Vec::new(),
                },
                // RAW codec (pack_level, not msgpack); count==0 → empty (delete).
                StoreSlot::BidLevel(m, p) => match self.bid_levels.get(&(m, p)) {
                    Some(Some(b)) => pack_level(b.as_ref()),
                    _ => Vec::new(),
                },
                StoreSlot::AskLevel(m, p) => match self.ask_levels.get(&(m, p)) {
                    Some(Some(b)) => pack_level(b.as_ref()),
                    _ => Vec::new(),
                },
            };
            out.push((key, bytes));
        }
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

/// The whole-store erasure seam (Stage A.2): the journal holds this as `Box<dyn PerpStore>` and
/// the storage layer downcasts once per op via [`PerpStore::as_any_mut`]. `take_delta` here feeds
/// the block-end `take_perp_delta` union — entries carry `decoded` (Arc of the typed value, for
/// the cross-block committed store, 选项A) alongside the canonical bytes, mirroring what the
/// overlay drain produces for `PerpEntry::Struct`.
impl PerpStore for TypedPerpStore {
    fn take_delta(&mut self) -> PerpDelta {
        let mut out = PerpDelta::default();
        // Encode failure is a bug (matches the overlay's `ser_blob` convention: the value was
        // well-formed enough to store, so canonical encoding cannot fail). `decoded` shares the
        // sub-map's Arc — zero clone into the cross-block committed store.
        fn entry<T: serde::Serialize + Send + Sync + 'static>(
            v: Option<&Slot<T>>,
        ) -> PerpDeltaEntry {
            match v {
                Some(Some(v)) => PerpDeltaEntry {
                    decoded: Some(v.clone() as Arc<PerpBlob>),
                    bytes: encode(v.as_ref()).expect("perp typed-store encode failure"),
                },
                // Removed this block (resident tombstone) → empty bytes (the delete convention);
                // nothing to retain in the decoded cross-block store.
                _ => PerpDeltaEntry {
                    decoded: None,
                    bytes: Vec::new(),
                },
            }
        }
        // RAW-codec level entry (pack_level, NOT msgpack): count==0 → empty bytes (delete). Mirrors
        // the overlay's ser_level Struct drain — decoded rides along (选项A), bytes = pack_level.
        fn level_entry(v: Option<&Slot<LevelBlob>>) -> PerpDeltaEntry {
            match v {
                Some(Some(b)) => PerpDeltaEntry {
                    decoded: Some(b.clone() as Arc<PerpBlob>),
                    bytes: pack_level(b.as_ref()),
                },
                _ => PerpDeltaEntry {
                    decoded: None,
                    bytes: Vec::new(),
                },
            }
        }
        for (key, slot) in self.dirty.drain() {
            let e = match slot {
                StoreSlot::Account(u) => entry(self.accounts.get(&u)),
                StoreSlot::Market(m) => entry(self.markets.get(&m)),
                StoreSlot::Position(u, m) => entry(self.positions.get(&(u, m))),
                StoreSlot::MarketHot(m) => entry(self.market_hots.get(&m)),
                StoreSlot::BuyOrders(u, m) => entry(self.buy_orders.get(&(u, m))),
                StoreSlot::SellOrders(u, m) => entry(self.sell_orders.get(&(u, m))),
                StoreSlot::Order(id) => entry(self.orders.get(&id)),
                StoreSlot::BidPrices(m) => entry(self.bid_prices.get(&m)),
                StoreSlot::AskPrices(m) => entry(self.ask_prices.get(&m)),
                StoreSlot::BidLevel(m, p) => level_entry(self.bid_levels.get(&(m, p))),
                StoreSlot::AskLevel(m, p) => level_entry(self.ask_levels.get(&(m, p))),
            };
            out.insert(key, e);
        }
        out
    }

    fn clone_box(&self) -> Box<dyn PerpStore> {
        Box::new(self.clone())
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn write_count(&self) -> u64 {
        self.write_count
    }

    fn tx_dirty(&self) -> bool {
        self.tx_dirty
    }

    fn reset_tx_dirty(&mut self) {
        self.tx_dirty = false;
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

        assert_eq!(store.account(u), Resident::Miss);
        store.set_account(u, UserAccount::default());
        {
            let a = store.account_mut(u).expect("just set");
            a.nonce = 7;
            a.perp_wallet_balance = 123;
        }
        assert_eq!(store.account(u).hit().unwrap().nonce, 7);

        store.set_market(1, sample_market(1));
        assert_eq!(store.market(1).hit().unwrap().mark_price, 65_000_00);

        let mut pos = PerpPosition::default();
        pos.amount = -42;
        pos.leverage = 6;
        store.set_position(u, 1, pos.clone());
        assert_eq!(store.position(u, 1).hit().unwrap().amount, -42);
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

    /// Stage A.2 end-to-end: install `TypedPerpStore` into a real `Journal` through the
    /// `JournalTr` seam, write through the per-op downcast, and harvest via `take_perp_delta` —
    /// the union path the block executor calls. Also proves overlay + live-store deltas merge
    /// (disjoint keys) and that the live-store entry carries canonical bytes + decoded Arc.
    #[test]
    fn journal_seam_install_downcast_write_harvest() {
        use context::{Journal, JournalTr};
        use database::InMemoryDB;

        let mut journal: Journal<InMemoryDB> = Journal::new(InMemoryDB::default());
        let u = addr(0x44);

        // Install through the seam (first touch), then per-op: get_mut → downcast → typed write.
        journal.perp_live_init(Box::new(TypedPerpStore::default()));
        {
            let store = journal
                .perp_live_get_mut()
                .expect("installed")
                .as_any_mut()
                .downcast_mut::<TypedPerpStore>()
                .expect("concrete type");
            let mut acct = UserAccount::default();
            acct.nonce = 9;
            store.set_account(u, acct);
        }

        // A raw overlay write on a DIFFERENT key — the union must carry both.
        let level_key = keys::bid_level_key(7, 100);
        journal.perp_store(level_key, vec![1, 2, 3]);

        let delta = journal.take_perp_delta();
        assert_eq!(delta.len(), 2, "overlay + live-store union");
        assert_eq!(delta[&level_key].bytes, vec![1, 2, 3]);

        let acct_entry = &delta[&keys::account_key(u)];
        let mut expect = UserAccount::default();
        expect.nonce = 9;
        assert_eq!(acct_entry.bytes, encode(&expect).unwrap(), "canonical bytes");
        let decoded = acct_entry
            .decoded
            .as_ref()
            .expect("live-store entry carries decoded Arc")
            .downcast_ref::<UserAccount>()
            .expect("decoded is the typed value");
        assert_eq!(decoded.nonce, 9);

        // Dirty set drained: a second harvest is empty.
        assert!(journal.take_perp_delta().is_empty());
    }

    /// The Arc-CoW property the Stage-B cold path relies on: a fill can share an external Arc
    /// (zero clone — same allocation), and the first mutable access breaks the share exactly once
    /// (`Arc::make_mut`), leaving the external holder's value untouched.
    #[test]
    fn arc_cow_shares_cold_fill_and_clones_only_on_first_write() {
        let mut store = TypedPerpStore::default();
        let u = addr(0x55);

        // Simulated cross-block fill: an Arc handed in from the committed store.
        let canonical = Arc::new(UserAccount::default());
        store.accounts.insert(u, Some(canonical.clone()));
        let resident = |s: &TypedPerpStore| s.accounts.get(&u).unwrap().clone().unwrap();
        assert!(
            Arc::ptr_eq(&canonical, &resident(&store)),
            "fill shares the SAME allocation (zero clone)"
        );

        // First write breaks the share (one clone), external Arc keeps the old value.
        store.account_mut(u).unwrap().nonce = 42;
        assert!(!Arc::ptr_eq(&canonical, &resident(&store)));
        assert_eq!(canonical.nonce, 0, "external holder untouched");
        assert_eq!(store.account(u).hit().unwrap().nonce, 42);

        // Second write: Arc now unique → in-place, no further clone (same allocation).
        let after_first = Arc::as_ptr(&resident(&store));
        store.account_mut(u).unwrap().nonce = 43;
        assert_eq!(
            after_first,
            Arc::as_ptr(&resident(&store)),
            "unique Arc mutates in place"
        );
    }

    /// The tombstone-residency property: a key removed THIS block reads `Deleted` (definitively
    /// absent — the reader must NOT fall through to the committed store, which still holds the
    /// pre-delete value), while a never-touched key reads `Miss` (cold path). Collapsing the two
    /// is exactly the read-back-after-delete consensus bug.
    #[test]
    fn removed_reads_deleted_not_miss() {
        let mut store = TypedPerpStore::default();
        let u = addr(0x66);

        store.set_position(u, 3, PerpPosition::default());
        store.remove_position(u, 3);
        assert_eq!(store.position(u, 3), Resident::Deleted);
        assert_eq!(store.position(u, 4), Resident::Miss, "untouched key");
        assert!(store.position_mut(u, 3).is_none(), "no mutable access to a tombstone");

        // The tombstone still emits the delete in the delta.
        let delta = store.take_delta().unwrap();
        assert_eq!(delta.len(), 1);
        assert!(delta[0].1.is_empty());
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
