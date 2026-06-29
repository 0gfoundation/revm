//! Storage helpers for the PerpDEX precompile.

pub mod keys;

use context::{ContextTr, JournalTr};
use primitives::{Address, HashMap, B256, U256};
use rmp_serde::{Deserializer as RMPDeserializer, Serializer as RMPSerializer};
use serde::{Deserialize, Serialize};

use crate::perp_dex::PERP_DEX_ADDRESS;
use crate::{
    perp_dex::{
        errors::perp_err,
        types::{
            ApiKey, FundingState, IndexPriceHistory, IndexPriceState, Market, Order, OrderEntry,
            PerpPosition, PremiumIndexAccumulator, PriceBasisWindow, UserAccount, UserFeeRates,
        },
    },
    stateful_precompiles::convert_db_err,
    PrecompileError,
};

use keys::{
    account_key, admin_key, api_key_ids_key, api_key_key, ask_level_key, best_ask_key,
    best_bid_key, bid_level_key, commitment_slot, erc20_balance_slot,
    funding_state_key, index_price_history_key, index_price_state_key, insurance_fund_key,
    last_traded_price_key, mark_price_key, market_fee_total_key, market_key, market_manager_key,
    open_interest_key, oracle_key, order_key, position_key, premium_accumulator_key,
    price_basis_window_key, trade_count_key, user_buy_orders_key, user_fee_rates_key,
    user_nonce_key, user_sell_orders_key,
};

// ── Generic msgpack helpers ───────────────────────────────────────────────────

fn encode<T: Serialize>(val: &T) -> Result<Vec<u8>, PrecompileError> {
    let mut buf = Vec::new();
    // Positional (array) msgpack: struct field NAMES are NOT serialized (P4/#20) — smaller blobs
    // and faster decode; the decoder reads fields by position via serde's `visit_seq`. Cross-
    // version blob compatibility is intentionally dropped (the chain is wiped on any encoding
    // change anyway, since blob bytes feed the on-trie commitment), so field ORDER in `types/` is
    // now layout-significant — only append fields, never reorder/insert.
    val.serialize(&mut RMPSerializer::new(&mut buf))
        .map_err(|_| perp_err("msgpack encode error"))?;
    Ok(buf)
}

fn decode<T: for<'de> Deserialize<'de>>(buf: &[u8]) -> Result<T, PrecompileError> {
    let mut de = RMPDeserializer::new(buf);
    Deserialize::deserialize(&mut de).map_err(|_| perp_err("msgpack decode error"))
}

/// Test-only instrumentation wrapped around the single off-trie blob read/write choke
/// (`load_blob`/`store_blob`). Quantifies the ser/deser VOLUME of a run and the redundancy that a
/// per-block deserialized-struct cache (catalog #14) or block-end serialization (#16d) would
/// collapse — measurable WITHOUT implementing either. Disabled by default (only the block-level
/// perf bench `enable()`s it around the timed block) and compiled out of production via
/// `cfg(test)`, so it adds ZERO hot-path cost on chain. Single-threaded use only (perf bench runs
/// `--test-threads=1`); state is thread-local.
#[cfg(test)]
pub(crate) mod bench_counter {
    use super::B256;
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;

    #[derive(Default, Clone)]
    pub(crate) struct Stats {
        /// `load_blob` calls (each is one potential deser; empty/absent reads decode to a default).
        pub read_calls: u64,
        pub read_bytes: u64,
        /// Distinct keys read — a per-block deser cache (#14) collapses `read_calls` → this.
        pub read_keys: HashSet<B256>,
        /// `store_blob` calls (each is one serialize by the typed `save_*` helper).
        pub write_calls: u64,
        pub write_bytes: u64,
        /// Distinct keys written — block-end serialization (#16d) collapses `write_calls` → this.
        pub write_keys: HashSet<B256>,
        /// Distinct (txn, key) pairs — a per-call cache (#14) collapses `write_calls` → this
        /// (still one serialize per written key per txn, for that txn's commitment fold).
        pub write_txn_keys: HashSet<(u32, B256)>,
        /// #16d block-end serializations: `ser_blob` calls during `take_perp_delta` — one per
        /// distinct struct key for the whole block. With deferral active these REPLACE the typed
        /// helpers' per-write `store_blob` serializations (which no longer hit the byte choke), so
        /// `write_calls` drops to the raw byte-path writers (level queues) and serialization is
        /// `block_end_ser_calls`.
        pub block_end_ser_calls: u64,
        pub block_end_ser_bytes: u64,
    }

    thread_local! {
        static STATS: RefCell<Stats> = RefCell::new(Stats::default());
        static TXN: Cell<u32> = const { Cell::new(0) };
        static ON: Cell<bool> = const { Cell::new(false) };
        static FORCE_PERCALL: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn reset() {
        STATS.with(|s| *s.borrow_mut() = Stats::default());
        TXN.with(|t| t.set(0));
    }
    pub(crate) fn enable() {
        ON.with(|o| o.set(true));
    }
    pub(crate) fn disable() {
        ON.with(|o| o.set(false));
    }
    /// When set, `save_cached`/`load_cached` bypass the #16d struct overlay + #14 read cache and
    /// serialize on every write / deserialize on every read (the pre-#14/#16d behavior). Lets one
    /// bench measure per-call vs deferred ser/deser on an identical workload. Independent of
    /// `enable()` (it changes BEHAVIOR, not counting); set explicitly per pass, not cleared by `reset`.
    pub(crate) fn set_force_percall(v: bool) {
        FORCE_PERCALL.with(|c| c.set(v));
    }
    pub(crate) fn force_percall() -> bool {
        FORCE_PERCALL.with(Cell::get)
    }
    /// Marks the start of a new logical transaction (handler call) for per-call write attribution.
    pub(crate) fn next_txn() {
        TXN.with(|t| t.set(t.get().wrapping_add(1)));
    }
    pub(crate) fn snapshot() -> Stats {
        STATS.with(|s| s.borrow().clone())
    }

    pub(crate) fn record_read(key: B256, bytes: usize) {
        if !ON.with(Cell::get) {
            return;
        }
        STATS.with(|s| {
            let mut s = s.borrow_mut();
            s.read_calls += 1;
            s.read_bytes += bytes as u64;
            s.read_keys.insert(key);
        });
    }

    pub(crate) fn record_write(key: B256, bytes: usize) {
        if !ON.with(Cell::get) {
            return;
        }
        let txn = TXN.with(Cell::get);
        STATS.with(|s| {
            let mut s = s.borrow_mut();
            s.write_calls += 1;
            s.write_bytes += bytes as u64;
            s.write_keys.insert(key);
            s.write_txn_keys.insert((txn, key));
        });
    }

    /// Records one block-end serialization (`ser_blob` in `take_perp_delta`).
    pub(crate) fn record_block_end_ser(bytes: usize) {
        if !ON.with(Cell::get) {
            return;
        }
        STATS.with(|s| {
            let mut s = s.borrow_mut();
            s.block_end_ser_calls += 1;
            s.block_end_ser_bytes += bytes as u64;
        });
    }
}

/// Reads an off-trie PerpDEX blob ("PerpState").
///
/// Returns the in-block overlay value if the key was written during this block, otherwise the
/// committed off-trie store. This replaces the previous chunked `sstore`/`sload` blob, which
/// lived in the state trie under `PERP_DEX_ADDRESS`; perp data now rides the journal's perp
/// section instead, so it gets the same revert lifecycle but never enters the state root.
/// See `docs/perpstate-journal集成方案.md` §4.2.
fn load_blob<CTX: ContextTr>(context: &mut CTX, key: B256) -> Result<Vec<u8>, PrecompileError> {
    let buf = context
        .journal_mut()
        .perp_load(key)
        .map_err(convert_db_err::<CTX::Db>)?;
    #[cfg(test)]
    bench_counter::record_read(key, buf.len());
    Ok(buf)
}

/// Writes an off-trie PerpDEX blob. An empty `buf` marks the key absent. The write is journaled
/// in the perp section (reverts in lock-step with the surrounding checkpoint / `discard_tx`) and
/// is never folded into the trie-bound `EvmState`.
///
/// #16d: the on-trie commitment is NO LONGER updated per write. It is folded ONCE at block end from
/// the net delta by [`finalize_block_commitment`] (which the block executor calls after
/// `take_perp_delta`), so a key written N times across the block is hashed once. This replaces the
/// former per-call `flush_commitment` over a per-write framed log.
fn store_blob<CTX: ContextTr>(
    context: &mut CTX,
    key: B256,
    buf: &[u8],
) -> Result<(), PrecompileError> {
    #[cfg(test)]
    bench_counter::record_write(key, buf.len());
    context.journal_mut().perp_store(key, buf.to_vec());
    Ok(())
}

/// Version byte mixed into the per-block commitment hash (catalog #16d). Bumped to 3 at the
/// switch from the per-call chained v2 (retired) to the per-block net-delta fold; bumped to 4 (#21
/// perp-parallel) when the sorted bid_prices/ask_prices price lists were dropped (book uses tick-walk
/// discovery), removing those keys from the committed key-set. Each bump is accompanied by a devnet
/// wipe so the framings never alias across the consensus transition.
const BLOCK_COMMITMENT_VERSION: u8 = 4;

/// Computes the per-BLOCK off-trie commitment over the block's NET delta (catalog #16d).
///
/// `C_block = blake3(C_prev ‖ BLOCK_COMMITMENT_VERSION ‖ Σ_sorted(key(32) ‖ len(u32 BE) ‖ value))`,
/// keys ascending. `delta` is the net block writes from [`JournalTr::take_perp_delta`] (already one
/// value per key, post-revert), so no coalescing is needed — only deterministic key-sorting (keys
/// are a total order; the HashMap is never iterated for the hash). An empty value is a deleted key,
/// framed with len 0 (same convention as the per-call path). Pure: the caller reads `C_prev` and
/// sstores the result. This commits the block's net STATE CHANGE; chained onto the previous block's
/// `C` it forms a block-granular running commitment, the off-trie analogue of the state root.
pub fn compute_block_commitment(prev: U256, delta: &HashMap<B256, Vec<u8>>) -> U256 {
    let mut keys: Vec<&B256> = delta.keys().collect();
    keys.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&prev.to_be_bytes::<32>());
    hasher.update(&[BLOCK_COMMITMENT_VERSION]);
    for key in keys {
        let value = &delta[key];
        hasher.update(key.as_slice());
        hasher.update(&(value.len() as u32).to_be_bytes());
        hasher.update(value);
    }
    U256::from_be_bytes(*hasher.finalize().as_bytes())
}

/// Block-end hook (catalog #16d): folds the block's net perp delta into the on-trie 0x1003 anchor
/// ONCE, replacing the per-call [`flush_commitment`]. The block executor calls this after
/// [`JournalTr::take_perp_delta`], while the journal is still alive. No-op on an empty delta.
/// `warm_account` + `touch_account` mirror `flush_commitment` / `save_erc20_balance`.
///
/// This writes a journaled `sstore` into the journal overlay ONLY — it does NOT itself reach the
/// `State`/bundle. Because the block executor's `finish`/`into_db` merely extract
/// `journaled_state.database` (dropping the overlay), the caller MUST drain the journal afterwards
/// (`JournalTr::finalize`) and commit the returned changeset into its `State` DB so the slot change
/// becomes a bundle transition. The alloy-evm `Evm::finalize_perp_commitment` wrapper does exactly
/// that. (Pre-#16d this ran inside a tx, so the enclosing `transact`+commit carried it; the
/// block-end call site has no such enclosing commit.) Journaled like any sstore.
pub fn finalize_block_commitment<CTX: ContextTr>(
    context: &mut CTX,
    delta: &HashMap<B256, Vec<u8>>,
) -> Result<(), PrecompileError> {
    if delta.is_empty() {
        return Ok(());
    }
    context
        .journal_mut()
        .warm_account(PERP_DEX_ADDRESS)
        .map_err(convert_db_err::<CTX::Db>)?;
    let c_prev = context
        .journal_mut()
        .sload(PERP_DEX_ADDRESS, commitment_slot().into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data;
    let c_new = compute_block_commitment(c_prev, delta);
    context
        .journal_mut()
        .sstore(PERP_DEX_ADDRESS, commitment_slot().into(), c_new)
        .map_err(convert_db_err::<CTX::Db>)?;
    context.journal_mut().touch_account(PERP_DEX_ADDRESS);
    Ok(())
}

// ── Admin ─────────────────────────────────────────────────────────────────────

/// Cached typed read of an off-trie blob (catalog #14). Returns the block-cached deserialized
/// value if present, else loads + msgpack-decodes the blob and caches it. `Ok(None)` if the blob
/// is absent (empty); the caller applies its own default / `Option` semantics (absence is not
/// cached). The cache lives in the journal's block-scoped, revert-cleared `PerpSection`, so this
/// collapses repeated reads of a hot blob within a block to a single decode.
fn load_cached<CTX: ContextTr, T>(context: &mut CTX, key: B256) -> Result<Option<T>, PrecompileError>
where
    T: Clone + 'static + for<'de> Deserialize<'de>,
{
    // Bench-only A/B lever (#14/#16d measurement): deserialize on every read with no cache,
    // reproducing pre-#14 behavior so a bench can diff per-call vs cached deser cost.
    #[cfg(test)]
    if bench_counter::force_percall() {
        let buf = load_blob(context, key)?;
        if buf.is_empty() {
            return Ok(None);
        }
        return Ok(Some(decode(&buf)?));
    }
    // Fast path: a deferred struct written this block — clone in place, no deserialization.
    if let Some(v) = context.journal_mut().perp_with_struct::<T, T>(key, |v| v.clone()) {
        return Ok(Some(v));
    }
    // Cold-read deser cache (#14), for keys only READ this block (not in the write overlay).
    if let Some(any) = context.journal_mut().perp_cache_get(key) {
        if let Some(v) = any.downcast_ref::<T>() {
            return Ok(Some(v.clone()));
        }
    }
    // Cold read: committed off-trie store (or an overlay Bytes entry) → decode once → cache.
    let buf = load_blob(context, key)?;
    if buf.is_empty() {
        return Ok(None);
    }
    let val: T = decode(&buf)?;
    context
        .journal_mut()
        .perp_cache_put(key, std::boxed::Box::new(val.clone()));
    Ok(Some(val))
}

/// Serializes a type-erased off-trie blob to its canonical bytes — the #16d block-end serializer,
/// monomorphized per blob type `T` and stored as a fn-ptr in the journal overlay. Produces bytes
/// IDENTICAL to a direct `encode`, so deferring serialization to block end leaves the commitment
/// byte-stream (and the on-trie anchor) unchanged. A type mismatch / encode failure is a bug.
fn ser_blob<T: Serialize + 'static>(v: &dyn core::any::Any) -> Vec<u8> {
    let val = v
        .downcast_ref::<T>()
        .expect("perp ser_blob: overlay value type mismatch (bug)");
    let buf = encode(val).expect("perp ser_blob: blob encode failed (bug)");
    #[cfg(test)]
    bench_counter::record_block_end_ser(buf.len());
    buf
}

/// Clones a type-erased off-trie blob into a fresh box (keeps the overlay `Clone`). The box is
/// `Send + Sync` because the parallel-path shared book is shared across worker threads (#21 Phase 3);
/// all off-trie blob types (order lists, accounts, levels) are plain `Send + Sync` data.
fn clone_blob<T: Clone + Send + Sync + 'static>(
    v: &dyn core::any::Any,
) -> std::boxed::Box<dyn core::any::Any + Send + Sync> {
    let val = v
        .downcast_ref::<T>()
        .expect("perp clone_blob: overlay value type mismatch (bug)");
    std::boxed::Box::new(val.clone())
}

/// Cached typed write of an off-trie blob (#16d Phase 2): DEFERS serialization. Stores the
/// deserialized struct plus its monomorphized `ser`/`clone` fns in the journal overlay; the
/// block-end `take_perp_delta` lowers it to canonical bytes ONCE (so a key written N times this
/// block is serialized once, not N times). No per-write `encode`, and the struct overlay doubles as
/// the in-block read cache — `store_struct` invalidates the #14 cold-read cache for this key.
fn save_cached<CTX: ContextTr, T>(context: &mut CTX, key: B256, val: &T) -> Result<(), PrecompileError>
where
    T: Clone + 'static + Serialize + Send + Sync,
{
    // Bench-only A/B lever (#16d measurement): serialize on every write into the byte overlay,
    // reproducing pre-#16d behavior so a bench can diff per-call vs deferred ser cost.
    #[cfg(test)]
    if bench_counter::force_percall() {
        let buf = encode(val)?;
        return store_blob(context, key, &buf);
    }
    context.journal_mut().perp_store_struct(
        key,
        std::boxed::Box::new(val.clone()),
        ser_blob::<T>,
        clone_blob::<T>,
    );
    Ok(())
}

/// Returns `Address::ZERO` when no admin has been initialised yet.
pub fn load_admin<CTX: ContextTr>(context: &mut CTX) -> Result<Address, PrecompileError> {
    Ok(load_cached::<_, Address>(context, admin_key())?.unwrap_or(Address::ZERO))
}

pub fn save_admin<CTX: ContextTr>(
    context: &mut CTX,
    admin: Address,
) -> Result<(), PrecompileError> {
    save_cached(context, admin_key(), &admin)
}

// ── UserAccount ───────────────────────────────────────────────────────────────

pub fn load_account<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
) -> Result<UserAccount, PrecompileError> {
    Ok(load_cached::<_, UserAccount>(context, account_key(user))?.unwrap_or_default())
}

pub fn save_account<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    account: UserAccount,
) -> Result<(), PrecompileError> {
    save_cached(context, account_key(user), &account)
}

pub fn load_user_fee_rates<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
) -> Result<UserFeeRates, PrecompileError> {
    Ok(load_cached::<_, UserFeeRates>(context, user_fee_rates_key(user))?.unwrap_or_default())
}

pub fn save_user_fee_rates<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    rates: UserFeeRates,
) -> Result<(), PrecompileError> {
    save_cached(context, user_fee_rates_key(user), &rates)
}

pub fn load_market_fee_total<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    Ok(load_cached::<_, u64>(context, market_fee_total_key(market_id))?.unwrap_or(0))
}

pub fn add_market_fee_total<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    amount: u64,
) -> Result<(), PrecompileError> {
    if amount == 0 {
        return Ok(());
    }
    let total = load_market_fee_total(context, market_id)?
        .checked_add(amount)
        .ok_or_else(|| perp_err("fee total overflow"))?;
    let buf = encode(&total)?;
    store_blob(context, market_fee_total_key(market_id), &buf)
}

// ── ERC-20 balance helpers ─────────────────────────────────────────────────────

pub fn load_erc20_balance<CTX: ContextTr>(
    context: &mut CTX,
    token: Address,
    account: Address,
) -> Result<U256, PrecompileError> {
    let slot = erc20_balance_slot(account);
    // Ensure the token address is loaded into journal state before sload.
    // sload panics if the account is absent from the journal.
    context
        .journal_mut()
        .warm_account(token)
        .map_err(convert_db_err::<CTX::Db>)?;
    let value = context
        .journal_mut()
        .sload(token, slot.into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data;
    Ok(value)
}

pub fn save_erc20_balance<CTX: ContextTr>(
    context: &mut CTX,
    token: Address,
    account: Address,
    balance: U256,
) -> Result<(), PrecompileError> {
    let slot = erc20_balance_slot(account);
    // Ensure the token address is loaded into journal state before sstore.
    context
        .journal_mut()
        .warm_account(token)
        .map_err(convert_db_err::<CTX::Db>)?;
    context
        .journal_mut()
        .sstore(token, slot.into(), balance)
        .map_err(convert_db_err::<CTX::Db>)?;
    // Mark the token account as touched so its storage changes are included in
    // the BundleState transition.  Without this, apply_account_state() skips
    // untouched accounts and the sstore above is silently dropped from the DB commit.
    context.journal_mut().touch_account(token);
    Ok(())
}

// ── PerpPosition ──────────────────────────────────────────────────────────────

pub fn load_position<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
) -> Result<PerpPosition, PrecompileError> {
    Ok(load_cached::<_, PerpPosition>(context, position_key(user, market_id))?.unwrap_or_default())
}

pub fn save_position<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    pos: &PerpPosition,
) -> Result<(), PrecompileError> {
    save_cached(context, position_key(user, market_id), pos)
}

// ── Order entry lists (per-user per-market) ───────────────────────────────────

pub fn load_buy_orders<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
) -> Result<Vec<OrderEntry>, PrecompileError> {
    Ok(load_cached::<_, Vec<OrderEntry>>(context, user_buy_orders_key(user, market_id))?
        .unwrap_or_default())
}

pub fn save_buy_orders<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    entries: &[OrderEntry],
) -> Result<(), PrecompileError> {
    // Cache the owned Vec; msgpack-encoding a `&[T]` and a `&Vec<T>` is byte-identical (both a
    // sequence), so the commitment stream is unchanged.
    save_cached(context, user_buy_orders_key(user, market_id), &entries.to_vec())
}

pub fn load_sell_orders<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
) -> Result<Vec<OrderEntry>, PrecompileError> {
    Ok(load_cached::<_, Vec<OrderEntry>>(context, user_sell_orders_key(user, market_id))?
        .unwrap_or_default())
}

pub fn save_sell_orders<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    entries: &[OrderEntry],
) -> Result<(), PrecompileError> {
    save_cached(context, user_sell_orders_key(user, market_id), &entries.to_vec())
}

/// In-place mutate the user's buy-order list (#21 靶子2): if it's already in the overlay, run `f`
/// on the live `&mut Vec` (one undo snapshot, no load/store clone round-trip); otherwise load it
/// (cache/cold) → run `f` → store as a deferred struct. `f`'s return value passes through (e.g. the
/// recomputed reservation, computed inside the borrow so it sees the post-mutation list). The result
/// is byte-identical to load→modify→`save_buy_orders` since the block-end ser fn is the same msgpack.
pub fn mutate_buy_orders<CTX: ContextTr, R>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    f: impl FnOnce(&mut Vec<OrderEntry>) -> R,
) -> Result<R, PrecompileError> {
    let key = user_buy_orders_key(user, market_id);
    if context.journal_mut().perp_contains_struct(key) {
        return Ok(context
            .journal_mut()
            .perp_with_struct_mut::<Vec<OrderEntry>, R>(key, f));
    }
    let mut entries: Vec<OrderEntry> = load_cached(context, key)?.unwrap_or_default();
    let r = f(&mut entries);
    save_cached(context, key, &entries)?;
    Ok(r)
}

/// In-place mutate the user's sell-order list (#21 靶子2). See [`mutate_buy_orders`].
pub fn mutate_sell_orders<CTX: ContextTr, R>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    f: impl FnOnce(&mut Vec<OrderEntry>) -> R,
) -> Result<R, PrecompileError> {
    let key = user_sell_orders_key(user, market_id);
    if context.journal_mut().perp_contains_struct(key) {
        return Ok(context
            .journal_mut()
            .perp_with_struct_mut::<Vec<OrderEntry>, R>(key, f));
    }
    let mut entries: Vec<OrderEntry> = load_cached(context, key)?.unwrap_or_default();
    let r = f(&mut entries);
    save_cached(context, key, &entries)?;
    Ok(r)
}

// ── Full Order struct ─────────────────────────────────────────────────────────

pub fn load_order<CTX: ContextTr>(
    context: &mut CTX,
    order_id: &[u8; 32],
) -> Result<Option<Order>, PrecompileError> {
    load_cached::<_, Order>(context, order_key(order_id))
}

pub fn save_order<CTX: ContextTr>(
    context: &mut CTX,
    order_id: &[u8; 32],
    order: &Order,
) -> Result<(), PrecompileError> {
    save_cached(context, order_key(order_id), order)
}

// ── Global trade counter ──────────────────────────────────────────────────────

/// Atomically increment and return the *current* trade ID for a market, then store the
/// incremented value.  Returns 0 for the first trade in that market, 1 for the second, etc.
/// Trade IDs are per-market so that indexers can use them directly as `fromId` cursors.
pub fn next_trade_id<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let current: u64 = load_cached::<_, u64>(context, trade_count_key(market_id))?.unwrap_or(0);
    save_cached(context, trade_count_key(market_id), &(current + 1))?;
    Ok(current)
}

// ── User nonce ────────────────────────────────────────────────────────────────

pub fn load_user_nonce<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
) -> Result<u64, PrecompileError> {
    Ok(load_cached::<_, u64>(context, user_nonce_key(user))?.unwrap_or(0))
}

pub fn save_user_nonce<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    nonce: u64,
) -> Result<(), PrecompileError> {
    save_cached(context, user_nonce_key(user), &nonce)
}

// ── Market ────────────────────────────────────────────────────────────────────

pub fn load_market<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<Option<Market>, PrecompileError> {
    load_cached::<_, Market>(context, market_key(market_id))
}

pub fn save_market<CTX: ContextTr>(
    context: &mut CTX,
    market: &Market,
) -> Result<(), PrecompileError> {
    save_cached(context, market_key(market.market_id), market)
}

// ── Mark price ────────────────────────────────────────────────────────────────

pub fn load_mark_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    Ok(load_cached::<_, u64>(context, mark_price_key(market_id))?.unwrap_or(0))
}

pub fn save_mark_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    save_cached(context, mark_price_key(market_id), &price)
}

// ── Open interest ─────────────────────────────────────────────────────────────

pub fn load_open_interest<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    Ok(load_cached::<_, u64>(context, open_interest_key(market_id))?.unwrap_or(0))
}

pub fn save_open_interest<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    oi: u64,
) -> Result<(), PrecompileError> {
    save_cached(context, open_interest_key(market_id), &oi)
}

// ── Order book: price level lists ─────────────────────────────────────────────
// REMOVED (#21 perp-parallel): the sorted bid_prices/ask_prices Vec was a single per-side key that
// every place-at-a-new-price + level-emptying RMW'd, serializing the whole side. The book now
// discovers prices by tick-walk from best (`next_ask_at_or_above` / `next_bid_at_or_below`), so
// different-price levels are independent keys with no shared list.

// ── Order book: FIFO queue at a price level ───────────────────────────────────

/// Packs an order-id FIFO queue as raw concatenated 32-byte ids — the most compact form for a
/// homogeneous fixed-size-id list, with zero msgpack overhead (P4/#20). An empty queue packs to an
/// empty buf (which `store_blob` treats as a delete; `load_*_level` reads it back as `vec![]`).
fn pack_order_ids(queue: &[[u8; 32]]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(queue.len() * 32);
    for id in queue {
        buf.extend_from_slice(id);
    }
    buf
}

/// Inverse of [`pack_order_ids`]: chunks a raw blob into 32-byte ids.
fn unpack_order_ids(buf: &[u8]) -> Result<Vec<[u8; 32]>, PrecompileError> {
    if buf.len() % 32 != 0 {
        return Err(perp_err("corrupt order-id queue blob"));
    }
    Ok(buf
        .chunks_exact(32)
        .map(|chunk| {
            let mut id = [0u8; 32];
            id.copy_from_slice(chunk);
            id
        })
        .collect())
}

/// Block-end serializer for a level FIFO held as a deferred `Struct` (#21): produces the SAME raw
/// packed bytes as the old `store_blob(pack_order_ids(..))` path, so the off-trie blob (and the
/// commitment) is byte-identical — only the serialization timing moves to block end.
fn ser_level(v: &dyn core::any::Any) -> Vec<u8> {
    pack_order_ids(
        v.downcast_ref::<Vec<[u8; 32]>>()
            .expect("perp ser_level: level-queue type mismatch (bug)"),
    )
}

/// Clones a deferred level-FIFO `Struct` (keeps the journal overlay `Clone`).
fn clone_level(v: &dyn core::any::Any) -> std::boxed::Box<dyn core::any::Any + Send + Sync> {
    std::boxed::Box::new(
        v.downcast_ref::<Vec<[u8; 32]>>()
            .expect("perp clone_level: level-queue type mismatch (bug)")
            .clone(),
    )
}

/// Reads a level FIFO queue. #21: if the queue is in the overlay as a deferred `Struct`
/// (`Vec<[u8;32]>`), clone it directly (no serialize→unpack round-trip). Otherwise consult the
/// #14 cold-read cache, then fall back to the committed byte store (cold read + unpack) and cache
/// the unpacked queue.
///
/// Caching the read collapses repeated reads of a level only READ (not yet written) this block —
/// e.g. the FOK feasibility pre-scan followed by the match pass re-reading the same levels — from
/// a re-fetch + re-unpack down to a clone. Safe: the write-overlay (`perp_get_struct`) is checked
/// FIRST and shadows this entry; `store` invalidates the cache per-key; a revert clears the whole
/// cache; and the cache is excluded from the block delta, so it can never affect the commitment.
fn load_level_cached<CTX: ContextTr>(
    context: &mut CTX,
    key: B256,
) -> Result<Vec<[u8; 32]>, PrecompileError> {
    if let Some(q) = context
        .journal_mut()
        .perp_with_struct::<Vec<[u8; 32]>, _>(key, |q| q.clone())
    {
        return Ok(q);
    }
    if let Some(any) = context.journal_mut().perp_cache_get(key) {
        if let Some(q) = any.downcast_ref::<Vec<[u8; 32]>>() {
            return Ok(q.clone());
        }
    }
    let buf = load_blob(context, key)?;
    if buf.is_empty() {
        return Ok(Vec::new());
    }
    let queue = unpack_order_ids(&buf)?;
    context
        .journal_mut()
        .perp_cache_put(key, std::boxed::Box::new(queue.clone()));
    Ok(queue)
}

/// Reads a bid level FIFO.
pub fn load_bid_level<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<Vec<[u8; 32]>, PrecompileError> {
    load_level_cached(context, bid_level_key(market_id, price))
}

/// Writes a bid level FIFO. #21: stores the `Vec` as a deferred `Struct` (packed ONCE at block end
/// by [`ser_level`]) instead of re-packing the whole blob per op — byte-identical final bytes.
pub fn save_bid_level<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
    queue: &[[u8; 32]],
) -> Result<(), PrecompileError> {
    context.journal_mut().perp_store_struct(
        bid_level_key(market_id, price),
        std::boxed::Box::new(queue.to_vec()),
        ser_level,
        clone_level,
    );
    Ok(())
}

/// Reads an ask level FIFO.
pub fn load_ask_level<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<Vec<[u8; 32]>, PrecompileError> {
    load_level_cached(context, ask_level_key(market_id, price))
}

pub fn save_ask_level<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
    queue: &[[u8; 32]],
) -> Result<(), PrecompileError> {
    context.journal_mut().perp_store_struct(
        ask_level_key(market_id, price),
        std::boxed::Box::new(queue.to_vec()),
        ser_level,
        clone_level,
    );
    Ok(())
}

// ── Order book helpers ────────────────────────────────────────────────────────
// (#21 perp-parallel: insert_/remove_bid/ask_price removed — no sorted price list to maintain.
// Resting just pushes to the per-(market,side,price) level key; emptying just leaves an empty level.)

/// Append `order_id` to the FIFO queue at the given bid price level. #21: if the level is already
/// in the overlay, append IN PLACE (one undo snapshot, no load/store clone round-trip); otherwise
/// materialize it once (committed bytes / absent) and store as a deferred `Struct`.
pub fn push_bid_order<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
    order_id: [u8; 32],
) -> Result<(), PrecompileError> {
    let key = bid_level_key(market_id, price);
    if context.journal_mut().perp_contains_struct(key) {
        context
            .journal_mut()
            .perp_with_struct_mut::<Vec<[u8; 32]>, ()>(key, |q| q.push(order_id));
        return Ok(());
    }
    let mut queue = load_bid_level(context, market_id, price)?;
    queue.push(order_id);
    save_bid_level(context, market_id, price, &queue)
}

/// Append `order_id` to the FIFO queue at the given ask price level. See [`push_bid_order`].
pub fn push_ask_order<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
    order_id: [u8; 32],
) -> Result<(), PrecompileError> {
    let key = ask_level_key(market_id, price);
    if context.journal_mut().perp_contains_struct(key) {
        context
            .journal_mut()
            .perp_with_struct_mut::<Vec<[u8; 32]>, ()>(key, |q| q.push(order_id));
        return Ok(());
    }
    let mut queue = load_ask_level(context, market_id, price)?;
    queue.push(order_id);
    save_ask_level(context, market_id, price, &queue)
}

// ── Best bid / ask cache ──────────────────────────────────────────────────────
// Stored as a single u64 per market.  0 means "no orders on that side".
// Kept in sync with the sorted price lists so callers can avoid loading the
// full list just for a PostOnly check or a quick spread query.

pub fn load_best_bid<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    Ok(load_cached::<_, u64>(context, best_bid_key(market_id))?.unwrap_or(0))
}

pub fn save_best_bid<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    save_cached(context, best_bid_key(market_id), &price)
}

pub fn load_best_ask<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    Ok(load_cached::<_, u64>(context, best_ask_key(market_id))?.unwrap_or(0))
}

pub fn save_best_ask<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    save_cached(context, best_ask_key(market_id), &price)
}

/// Re-derive best_bid after an op that may have emptied the top bid level. With the price list gone
/// (#21), we tick-walk DOWN from the OLD best to the highest still-non-empty bid level. This is
/// correct + bounded because a removal can only LOWER the best (movers set best directly when a
/// better price arrives), so the new best is at-or-below the old one; capped by [`next_bid_at_or_below`].
pub fn refresh_best_bid<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let from = load_best_bid(context, market_id)?;
    let tick = load_market(context, market_id)?.map(|m| m.tick_size).unwrap_or(0);
    let best = if from == 0 {
        0
    } else {
        next_bid_at_or_below(context, market_id, from, tick)?.unwrap_or(0)
    };
    save_best_bid(context, market_id, best)?;
    Ok(best)
}

/// Re-derive best_ask after an op that may have emptied the top ask level — tick-walk UP from the
/// OLD best to the lowest still-non-empty ask level. See [`refresh_best_bid`].
pub fn refresh_best_ask<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let from = load_best_ask(context, market_id)?;
    let tick = load_market(context, market_id)?.map(|m| m.tick_size).unwrap_or(0);
    let best = if from == 0 {
        0
    } else {
        next_ask_at_or_above(context, market_id, from, tick)?.unwrap_or(0)
    };
    save_best_ask(context, market_id, best)?;
    Ok(best)
}

// ── Tick-walk price discovery (#21 perp-parallel: replaces the sorted bid_prices/ask_prices Vec) ──
// The sorted per-side price list was a single per-side key that EVERY place-at-a-new-price and every
// level-emptying RMW'd → it serialized the whole side (the BookSideLock bottleneck). We drop it: a
// level is found purely from the per-(market,side,price) FIFO key, and "the next price" is discovered
// by walking the tick grid from a starting price. Different-price levels are then independent keys
// (no shared list) → different-price place/cancel can run in parallel.

// Discovery (matching sweeps + best-refresh) runs ONLY in serial contexts (the parallel driver's
// barrier / the serial EVM pass — every caller of these + refresh_best is serial), so it is free to
// be a plain price-grid walk. It is UNBOUNDED over the OCCUPIED range — the ask walk stops at the
// market's max_price, the bid walk at the tick floor — so a level ANY distance away is always found.
//
// (#21: this REPLACES the bounded TICK_WALK_CAP=4096. The cap wrongly treated a level > CAP ticks away
// as empty on a sparse/wide book -> takers stopped sweeping early, best-refresh zeroed best and
// orphaned the far level -> cross-level wrong fills + non-conservation. See perpdex-parallel-fail.)
//
// PERF: an EMPTY / sparse side costs O(range/tick) probes per discovery; pureChurn clusters orders
// near best so hops are 1-2 ticks and this is cheap. The deferred sparse-book optimization is a coarse
// per-region occupancy index that SKIPS empty gaps — and it MUST be a commutative signed-delta counter
// (atomic add, undo subtracts its own delta), NOT a snapshot-restored aggregate value, or a concurrent
// revert in the parallel batch clobbers a sibling slot's committed bump (lost update).

/// Lowest non-empty ASK level at a price `>= from`, walking UP the tick grid from `from` (inclusive)
/// in `tick` steps, up to the market's `max_price`. `Ok(None)` = no ask liquidity at-or-above `from`.
/// `from` must be tick-aligned (all resting prices are). `tick == 0` (mis-configured market) -> `None`.
pub fn next_ask_at_or_above<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    from: u64,
    tick: u64,
) -> Result<Option<u64>, PrecompileError> {
    if tick == 0 {
        return Ok(None);
    }
    // Bound the walk by max_price (no order can rest above it — enforced at placement), so an empty
    // ask side terminates instead of walking to u64::MAX.
    let max_price = load_market(context, market_id)?
        .map(|m| m.max_price)
        .unwrap_or(0);
    let mut p = from;
    while p <= max_price {
        if !load_ask_level(context, market_id, p)?.is_empty() {
            return Ok(Some(p));
        }
        p = match p.checked_add(tick) {
            Some(x) => x,
            None => return Ok(None),
        };
    }
    Ok(None)
}

/// Highest non-empty BID level at a price `<= from`, walking DOWN the tick grid from `from`
/// (inclusive) to the tick floor. `Ok(None)` = no bid liquidity at-or-below `from`.
/// See [`next_ask_at_or_above`].
pub fn next_bid_at_or_below<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    from: u64,
    tick: u64,
) -> Result<Option<u64>, PrecompileError> {
    if tick == 0 {
        return Ok(None);
    }
    let mut p = from;
    loop {
        if !load_bid_level(context, market_id, p)?.is_empty() {
            return Ok(Some(p));
        }
        if p < tick {
            return Ok(None); // below the lowest representable tick → no more bids
        }
        p -= tick;
    }
}

// ── API key (ed25519 signed orders) ──────────────────────────────────────────

pub fn load_api_key<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    key_id: u8,
) -> Result<Option<ApiKey>, PrecompileError> {
    let buf = load_blob(context, api_key_key(user, key_id))?;
    if buf.is_empty() {
        return Ok(None);
    }
    Ok(Some(decode(&buf)?))
}

pub fn save_api_key<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    key_id: u8,
    key: ApiKey,
) -> Result<(), PrecompileError> {
    let buf = encode(&key)?;
    store_blob(context, api_key_key(user, key_id), &buf)?;
    // Track this key_id in the user's id list (deduplicated).
    let mut ids = load_api_key_ids(context, user)?;
    if !ids.contains(&key_id) {
        ids.push(key_id);
        ids.sort_unstable();
        save_api_key_ids(context, user, &ids)?;
    }
    Ok(())
}

pub fn delete_api_key<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    key_id: u8,
) -> Result<(), PrecompileError> {
    store_blob(context, api_key_key(user, key_id), &[])?;
    let mut ids = load_api_key_ids(context, user)?;
    ids.retain(|&id| id != key_id);
    save_api_key_ids(context, user, &ids)
}

pub fn load_api_key_ids<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
) -> Result<Vec<u8>, PrecompileError> {
    let buf = load_blob(context, api_key_ids_key(user))?;
    if buf.is_empty() {
        return Ok(vec![]);
    }
    decode(&buf)
}

fn save_api_key_ids<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    ids: &[u8],
) -> Result<(), PrecompileError> {
    let buf = encode(&ids)?;
    store_blob(context, api_key_ids_key(user), &buf)
}

// ── Role addresses ────────────────────────────────────────────────────────────

pub fn load_oracle<CTX: ContextTr>(context: &mut CTX) -> Result<Address, PrecompileError> {
    let buf = load_blob(context, oracle_key())?;
    if buf.is_empty() {
        return Ok(Address::ZERO);
    }
    decode(&buf)
}

pub fn save_oracle<CTX: ContextTr>(
    context: &mut CTX,
    oracle: Address,
) -> Result<(), PrecompileError> {
    let buf = encode(&oracle)?;
    store_blob(context, oracle_key(), &buf)
}

pub fn load_market_manager<CTX: ContextTr>(context: &mut CTX) -> Result<Address, PrecompileError> {
    let buf = load_blob(context, market_manager_key())?;
    if buf.is_empty() {
        return Ok(Address::ZERO);
    }
    decode(&buf)
}

pub fn save_market_manager<CTX: ContextTr>(
    context: &mut CTX,
    manager: Address,
) -> Result<(), PrecompileError> {
    let buf = encode(&manager)?;
    store_blob(context, market_manager_key(), &buf)
}

// ── Index price state ─────────────────────────────────────────────────────────

pub fn load_index_price_state<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<IndexPriceState, PrecompileError> {
    let buf = load_blob(context, index_price_state_key(market_id))?;
    if buf.is_empty() {
        return Ok(IndexPriceState::default());
    }
    decode(&buf)
}

pub fn save_index_price_state<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    state: &IndexPriceState,
) -> Result<(), PrecompileError> {
    let buf = encode(state)?;
    store_blob(context, index_price_state_key(market_id), &buf)
}

// ── Price mid window (30s MA basis input) ─────────────────────────────────────

pub fn load_index_price_history<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<IndexPriceHistory, PrecompileError> {
    let buf = load_blob(context, index_price_history_key(market_id))?;
    if buf.is_empty() {
        return Ok(IndexPriceHistory::default());
    }
    decode(&buf)
}

pub fn save_index_price_history<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    history: &IndexPriceHistory,
) -> Result<(), PrecompileError> {
    let buf = encode(history)?;
    store_blob(context, index_price_history_key(market_id), &buf)
}

pub fn load_price_basis_window<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<PriceBasisWindow, PrecompileError> {
    let buf = load_blob(context, price_basis_window_key(market_id))?;
    if buf.is_empty() {
        return Ok(PriceBasisWindow::default());
    }
    decode(&buf)
}

pub fn save_price_basis_window<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    window: &PriceBasisWindow,
) -> Result<(), PrecompileError> {
    let buf = encode(window)?;
    store_blob(context, price_basis_window_key(market_id), &buf)
}

// ── Last traded price (contract price) ───────────────────────────────────────

pub fn load_last_traded_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, last_traded_price_key(market_id))?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn save_last_traded_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    let buf = encode(&price)?;
    store_blob(context, last_traded_price_key(market_id), &buf)
}

// ── Funding state ─────────────────────────────────────────────────────────────

pub fn load_funding_state<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<FundingState, PrecompileError> {
    let buf = load_blob(context, funding_state_key(market_id))?;
    if buf.is_empty() {
        return Ok(FundingState::default());
    }
    decode(&buf)
}

pub fn save_funding_state<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    state: &FundingState,
) -> Result<(), PrecompileError> {
    let buf = encode(state)?;
    store_blob(context, funding_state_key(market_id), &buf)
}

// ── Insurance Fund ────────────────────────────────────────────────────────────

pub fn load_insurance_fund<CTX: ContextTr>(context: &mut CTX) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, insurance_fund_key())?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn save_insurance_fund<CTX: ContextTr>(
    context: &mut CTX,
    balance: u64,
) -> Result<(), PrecompileError> {
    let buf = encode(&balance)?;
    store_blob(context, insurance_fund_key(), &buf)
}

/// Absorb up to `deficit` from the insurance fund.
/// Returns `(absorbed, remaining_deficit)`.
/// If the fund covers everything, `remaining_deficit` is 0.
/// If the fund is insufficient, it is drained to zero and `remaining_deficit` > 0.
pub fn absorb_from_insurance_fund<CTX: ContextTr>(
    context: &mut CTX,
    deficit: u64,
) -> Result<(u64, u64), PrecompileError> {
    let balance = load_insurance_fund(context)?;
    let absorbed = deficit.min(balance);
    let remaining = deficit - absorbed;
    save_insurance_fund(context, balance - absorbed)?;
    Ok((absorbed, remaining))
}

// ── Premium index accumulator ─────────────────────────────────────────────────

pub fn load_premium_accumulator<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<PremiumIndexAccumulator, PrecompileError> {
    let buf = load_blob(context, premium_accumulator_key(market_id))?;
    if buf.is_empty() {
        return Ok(PremiumIndexAccumulator::default());
    }
    decode(&buf)
}

pub fn save_premium_accumulator<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    acc: &PremiumIndexAccumulator,
) -> Result<(), PrecompileError> {
    let buf = encode(acc)?;
    store_blob(context, premium_accumulator_key(market_id), &buf)
}

#[cfg(test)]
mod commitment_tests {
    use super::*;
    use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
    use database::InMemoryDB;
    use primitives::hardfork::SpecId;

    type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

    /// Mirror of `perp_dex::trading::tests::make_ctx`: build a journal-backed context and warm the
    /// PERP_DEX account so the on-trie commitment sload/sstore have an account to operate on.
    fn new_test_ctx() -> TestCtx {
        let db = InMemoryDB::default();
        let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
        JournalTr::load_account(ctx.journal_mut(), PERP_DEX_ADDRESS).unwrap();
        ctx
    }

    fn read_commitment<CTX: ContextTr>(ctx: &mut CTX) -> U256 {
        ctx.journal_mut()
            .sload(PERP_DEX_ADDRESS, commitment_slot().into())
            .unwrap()
            .data
    }







    /// #16d: `compute_block_commitment` hashes the block NET delta once. Cross-checked against an
    /// independent BTreeMap reference (sorted, v3 framing) — a different impl than production's
    /// HashMap+sort, so agreement is a genuine check. Includes a deleted key (empty value, len 0).
    /// `finalize_block_commitment` writes the result to the 0x1003 slot; an empty delta is a no-op.
    #[test]
    fn block_commitment_over_net_delta() {
        let (k1, b1) = (B256::with_last_byte(1), vec![0xAAu8]);
        let (k2, b2) = (B256::with_last_byte(2), vec![0xBBu8, 0xCC]);
        let (k3, b3) = (B256::with_last_byte(3), Vec::<u8>::new()); // deleted key, framed len 0
        let mut delta: HashMap<B256, Vec<u8>> = HashMap::default();
        delta.insert(k1, b1.clone());
        delta.insert(k2, b2.clone());
        delta.insert(k3, b3.clone());

        // Independent reference: BTreeMap (sorted), v3 framing.
        let mut sorted: std::collections::BTreeMap<B256, Vec<u8>> = std::collections::BTreeMap::new();
        sorted.insert(k1, b1);
        sorted.insert(k2, b2);
        sorted.insert(k3, b3);
        let mut p = Vec::new();
        p.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
        p.push(BLOCK_COMMITMENT_VERSION);
        for (k, v) in &sorted {
            p.extend_from_slice(k.as_slice());
            p.extend_from_slice(&(v.len() as u32).to_be_bytes());
            p.extend_from_slice(v);
        }
        let expected = U256::from_be_bytes(*blake3::hash(&p).as_bytes());
        assert_eq!(compute_block_commitment(U256::ZERO, &delta), expected);

        // finalize_block_commitment writes it to the on-trie slot.
        let mut ctx = new_test_ctx();
        finalize_block_commitment(&mut ctx, &delta).unwrap();
        assert_eq!(read_commitment(&mut ctx), expected);

        // Empty delta is a no-op (slot stays at genesis 0).
        let mut ctx2 = new_test_ctx();
        finalize_block_commitment(&mut ctx2, &HashMap::default()).unwrap();
        assert_eq!(read_commitment(&mut ctx2), U256::ZERO);
    }

    /// #16d end-to-end: `store_blob` writes accumulate in the overlay; the on-trie slot stays
    /// untouched until the block-end finalize folds the net delta ONCE (mirroring the executor:
    /// `take_perp_delta` → `finalize_block_commitment`). A key overwritten in-block contributes
    /// only its final value.
    #[test]
    fn block_commitment_from_overlay_writes() {
        let mut ctx = new_test_ctx();
        let (k1, b1) = (B256::with_last_byte(0x11), vec![0x01u8, 0x02]);
        let (k2, b2) = (B256::with_last_byte(0x22), vec![0x03u8]);
        store_blob(&mut ctx, k1, &b1).unwrap();
        store_blob(&mut ctx, k2, &b2).unwrap();
        store_blob(&mut ctx, k1, &[0x09]).unwrap(); // overwrite k1; net delta keeps the last value
        // No per-call flush: the on-trie slot stays at genesis until block end.
        assert_eq!(read_commitment(&mut ctx), U256::ZERO, "slot must not move until block end");

        let delta = ctx.journal_mut().take_perp_delta();
        let expected = compute_block_commitment(U256::ZERO, &delta);
        finalize_block_commitment(&mut ctx, &delta).unwrap();
        assert_eq!(read_commitment(&mut ctx), expected);
        assert_ne!(expected, U256::ZERO);
    }
}

#[cfg(test)]
mod size_probe_tests {
    use super::*;
    use crate::perp_dex::types::{
        OrderStatus, OrderType, Side, TimeInForce, PRICE_BASIS_WINDOW_SIZE,
    };

    #[test]
    fn probe_encoded_sizes() {
        // Order — realistic BTC-ish values
        let order = Order {
            owner: [0xAB; 20],
            market_id: 1,
            side: Side::Buy,
            price: 65_432_10,        // 7 digits
            quantity: 150_000_000,   // 1.5 BTC @ 8 decimals
            filled: 50_000_000,
            order_type: OrderType::Limit,
            tif: TimeInForce::Gtc,
            status: OrderStatus::PartiallyFilled,
        };
        let buf = encode(&order).unwrap();
        println!("Order: {} bytes; hex={}", buf.len(), primitives::hex::encode(&buf));

        let entry = OrderEntry {
            order_id: [0xCD; 32],
            price: 65_432_10,
            amount: 150_000_000,
            maker_fee_bps: 2,
        };
        println!("OrderEntry x1 (in vec): {} bytes", encode(&vec![entry]).unwrap().len());
        println!("OrderEntry x5: {} bytes", encode(&vec![entry; 5]).unwrap().len());
        println!("OrderEntry x20: {} bytes", encode(&vec![entry; 20]).unwrap().len());
        println!("OrderEntry single hex={}", primitives::hex::encode(encode(&entry).unwrap()));

        let prices: Vec<u64> = (0..1u64).map(|i| 65_000_00 + i * 10).collect();
        println!("bid_prices x1: {} bytes", encode(&prices).unwrap().len());
        let prices: Vec<u64> = (0..10u64).map(|i| 65_000_00 + i * 10).collect();
        println!("bid_prices x10: {} bytes", encode(&prices).unwrap().len());
        let prices: Vec<u64> = (0..100u64).map(|i| 65_000_00 + i * 10).collect();
        println!("bid_prices x100: {} bytes", encode(&prices).unwrap().len());

        let q: Vec<[u8; 32]> = vec![[0xEF; 32]; 1];
        println!("level queue x1: {} bytes", encode(&q).unwrap().len());
        let q: Vec<[u8; 32]> = vec![[0xEF; 32]; 5];
        println!("level queue x5: {} bytes", encode(&q).unwrap().len());
        println!("level queue single elem hex={}", primitives::hex::encode(encode(&[0xEFu8; 32]).unwrap()));

        let pos = PerpPosition {
            amount: 150_000_000,
            v_quote_balance: -98_148_315,
            margin: 9_814_831,
            margin_reserved: 5_000_000,
            margin_reserved_notional: 50_000_000,
            buy_side_margin_reserved: 5_000_000,
            buy_side_reserved_notional: 50_000_000,
            sell_side_margin_reserved: 1_000_000,
            sell_side_reserved_notional: 10_000_000,
            fee_reserved: 10_000,
            leverage: 10,
            last_funding_index: 123_456_789_012_345i128,
        };
        let buf = encode(&pos).unwrap();
        println!("PerpPosition: {} bytes; hex={}", buf.len(), primitives::hex::encode(&buf));
        println!("PerpPosition default: {} bytes", encode(&PerpPosition::default()).unwrap().len());

        let acct = UserAccount {
            usdc_balance: "123456789000000000000".into(), // 21-digit decimal string
            perp_wallet_balance: 1_234_567_890,
        };
        let buf = encode(&acct).unwrap();
        println!("UserAccount: {} bytes; hex={}", buf.len(), primitives::hex::encode(&buf));

        let market = Market {
            market_id: 1,
            base_decimals: 8,
            price_decimals: 2,
            tick_size: 10,
            step_size: 1000,
            min_quantity: 1000,
            max_quantity: 10_000_000_000,
            max_price: 100_000_000,
            price_update_interval: 1,
            active: true,
            funding_interval: 28_800,
            interest_rate: 100,
            liquidation_fee_rate_bps: 50,
        };
        let buf = encode(&market).unwrap();
        println!("Market: {} bytes", buf.len());

        let fs = FundingState {
            last_funding_rate: 125,
            next_funding_ts: 1_750_000_000,
            cumulative_funding_index: 9_876_543_210_123i128,
        };
        println!("FundingState: {} bytes", encode(&fs).unwrap().len());

        let acc = PremiumIndexAccumulator {
            weighted_sum: 123_456_789_012i128,
            sample_count: 28_000,
            epoch_start_ts: 1_750_000_000,
            last_pi: -1234,
            last_sample_ts: 1_750_000_123,
        };
        println!("PremiumIndexAccumulator: {} bytes", encode(&acc).unwrap().len());

        let mut window = PriceBasisWindow::default();
        for i in 0..PRICE_BASIS_WINDOW_SIZE as u64 {
            let _ = window.record_observation(1_750_000_000 + i, 65_000_00 + i);
        }
        println!("PriceBasisWindow full: {} bytes", encode(&window).unwrap().len());
        println!("PriceBasisWindow empty: {} bytes", encode(&PriceBasisWindow::default()).unwrap().len());

        let mut hist = IndexPriceHistory::default();
        for i in 0..32u64 {
            hist.push(
                IndexPriceState { index_price: 65_000_00 + i, timestamp: 1_750_000_000 + i },
                32,
            );
        }
        println!("IndexPriceHistory x32: {} bytes", encode(&hist).unwrap().len());

        println!("UserFeeRates: {} bytes", encode(&UserFeeRates { maker_fee_bps: 2, taker_fee_bps: 5 }).unwrap().len());
        println!("u64 scalar (mark price 6_543_210): {} bytes", encode(&6_543_210u64).unwrap().len());
        println!("u64 scalar small (nonce 7): {} bytes", encode(&7u64).unwrap().len());
        println!("Address: {} bytes", encode(&Address::ZERO).unwrap().len());
        println!(
            "ApiKey: {} bytes",
            encode(&ApiKey { pubkey: [9; 32], expiry: 1_750_000_000 }).unwrap().len()
        );
    }
}

/// Permanent guard for the P4/#20 blob encoding (positional msgpack + serde_bytes byte-arrays +
/// serde_repr integer enums + raw-packed order-id queues). The golden commitment test only detects
/// DRIFT (any byte change re-pins it); these `decode(encode(v)) == v` checks catch a SELF-CONSISTENT
/// mis-round-trip on a field the golden scenario never stresses — negative i128s, bytes > 0x7F in
/// fixed arrays, every enum discriminant, defaulted middle fields at extremes. Value-equality is
/// required: for positional encoding `encode∘decode` is a byte-identity, so a byte round-trip would
/// be vacuously true.
#[cfg(test)]
mod encoding_roundtrip_tests {
    use super::*;
    use crate::perp_dex::types::{OrderStatus, OrderType, Side, TimeInForce};

    fn rt<T>(label: &str, v: T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + core::fmt::Debug,
    {
        let buf = encode(&v).unwrap();
        let back: T = decode(&buf).unwrap();
        assert_eq!(v, back, "round-trip mismatch for {label}");
    }

    #[test]
    fn order_every_enum_combo_and_high_byte_owner() {
        for side in [Side::Buy, Side::Sell] {
            for ot in [OrderType::Limit, OrderType::Market] {
                for tif in [
                    TimeInForce::Gtc,
                    TimeInForce::Ioc,
                    TimeInForce::Fok,
                    TimeInForce::PostOnly,
                ] {
                    for status in [
                        OrderStatus::Open,
                        OrderStatus::PartiallyFilled,
                        OrderStatus::Filled,
                        OrderStatus::Cancelled,
                        OrderStatus::Expired,
                    ] {
                        // owner spans the full byte domain incl. > 0x7F.
                        let mut owner = [0u8; 20];
                        for (i, b) in owner.iter_mut().enumerate() {
                            *b = (0x80 + i as u8) ^ 0xA5;
                        }
                        rt(
                            "Order",
                            Order {
                                owner,
                                market_id: u64::MAX,
                                side,
                                price: u64::MAX,
                                quantity: u64::MAX - 1,
                                filled: 1,
                                order_type: ot,
                                tif,
                                status,
                            },
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn order_entry_and_api_key_high_byte_arrays() {
        let id: [u8; 32] = core::array::from_fn(|i| (0xFF - i as u8) ^ 0x3C);
        rt(
            "OrderEntry",
            OrderEntry { order_id: id, price: u64::MAX, amount: u64::MAX, maker_fee_bps: 12_345 },
        );
        rt("ApiKey", ApiKey { pubkey: id, expiry: u64::MAX });
        rt("ApiKey-never-expires", ApiKey { pubkey: [0xFF; 32], expiry: 0 });
    }

    #[test]
    fn perp_position_signed_extremes() {
        rt(
            "PerpPosition",
            PerpPosition {
                amount: i64::MIN,
                v_quote_balance: i64::MAX,
                margin: -1,
                margin_reserved: u64::MAX,
                margin_reserved_notional: u64::MAX,
                buy_side_margin_reserved: 0,
                buy_side_reserved_notional: u64::MAX,
                sell_side_margin_reserved: 7,
                sell_side_reserved_notional: 0,
                fee_reserved: u64::MAX,
                leverage: 20,
                last_funding_index: i128::MIN,
            },
        );
        rt(
            "PerpPosition-pos-i128",
            PerpPosition { last_funding_index: i128::MAX, ..PerpPosition::default() },
        );
    }

    #[test]
    fn market_all_fields_incl_defaulted() {
        rt(
            "Market",
            Market {
                market_id: u64::MAX,
                base_decimals: u32::MAX,
                price_decimals: 18,
                tick_size: u64::MAX,
                step_size: 1,
                min_quantity: 1,
                max_quantity: u64::MAX,
                max_price: u64::MAX,
                price_update_interval: 15,
                active: true,
                funding_interval: 3600,
                interest_rate: i64::MIN,
                liquidation_fee_rate_bps: u32::MAX,
            },
        );
    }

    #[test]
    fn funding_and_premium_signed_i128() {
        rt(
            "FundingState",
            FundingState {
                last_funding_rate: i64::MIN,
                next_funding_ts: u64::MAX,
                cumulative_funding_index: i128::MIN,
            },
        );
        rt(
            "PremiumIndexAccumulator",
            PremiumIndexAccumulator {
                weighted_sum: i128::MIN,
                sample_count: u64::MAX,
                epoch_start_ts: 1,
                last_pi: i64::MIN,
                last_sample_ts: u64::MAX,
            },
        );
    }

    #[test]
    fn enum_out_of_range_discriminant_errors_cleanly() {
        // Each enum round-trips its real discriminants...
        for s in [OrderStatus::Open, OrderStatus::Filled, OrderStatus::Expired] {
            let buf = encode(&s).unwrap();
            assert_eq!(decode::<OrderStatus>(&buf).unwrap(), s);
        }
        // ...and an out-of-range discriminant decodes to a clean Err, never UB/panic.
        // 99 encodes as a 1-byte msgpack positive fixint (0x63); no OrderStatus variant == 99.
        assert!(
            decode::<OrderStatus>(&[0x63]).is_err(),
            "out-of-range enum discriminant must error, not panic/UB"
        );
        assert!(decode::<Side>(&[0x05]).is_err());
    }

    #[test]
    fn order_id_queue_pack_unpack_roundtrip_and_rejects_misaligned() {
        for q in [
            vec![],
            vec![[0xFFu8; 32]],
            vec![[0x00u8; 32], [0x80u8; 32], core::array::from_fn(|i| i as u8)],
        ] {
            let packed = pack_order_ids(&q);
            assert_eq!(packed.len(), q.len() * 32);
            assert_eq!(unpack_order_ids(&packed).unwrap(), q, "pack/unpack must round-trip");
        }
        // A blob whose length is not a multiple of 32 is rejected, never silently truncated.
        for bad_len in [1usize, 31, 33, 63] {
            assert!(unpack_order_ids(&vec![0xABu8; bad_len]).is_err(), "len {bad_len} must error");
        }
    }
}
