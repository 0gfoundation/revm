//! Module containing the [`JournalInner`] that is part of [`crate::Journal`].
use crate::{entry::SelfdestructionRevertStatus, warm_addresses::WarmAddresses};

use super::JournalEntryTr;
use bytecode::Bytecode;
use context_interface::{
    context::{SStoreResult, SelfDestructResult, StateLoad},
    journaled_state::{AccountLoad, JournalCheckpoint, PerpBlob, PerpDelta, TransferError},
};
use core::mem;
use database_interface::Database;
use primitives::{
    hardfork::SpecId::{self, *},
    hash_map::Entry,
    Address, HashMap, Log, StorageKey, StorageValue, B256, KECCAK_EMPTY, U256,
};
use state::{Account, EvmState, EvmStorageSlot, TransientStorage};
use std::vec::Vec;

/// Off-trie PerpDEX section of the journal (the in-memory orderbook overlay, "PerpState").
///
/// This is a second instance of revm's own state/journal model, applied to the PerpDEX
/// orderbook so it gets the SAME checkpoint / revert / commit lifecycle as [`EvmState`] —
/// but it is deliberately never folded into the [`EvmState`] returned by
/// [`JournalInner::finalize`], so it stays out of the state trie. `working` holds ONLY the
/// keys written during the current block (reads pass through to the committed store without
/// caching); `undo` is the per-transaction reversible log. See
/// `docs/perpstate-journal集成方案.md`.
// #16d Phase 2: drops PartialEq/Eq/serde (all unused on the journal types — verified zero usage)
// so `working` can hold deserialized blobs (`Box<dyn Any>`, neither Eq nor serde). Clone is kept;
// `PerpEntry` stays Clone via a per-entry clone fn-pointer.
#[derive(Debug, Clone, Default)]
pub struct PerpSection {
    /// In-block write overlay. Each value is a [`PerpEntry`]: a deferred deserialized blob
    /// (`Struct`, serialized once at block end) or raw bytes (`Bytes`, e.g. via `store_blob`).
    /// `Bytes(empty)` means the key is absent/deleted.
    working: HashMap<B256, PerpEntry>,
    /// Reversible undo log for `working`, mirroring the EVM journal `Vec<ENTRY>`.
    undo: Vec<PerpUndo>,
    /// Block-scoped cache of DESERIALIZED blobs (catalog #14): a pure accelerator over `working`
    /// + cold reads, type-erased so this crate need not know the precompile's blob types. The
    /// precompile's cached load/save helpers populate it; it is invalidated per-key on `store`,
    /// cleared on any revert (`undo_to`) and at the block boundary (`take_delta`), and kept across
    /// txns within a block (like `working`). Transparent to this struct's derives — clones empty,
    /// ignored by equality, skipped by serde — since it is always reconstructible and carries no
    /// semantic state.
    cache: PerpCache,
}

/// A single reversible PerpDEX overlay write: restores `prev` on revert
/// (`None` = the key was absent in `working`, so revert removes it). `prev` is the entry moved out
/// by `HashMap::insert` at write time, so no clone is needed for the undo log.
#[derive(Debug, Clone)]
struct PerpUndo {
    key: B256,
    prev: Option<PerpEntry>,
}

/// One off-trie overlay value (#16d Phase 2). A typed `save_*` write stores the DESERIALIZED blob
/// (`Struct`) and defers serialization to block end; a raw byte write (`store_blob`, e.g. level
/// queues) stores `Bytes`. Both lower to the canonical off-trie bytes via `into_bytes`/`to_bytes`;
/// `Bytes(empty)` is the deleted-key convention. The `ser`/`clone` fn pointers are monomorphized in
/// the precompile (carrying the blob type + msgpack codec), so this crate stays format-agnostic and
/// the entry is `Clone` without cloning through `dyn Any`.
enum PerpEntry {
    Struct {
        val: std::boxed::Box<PerpBlob>,
        ser: fn(&PerpBlob) -> Vec<u8>,
        clone: fn(&PerpBlob) -> std::boxed::Box<PerpBlob>,
    },
    Bytes(Vec<u8>),
}

impl Clone for PerpEntry {
    fn clone(&self) -> Self {
        match self {
            PerpEntry::Struct { val, ser, clone } => PerpEntry::Struct {
                val: clone(val.as_ref()),
                ser: *ser,
                clone: *clone,
            },
            PerpEntry::Bytes(b) => PerpEntry::Bytes(b.clone()),
        }
    }
}

impl core::fmt::Debug for PerpEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PerpEntry::Struct { .. } => f.write_str("PerpEntry::Struct(..)"),
            PerpEntry::Bytes(b) => write!(f, "PerpEntry::Bytes({} bytes)", b.len()),
        }
    }
}

impl PerpEntry {
    /// Lowers to the canonical off-trie bytes, consuming the entry (block-end drain).
    #[inline]
    fn into_bytes(self) -> Vec<u8> {
        match self {
            PerpEntry::Struct { val, ser, .. } => ser(val.as_ref()),
            PerpEntry::Bytes(b) => b,
        }
    }

    /// Lowers to the canonical off-trie bytes by reference (byte-interface read path).
    #[inline]
    fn to_bytes(&self) -> Vec<u8> {
        match self {
            PerpEntry::Struct { val, ser, .. } => ser(val.as_ref()),
            PerpEntry::Bytes(b) => b.clone(),
        }
    }
}

/// Type-erased, block-scoped cache of deserialized off-trie blobs (see [`PerpSection::cache`]).
/// A pure accelerator carrying no semantic state, so it is transparent to [`PerpSection`]'s
/// derives: a clone starts empty (re-warms lazily), equality ignores it, and serde skips it.
#[derive(Default)]
struct PerpCache(HashMap<B256, std::sync::Arc<PerpBlob>>);

impl Clone for PerpCache {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl core::fmt::Debug for PerpCache {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PerpCache({} entries)", self.0.len())
    }
}

impl PerpCache {
    #[inline]
    fn get(&self, key: B256) -> Option<&PerpBlob> {
        self.0.get(&key).map(|b| b.as_ref())
    }
    #[inline]
    fn put(&mut self, key: B256, value: std::sync::Arc<PerpBlob>) {
        self.0.insert(key, value);
    }
    #[inline]
    fn remove(&mut self, key: B256) {
        self.0.remove(&key);
    }
    #[inline]
    fn clear(&mut self) {
        self.0.clear();
    }
}

impl PerpSection {
    /// Reads the overlay for `key` as canonical bytes (in-block writes only); `None` = not written.
    /// A deferred `Struct` is serialized on demand — rare via this path (the typed read uses
    /// `get_struct`); byte-path keys (level queues) are the common case and just clone.
    #[inline]
    fn get_bytes(&self, key: B256) -> Option<Vec<u8>> {
        self.working.get(&key).map(PerpEntry::to_bytes)
    }

    /// Reads a deferred `Struct` overlay value (type-erased) for the typed fast-path read; `None`
    /// if the key is absent or was written as raw `Bytes`.
    #[inline]
    fn get_struct(&self, key: B256) -> Option<&PerpBlob> {
        match self.working.get(&key) {
            Some(PerpEntry::Struct { val, .. }) => Some(val.as_ref()),
            _ => None,
        }
    }

    /// Mutable handle into a deferred `Struct` overlay value for IN-PLACE mutation (catalog #21):
    /// snapshots the current value into the undo log ONCE (so a mid-tx revert restores it), then
    /// returns `&mut dyn Any` for the caller to downcast + mutate the live struct directly — avoiding
    /// the load(clone)→modify→store(clone) round-trip. `None` if the key is absent or was written as
    /// raw `Bytes` (the caller falls back to load + `store_struct`). Each call records one undo
    /// snapshot (a clone), so callers fetch the handle ONCE per logical mutation, not in a loop.
    #[inline]
    fn get_struct_mut(&mut self, key: B256) -> Option<&mut PerpBlob> {
        // Snapshot the pre-mutation value for revert (clone via the entry's clone fn-ptr). The
        // immutable borrow ends with `snapshot`; only `Struct` entries can be mutated in place.
        let snapshot = match self.working.get(&key) {
            Some(PerpEntry::Struct { val, ser, clone }) => PerpEntry::Struct {
                val: clone(val.as_ref()),
                ser: *ser,
                clone: *clone,
            },
            _ => return None,
        };
        self.undo.push(PerpUndo {
            key,
            prev: Some(snapshot),
        });
        // The struct is about to change in place; drop any stale deser-cache entry (mirrors `store_*`).
        self.cache.remove(key);
        match self.working.get_mut(&key) {
            Some(PerpEntry::Struct { val, .. }) => Some(val.as_mut()),
            // Unreachable: matched `Struct` above and `working` was not touched since.
            _ => None,
        }
    }

    /// Writes raw `Bytes` (byte-path writers, e.g. `store_blob`), recording the prior entry (moved
    /// out by `insert`) for revert.
    #[inline]
    fn store_bytes(&mut self, key: B256, value: Vec<u8>) {
        let prev = self.working.insert(key, PerpEntry::Bytes(value));
        self.undo.push(PerpUndo { key, prev });
        // Invalidate the deser cache; a typed cached save re-populates it (write-through).
        self.cache.remove(key);
    }

    /// Writes a deferred `Struct` (typed writers): no serialization now — lowered to bytes once at
    /// `take_delta`. Records the prior entry for revert.
    #[inline]
    fn store_struct(
        &mut self,
        key: B256,
        val: std::boxed::Box<PerpBlob>,
        ser: fn(&PerpBlob) -> Vec<u8>,
        clone: fn(&PerpBlob) -> std::boxed::Box<PerpBlob>,
    ) {
        let prev = self.working.insert(key, PerpEntry::Struct { val, ser, clone });
        self.undo.push(PerpUndo { key, prev });
        self.cache.remove(key);
    }

    /// Reverts overlay writes recorded at or after undo index `i`, in reverse order.
    fn undo_to(&mut self, i: usize) {
        if i >= self.undo.len() {
            return;
        }
        // A revert restores prior overlay values, so any deser-cache entry may now be stale; drop
        // the whole cache (it re-warms lazily). Coarse but always correct.
        self.cache.clear();
        for entry in self.undo.drain(i..).rev() {
            match entry.prev {
                Some(prev) => {
                    self.working.insert(entry.key, prev);
                }
                None => {
                    self.working.remove(&entry.key);
                }
            }
        }
    }

    /// Drains the net in-block writes as a [`PerpDelta`], serializing each entry to canonical bytes
    /// ONCE here — deferred `Struct` writes are serialized at this block boundary (#16d). Clears undo.
    #[inline]
    fn take_delta(&mut self) -> PerpDelta {
        self.undo.clear();
        // Block boundary: the next block must not see this block's cached structs (the committed
        // store changes between blocks via the delta merge).
        self.cache.clear();
        mem::take(&mut self.working)
            .into_iter()
            .map(|(k, e)| (k, e.into_bytes()))
            .collect()
    }

    /// Reads the block-scoped deserialized-blob cache (type-erased). See [`PerpSection::cache`].
    #[inline]
    fn cache_get(&self, key: B256) -> Option<&PerpBlob> {
        self.cache.get(key)
    }

    /// Inserts into the block-scoped deserialized-blob cache.
    #[inline]
    fn cache_put(&mut self, key: B256, value: std::sync::Arc<PerpBlob>) {
        self.cache.put(key, value);
    }
}

/// Inner journal state that contains journal and state changes.
///
/// Spec Id is a essential information for the Journal.
#[derive(Debug, Clone)]
pub struct JournalInner<ENTRY> {
    /// The current state
    pub state: EvmState,
    /// Transient storage that is discarded after every transaction.
    ///
    /// See [EIP-1153](https://eips.ethereum.org/EIPS/eip-1153).
    pub transient_storage: TransientStorage,
    /// Emitted logs
    pub logs: Vec<Log>,
    /// The current call stack depth
    pub depth: usize,
    /// The journal of state changes, one for each transaction
    pub journal: Vec<ENTRY>,
    /// Global transaction id that represent number of transactions executed (Including reverted ones).
    /// It can be different from number of `journal_history` as some transaction could be
    /// reverted or had a error on execution.
    ///
    /// This ID is used in `Self::state` to determine if account/storage is touched/warm/cold.
    pub transaction_id: usize,
    /// The spec ID for the EVM. Spec is required for some journal entries and needs to be set for
    /// JournalInner to be functional.
    ///
    /// If spec is set it assumed that precompile addresses are set as well for this particular spec.
    ///
    /// This spec is used for two things:
    ///
    /// - [EIP-161]: Prior to this EIP, Ethereum had separate definitions for empty and non-existing accounts.
    /// - [EIP-6780]: `SELFDESTRUCT` only in same transaction
    ///
    /// [EIP-161]: https://eips.ethereum.org/EIPS/eip-161
    /// [EIP-6780]: https://eips.ethereum.org/EIPS/eip-6780
    pub spec: SpecId,
    /// Warm addresses containing both coinbase and current precompiles.
    pub warm_addresses: WarmAddresses,
    /// Off-trie PerpDEX overlay + undo log. Journaled like the rest of the state, but never
    /// folded into the [`EvmState`] returned by [`Self::finalize`], so it stays off the trie.
    pub perp: PerpSection,
    /// Per-call PerpDEX commitment log. Each off-trie write appends its framed bytes
    /// (`key ‖ len ‖ value`); the whole log is hashed ONCE at call exit
    /// (`C_new = H(C_prev ‖ ver ‖ log)`) and sstored to the on-trie anchor, instead of sload+sstore
    /// per write. Always empty at call/tx boundaries (flushed or discarded at call exit); its
    /// length is snapshotted into [`JournalCheckpoint`] and truncated back on revert. Never folded
    /// into [`EvmState`].
    pub perp_commitment_log: Vec<u8>,
}

impl<ENTRY: JournalEntryTr> Default for JournalInner<ENTRY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<ENTRY: JournalEntryTr> JournalInner<ENTRY> {
    /// Creates new [`JournalInner`].
    ///
    /// `warm_preloaded_addresses` is used to determine if address is considered warm loaded.
    /// In ordinary case this is precompile or beneficiary.
    pub fn new() -> JournalInner<ENTRY> {
        Self {
            state: HashMap::default(),
            transient_storage: TransientStorage::default(),
            logs: Vec::new(),
            journal: Vec::default(),
            transaction_id: 0,
            depth: 0,
            spec: SpecId::default(),
            warm_addresses: WarmAddresses::new(),
            perp: PerpSection::default(),
            perp_commitment_log: Vec::new(),
        }
    }

    /// Returns the logs
    #[inline]
    pub fn take_logs(&mut self) -> Vec<Log> {
        mem::take(&mut self.logs)
    }

    /// Reads the off-trie PerpDEX overlay for `key`; `None` = not written in this block
    /// (the caller falls through to the committed store, see `JournalTr::perp_load`).
    #[inline]
    pub fn perp_get_overlay(&self, key: B256) -> Option<Vec<u8>> {
        self.perp.get_bytes(key)
    }

    /// Writes an off-trie PerpDEX blob (raw bytes) to the overlay, journaled for revert.
    #[inline]
    pub fn perp_store(&mut self, key: B256, value: Vec<u8>) {
        self.perp.store_bytes(key, value);
    }

    /// Writes a deferred deserialized blob (#16d Phase 2): serialization is deferred to the
    /// block-end `take_perp_delta`. `ser`/`clone` are monomorphized in the precompile, so this
    /// crate stays format-agnostic.
    #[inline]
    pub fn perp_store_struct(
        &mut self,
        key: B256,
        val: std::boxed::Box<PerpBlob>,
        ser: fn(&PerpBlob) -> Vec<u8>,
        clone: fn(&PerpBlob) -> std::boxed::Box<PerpBlob>,
    ) {
        self.perp.store_struct(key, val, ser, clone);
    }

    /// Reads a deferred `Struct` overlay value (type-erased) for the typed fast-path read.
    #[inline]
    pub fn perp_get_struct(&self, key: B256) -> Option<&PerpBlob> {
        self.perp.get_struct(key)
    }

    /// Mutable handle into a deferred `Struct` overlay value for in-place mutation (catalog #21);
    /// snapshots the prior value for revert. `None` if absent or stored as raw bytes.
    #[inline]
    pub fn perp_get_struct_mut(&mut self, key: B256) -> Option<&mut PerpBlob> {
        self.perp.get_struct_mut(key)
    }

    /// Reads the block-scoped deserialized PerpDEX blob cache (catalog #14; type-erased).
    #[inline]
    pub fn perp_cache_get(&self, key: B256) -> Option<&PerpBlob> {
        self.perp.cache_get(key)
    }

    /// Inserts a deserialized PerpDEX blob into the block-scoped read cache.
    #[inline]
    pub fn perp_cache_put(&mut self, key: B256, value: std::sync::Arc<PerpBlob>) {
        self.perp.cache_put(key, value);
    }

    /// Reverts off-trie PerpDEX overlay writes back to the given undo index.
    #[inline]
    pub fn perp_undo_to(&mut self, perp_journal_i: usize) {
        self.perp.undo_to(perp_journal_i);
    }

    /// Drains the block's net off-trie PerpDEX writes ([`PerpDelta`]) and clears the undo log.
    #[inline]
    pub fn take_perp_delta(&mut self) -> PerpDelta {
        self.perp.take_delta()
    }

    /// Appends framed bytes for one off-trie write to the per-call PerpDEX commitment log.
    #[inline]
    pub fn perp_fold_append(&mut self, bytes: &[u8]) {
        self.perp_commitment_log.extend_from_slice(bytes);
    }

    /// Takes (clears) the per-call PerpDEX commitment log for the call-exit hash.
    #[inline]
    pub fn perp_fold_take_log(&mut self) -> Vec<u8> {
        mem::take(&mut self.perp_commitment_log)
    }

    /// Current length of the per-call PerpDEX commitment log.
    #[inline]
    pub fn perp_fold_log_len(&self) -> usize {
        self.perp_commitment_log.len()
    }

    /// Prepare for next transaction, by committing the current journal to history, incrementing the transaction id
    /// and returning the logs.
    ///
    /// This function is used to prepare for next transaction. It will save the current journal
    /// and clear the journal for the next transaction.
    ///
    /// `commit_tx` is used even for discarding transactions so transaction_id will be incremented.
    pub fn commit_tx(&mut self) {
        // Clears all field from JournalInner. Doing it this way to avoid
        // missing any field.
        let Self {
            state,
            transient_storage,
            logs,
            depth,
            journal,
            transaction_id,
            spec,
            warm_addresses,
            perp,
            perp_commitment_log,
        } = self;
        // Spec precompiles and state are not changed. It is always set again execution.
        let _ = spec;
        let _ = state;
        transient_storage.clear();
        *depth = 0;

        // Do nothing with journal history so we can skip cloning present journal.
        journal.clear();

        // Keep the perp overlay (later txs in this block must see this tx's writes, exactly
        // like `state` above); only the tx-scoped undo log is spent.
        perp.undo.clear();

        // The commitment log is call-scoped (hashed/discarded at each precompile call exit), so it
        // must be empty at this tx boundary; reset defensively against any leak.
        perp_commitment_log.clear();

        // Clear coinbase address warming for next tx
        warm_addresses.clear_coinbase();
        // increment transaction id.
        *transaction_id += 1;
        logs.clear();
    }

    /// Discard the current transaction, by reverting the journal entries and incrementing the transaction id.
    pub fn discard_tx(&mut self) {
        // if there is no journal entries, there has not been any changes.
        let Self {
            state,
            transient_storage,
            logs,
            depth,
            journal,
            transaction_id,
            spec,
            warm_addresses,
            perp,
            perp_commitment_log,
        } = self;
        let is_spurious_dragon_enabled = spec.is_enabled_in(SPURIOUS_DRAGON);
        // iterate over all journals entries and revert our global state
        journal.drain(..).rev().for_each(|entry| {
            entry.revert(state, None, is_spurious_dragon_enabled);
        });
        // Revert this transaction's perp overlay writes too (mirrors the journal revert above),
        // so a discarded tx leaves no perp residue.
        perp.undo_to(0);
        // Call-scoped commitment log must be empty at this tx boundary; reset defensively.
        perp_commitment_log.clear();
        transient_storage.clear();
        *depth = 0;
        logs.clear();
        *transaction_id += 1;

        // Clear coinbase address warming for next tx
        warm_addresses.clear_coinbase();
    }

    /// Take the [`EvmState`] and clears the journal by resetting it to initial state.
    ///
    /// Note: Precompile addresses and spec are preserved and initial state of
    /// warm_preloaded_addresses will contain precompiles addresses.
    #[inline]
    pub fn finalize(&mut self) -> EvmState {
        // Clears all field from JournalInner. Doing it this way to avoid
        // missing any field.
        let Self {
            state,
            transient_storage,
            logs,
            depth,
            journal,
            transaction_id,
            spec,
            warm_addresses,
            perp,
            perp_commitment_log,
        } = self;
        // Spec is not changed. And it is always set again in execution.
        let _ = spec;
        // Clear coinbase address warming for next tx
        warm_addresses.clear_coinbase();

        let state = mem::take(state);
        logs.clear();
        transient_storage.clear();

        // Perp data leaves the journal via `take_perp_delta`, never folded into the returned
        // `EvmState`, so it never enters the state trie.
        //
        // Crucially, do NOT clear `perp.working` here. `finalize` runs once PER TRANSACTION in
        // block execution — alloy-evm's block executor calls `transact` (= `transact_one` +
        // `finalize`) for every tx — whereas the perp overlay is BLOCK-scoped: it must accumulate
        // across the block's transactions until the end-of-block `take_perp_delta` harvest drains
        // it into the canonical off-trie store. Clearing it here dropped every committed tx's perp
        // write before it could be harvested (canonical_perp stayed empty forever). Only the
        // tx-scoped undo log is reset, mirroring `commit_tx`, which already keeps `working`.
        perp.undo.clear();
        // Call-scoped commitment log must be empty at this tx boundary; reset defensively.
        perp_commitment_log.clear();

        // clear journal and journal history.
        journal.clear();
        *depth = 0;
        // reset transaction id.
        *transaction_id = 0;

        state
    }

    /// Return reference to state.
    #[inline]
    pub fn state(&mut self) -> &mut EvmState {
        &mut self.state
    }

    /// Sets SpecId.
    #[inline]
    pub fn set_spec_id(&mut self, spec: SpecId) {
        self.spec = spec;
    }

    /// Mark account as touched as only touched accounts will be added to state.
    /// This is especially important for state clear where touched empty accounts needs to
    /// be removed from state.
    #[inline]
    pub fn touch(&mut self, address: Address) {
        if let Some(account) = self.state.get_mut(&address) {
            Self::touch_account(&mut self.journal, address, account);
        }
    }

    /// Mark account as touched.
    #[inline]
    fn touch_account(journal: &mut Vec<ENTRY>, address: Address, account: &mut Account) {
        if !account.is_touched() {
            journal.push(ENTRY::account_touched(address));
            account.mark_touch();
        }
    }

    /// Returns the _loaded_ [Account] for the given address.
    ///
    /// This assumes that the account has already been loaded.
    ///
    /// # Panics
    ///
    /// Panics if the account has not been loaded and is missing from the state set.
    #[inline]
    pub fn account(&self, address: Address) -> &Account {
        self.state
            .get(&address)
            .expect("Account expected to be loaded") // Always assume that acc is already loaded
    }

    /// Set code and its hash to the account.
    ///
    /// Note: Assume account is warm and that hash is calculated from code.
    #[inline]
    pub fn set_code_with_hash(&mut self, address: Address, code: Bytecode, hash: B256) {
        let account = self.state.get_mut(&address).unwrap();
        Self::touch_account(&mut self.journal, address, account);

        self.journal.push(ENTRY::code_changed(address));

        account.info.code_hash = hash;
        account.info.code = Some(code);
    }

    /// Use it only if you know that acc is warm.
    ///
    /// Assume account is warm.
    ///
    /// In case of EIP-7702 code with zero address, the bytecode will be erased.
    #[inline]
    pub fn set_code(&mut self, address: Address, code: Bytecode) {
        if let Bytecode::Eip7702(eip7702_bytecode) = &code {
            if eip7702_bytecode.address().is_zero() {
                self.set_code_with_hash(address, Bytecode::default(), KECCAK_EMPTY);
                return;
            }
        }

        let hash = code.hash_slow();
        self.set_code_with_hash(address, code, hash)
    }

    /// Add journal entry for caller accounting.
    #[inline]
    pub fn caller_accounting_journal_entry(
        &mut self,
        address: Address,
        old_balance: U256,
        bump_nonce: bool,
    ) {
        // account balance changed.
        self.journal
            .push(ENTRY::balance_changed(address, old_balance));
        // account is touched.
        self.journal.push(ENTRY::account_touched(address));

        if bump_nonce {
            // nonce changed.
            self.journal.push(ENTRY::nonce_changed(address));
        }
    }

    /// Increments the balance of the account.
    ///
    /// Mark account as touched.
    #[inline]
    pub fn balance_incr<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        balance: U256,
    ) -> Result<(), DB::Error> {
        let account = self.load_account(db, address)?.data;
        let old_balance = account.info.balance;
        account.info.balance = account.info.balance.saturating_add(balance);

        // march account as touched.
        if !account.is_touched() {
            account.mark_touch();
            self.journal.push(ENTRY::account_touched(address));
        }

        // add journal entry for balance increment.
        self.journal
            .push(ENTRY::balance_changed(address, old_balance));
        Ok(())
    }

    /// Decreases the balance of the account.
    ///
    /// Mark account as touched.
    #[inline]
    pub fn balance_decr<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, DB::Error> {
        let account = self.load_account(db, address)?.data;
        let old_balance = account.info.balance;
        let Some(new_balance) = old_balance.checked_sub(balance) else {
            return Ok(Some(TransferError::OutOfFunds));
        };
        account.info.balance = new_balance;

        if !account.is_touched() {
            account.mark_touch();
            self.journal.push(ENTRY::account_touched(address));
        }

        self.journal
            .push(ENTRY::balance_changed(address, old_balance));
        Ok(None)
    }

    /// Increments the nonce of the account.
    #[inline]
    pub fn nonce_bump_journal_entry(&mut self, address: Address) {
        self.journal.push(ENTRY::nonce_changed(address));
    }

    /// Transfers balance from two accounts. Returns error if sender balance is not enough.
    #[inline]
    pub fn transfer<DB: Database>(
        &mut self,
        db: &mut DB,
        from: Address,
        to: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, DB::Error> {
        if balance.is_zero() {
            self.load_account(db, to)?;
            let to_account = self.state.get_mut(&to).unwrap();
            Self::touch_account(&mut self.journal, to, to_account);
            return Ok(None);
        }
        // load accounts
        self.load_account(db, from)?;
        self.load_account(db, to)?;

        // sub balance from
        let from_account = self.state.get_mut(&from).unwrap();
        Self::touch_account(&mut self.journal, from, from_account);
        let from_balance = &mut from_account.info.balance;

        let Some(from_balance_decr) = from_balance.checked_sub(balance) else {
            return Ok(Some(TransferError::OutOfFunds));
        };
        *from_balance = from_balance_decr;

        // add balance to
        let to_account = &mut self.state.get_mut(&to).unwrap();
        Self::touch_account(&mut self.journal, to, to_account);
        let to_balance = &mut to_account.info.balance;
        let Some(to_balance_incr) = to_balance.checked_add(balance) else {
            return Ok(Some(TransferError::OverflowPayment));
        };
        *to_balance = to_balance_incr;
        // Overflow of U256 balance is not possible to happen on mainnet. We don't bother to return funds from from_acc.

        self.journal
            .push(ENTRY::balance_transfer(from, to, balance));

        Ok(None)
    }

    /// Creates account or returns false if collision is detected.
    ///
    /// There are few steps done:
    /// 1. Make created account warm loaded (AccessList) and this should
    ///    be done before subroutine checkpoint is created.
    /// 2. Check if there is collision of newly created account with existing one.
    /// 3. Mark created account as created.
    /// 4. Add fund to created account
    /// 5. Increment nonce of created account if SpuriousDragon is active
    /// 6. Decrease balance of caller account.
    ///
    /// # Panics
    ///
    /// Panics if the caller is not loaded inside the EVM state.
    /// This should have been done inside `create_inner`.
    #[inline]
    pub fn create_account_checkpoint(
        &mut self,
        caller: Address,
        target_address: Address,
        balance: U256,
        spec_id: SpecId,
    ) -> Result<JournalCheckpoint, TransferError> {
        // Enter subroutine
        let checkpoint = self.checkpoint();

        // Fetch balance of caller.
        let caller_balance = self.state.get(&caller).unwrap().info.balance;
        // Check if caller has enough balance to send to the created contract.
        if caller_balance < balance {
            self.checkpoint_revert(checkpoint);
            return Err(TransferError::OutOfFunds);
        }

        // Newly created account is present, as we just loaded it.
        let target_acc = self.state.get_mut(&target_address).unwrap();
        let last_journal = &mut self.journal;

        // New account can be created if:
        // Bytecode is not empty.
        // Nonce is not zero
        // Account is not precompile.
        if target_acc.info.code_hash != KECCAK_EMPTY || target_acc.info.nonce != 0 {
            self.checkpoint_revert(checkpoint);
            return Err(TransferError::CreateCollision);
        }

        // set account status to create.
        let is_created_globally = target_acc.mark_created_locally();

        // this entry will revert set nonce.
        last_journal.push(ENTRY::account_created(target_address, is_created_globally));
        target_acc.info.code = None;
        // EIP-161: State trie clearing (invariant-preserving alternative)
        if spec_id.is_enabled_in(SPURIOUS_DRAGON) {
            // nonce is going to be reset to zero in AccountCreated journal entry.
            target_acc.info.nonce = 1;
        }

        // touch account. This is important as for pre SpuriousDragon account could be
        // saved even empty.
        Self::touch_account(last_journal, target_address, target_acc);

        // Add balance to created account, as we already have target here.
        let Some(new_balance) = target_acc.info.balance.checked_add(balance) else {
            self.checkpoint_revert(checkpoint);
            return Err(TransferError::OverflowPayment);
        };
        target_acc.info.balance = new_balance;

        // safe to decrement for the caller as balance check is already done.
        self.state.get_mut(&caller).unwrap().info.balance -= balance;

        // add journal entry of transferred balance
        last_journal.push(ENTRY::balance_transfer(caller, target_address, balance));

        Ok(checkpoint)
    }

    /// Makes a checkpoint that in case of Revert can bring back state to this point.
    #[inline]
    pub fn checkpoint(&mut self) -> JournalCheckpoint {
        let checkpoint = JournalCheckpoint {
            log_i: self.logs.len(),
            journal_i: self.journal.len(),
            perp_journal_i: self.perp.undo.len(),
            perp_commitment_log_len: self.perp_commitment_log.len(),
        };
        self.depth += 1;
        checkpoint
    }

    /// Commits the checkpoint.
    #[inline]
    pub fn checkpoint_commit(&mut self) {
        self.depth -= 1;
    }

    /// Reverts all changes to state until given checkpoint.
    #[inline]
    pub fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        let is_spurious_dragon_enabled = self.spec.is_enabled_in(SPURIOUS_DRAGON);
        let state = &mut self.state;
        let transient_storage = &mut self.transient_storage;
        self.depth -= 1;
        self.logs.truncate(checkpoint.log_i);

        // iterate over last N journals sets and revert our global state
        if checkpoint.journal_i < self.journal.len() {
            self.journal
                .drain(checkpoint.journal_i..)
                .rev()
                .for_each(|entry| {
                    entry.revert(state, Some(transient_storage), is_spurious_dragon_enabled);
                });
        }

        // Revert off-trie PerpDEX overlay writes made after the checkpoint, in lock-step with
        // the EVM journal entries above.
        self.perp.undo_to(checkpoint.perp_journal_i);

        // Truncate the per-call commitment log back to its checkpoint length, dropping the framed
        // writes appended after the checkpoint so the call-exit hash stays consistent with the
        // reverted overlay. The log is append-only within a call, so a length truncate suffices.
        self.perp_commitment_log.truncate(checkpoint.perp_commitment_log_len);
    }

    /// Performs selfdestruct action.
    /// Transfers balance from address to target. Check if target exist/is_cold
    ///
    /// Note: Balance will be lost if address and target are the same BUT when
    /// current spec enables Cancun, this happens only when the account associated to address
    /// is created in the same tx
    ///
    /// # References:
    ///  * <https://github.com/ethereum/go-ethereum/blob/141cd425310b503c5678e674a8c3872cf46b7086/core/vm/instructions.go#L832-L833>
    ///  * <https://github.com/ethereum/go-ethereum/blob/141cd425310b503c5678e674a8c3872cf46b7086/core/state/statedb.go#L449>
    ///  * <https://eips.ethereum.org/EIPS/eip-6780>
    #[inline]
    pub fn selfdestruct<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        target: Address,
    ) -> Result<StateLoad<SelfDestructResult>, DB::Error> {
        let spec = self.spec;
        let account_load = self.load_account(db, target)?;
        let is_cold = account_load.is_cold;
        let is_empty = account_load.state_clear_aware_is_empty(spec);

        if address != target {
            // Both accounts are loaded before this point, `address` as we execute its contract.
            // and `target` at the beginning of the function.
            let acc_balance = self.state.get(&address).unwrap().info.balance;

            let target_account = self.state.get_mut(&target).unwrap();
            Self::touch_account(&mut self.journal, target, target_account);
            target_account.info.balance += acc_balance;
        }

        let acc = self.state.get_mut(&address).unwrap();
        let balance = acc.info.balance;

        let destroyed_status = if !acc.is_selfdestructed() {
            SelfdestructionRevertStatus::GloballySelfdestroyed
        } else if !acc.is_selfdestructed_locally() {
            SelfdestructionRevertStatus::LocallySelfdestroyed
        } else {
            SelfdestructionRevertStatus::RepeatedSelfdestruction
        };

        let is_cancun_enabled = spec.is_enabled_in(CANCUN);

        // EIP-6780 (Cancun hard-fork): selfdestruct only if contract is created in the same tx
        let journal_entry = if acc.is_created_locally() || !is_cancun_enabled {
            acc.mark_selfdestructed_locally();
            acc.info.balance = U256::ZERO;
            Some(ENTRY::account_destroyed(
                address,
                target,
                destroyed_status,
                balance,
            ))
        } else if address != target {
            acc.info.balance = U256::ZERO;
            Some(ENTRY::balance_transfer(address, target, balance))
        } else {
            // State is not changed:
            // * if we are after Cancun upgrade and
            // * Selfdestruct account that is created in the same transaction and
            // * Specify the target is same as selfdestructed account. The balance stays unchanged.
            None
        };

        if let Some(entry) = journal_entry {
            self.journal.push(entry);
        };

        Ok(StateLoad {
            data: SelfDestructResult {
                had_value: !balance.is_zero(),
                target_exists: !is_empty,
                previously_destroyed: destroyed_status
                    == SelfdestructionRevertStatus::RepeatedSelfdestruction,
            },
            is_cold,
        })
    }

    /// Loads account into memory. return if it is cold or warm accessed
    #[inline]
    pub fn load_account<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, DB::Error> {
        self.load_account_optional(db, address, false, [])
    }

    /// Loads account into memory. If account is EIP-7702 type it will additionally
    /// load delegated account.
    ///
    /// It will mark both this and delegated account as warm loaded.
    ///
    /// Returns information about the account (If it is empty or cold loaded) and if present the information
    /// about the delegated account (If it is cold loaded).
    #[inline]
    pub fn load_account_delegated<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
    ) -> Result<StateLoad<AccountLoad>, DB::Error> {
        let spec = self.spec;
        let is_eip7702_enabled = spec.is_enabled_in(SpecId::PRAGUE);
        let account = self.load_account_optional(db, address, is_eip7702_enabled, [])?;
        let is_empty = account.state_clear_aware_is_empty(spec);

        let mut account_load = StateLoad::new(
            AccountLoad {
                is_delegate_account_cold: None,
                is_empty,
            },
            account.is_cold,
        );

        // load delegate code if account is EIP-7702
        if let Some(Bytecode::Eip7702(code)) = &account.info.code {
            let address = code.address();
            let delegate_account = self.load_account(db, address)?;
            account_load.data.is_delegate_account_cold = Some(delegate_account.is_cold);
        }

        Ok(account_load)
    }

    /// Loads account and its code. If account is already loaded it will load its code.
    ///
    /// It will mark account as warm loaded. If not existing Database will be queried for data.
    ///
    /// In case of EIP-7702 delegated account will not be loaded,
    /// [`Self::load_account_delegated`] should be used instead.
    #[inline]
    pub fn load_code<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, DB::Error> {
        self.load_account_optional(db, address, true, [])
    }

    /// Loads account. If account is already loaded it will be marked as warm.
    #[inline]
    pub fn load_account_optional<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        load_code: bool,
        storage_keys: impl IntoIterator<Item = StorageKey>,
    ) -> Result<StateLoad<&mut Account>, DB::Error> {
        let load = match self.state.entry(address) {
            Entry::Occupied(entry) => {
                let account = entry.into_mut();
                let is_cold = account.mark_warm_with_transaction_id(self.transaction_id);
                // if it is colad loaded we need to clear local flags that can interact with selfdestruct
                if is_cold {
                    // if it is cold loaded and we have selfdestructed locally it means that
                    // account was selfdestructed in previous transaction and we need to clear its information and storage.
                    if account.is_selfdestructed_locally() {
                        account.selfdestruct();
                        account.unmark_selfdestructed_locally();
                    }
                    // unmark locally created
                    account.unmark_created_locally();
                }
                StateLoad {
                    data: account,
                    is_cold,
                }
            }
            Entry::Vacant(vac) => {
                let account = if let Some(account) = db.basic(address)? {
                    account.into()
                } else {
                    Account::new_not_existing(self.transaction_id)
                };

                // Precompiles among some other account(coinbase included) are warm loaded so we need to take that into account
                let is_cold = self.warm_addresses.is_cold(&address);

                StateLoad {
                    data: vac.insert(account),
                    is_cold,
                }
            }
        };

        // journal loading of cold account.
        if load.is_cold {
            self.journal.push(ENTRY::account_warmed(address));
        }
        if load_code {
            let info = &mut load.data.info;
            if info.code.is_none() {
                let code = if info.code_hash == KECCAK_EMPTY {
                    Bytecode::default()
                } else {
                    db.code_by_hash(info.code_hash)?
                };
                info.code = Some(code);
            }
        }

        for storage_key in storage_keys.into_iter() {
            sload_with_account(
                load.data,
                db,
                &mut self.journal,
                self.transaction_id,
                address,
                storage_key,
            )?;
        }
        Ok(load)
    }

    /// Loads storage slot.
    ///
    /// # Panics
    ///
    /// Panics if the account is not present in the state.
    #[inline]
    pub fn sload<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        key: StorageKey,
    ) -> Result<StateLoad<StorageValue>, DB::Error> {
        // assume acc is warm
        let account = self.state.get_mut(&address).unwrap();
        // only if account is created in this tx we can assume that storage is empty.
        sload_with_account(
            account,
            db,
            &mut self.journal,
            self.transaction_id,
            address,
            key,
        )
    }

    /// Stores storage slot.
    ///
    /// And returns (original,present,new) slot value.
    ///
    /// **Note**: Account should already be present in our state.
    #[inline]
    pub fn sstore<DB: Database>(
        &mut self,
        db: &mut DB,
        address: Address,
        key: StorageKey,
        new: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, DB::Error> {
        // assume that acc exists and load the slot.
        let present = self.sload(db, address, key)?;
        let acc = self.state.get_mut(&address).unwrap();

        // if there is no original value in dirty return present value, that is our original.
        let slot = acc.storage.get_mut(&key).unwrap();

        // new value is same as present, we don't need to do anything
        if present.data == new {
            return Ok(StateLoad::new(
                SStoreResult {
                    original_value: slot.original_value(),
                    present_value: present.data,
                    new_value: new,
                },
                present.is_cold,
            ));
        }

        self.journal
            .push(ENTRY::storage_changed(address, key, present.data));
        // insert value into present state.
        slot.present_value = new;
        Ok(StateLoad::new(
            SStoreResult {
                original_value: slot.original_value(),
                present_value: present.data,
                new_value: new,
            },
            present.is_cold,
        ))
    }

    /// Read transient storage tied to the account.
    ///
    /// EIP-1153: Transient storage opcodes
    #[inline]
    pub fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue {
        self.transient_storage
            .get(&(address, key))
            .copied()
            .unwrap_or_default()
    }

    /// Store transient storage tied to the account.
    ///
    /// If values is different add entry to the journal
    /// so that old state can be reverted if that action is needed.
    ///
    /// EIP-1153: Transient storage opcodes
    #[inline]
    pub fn tstore(&mut self, address: Address, key: StorageKey, new: StorageValue) {
        let had_value = if new.is_zero() {
            // if new values is zero, remove entry from transient storage.
            // if previous values was some insert it inside journal.
            // If it is none nothing should be inserted.
            self.transient_storage.remove(&(address, key))
        } else {
            // insert values
            let previous_value = self
                .transient_storage
                .insert((address, key), new)
                .unwrap_or_default();

            // check if previous value is same
            if previous_value != new {
                // if it is different, insert previous values inside journal.
                Some(previous_value)
            } else {
                None
            }
        };

        if let Some(had_value) = had_value {
            // insert in journal only if value was changed.
            self.journal
                .push(ENTRY::transient_storage_changed(address, key, had_value));
        }
    }

    /// Pushes log into subroutine.
    #[inline]
    pub fn log(&mut self, log: Log) {
        self.logs.push(log);
    }
}

/// Loads storage slot with account.
#[inline]
pub fn sload_with_account<DB: Database, ENTRY: JournalEntryTr>(
    account: &mut Account,
    db: &mut DB,
    journal: &mut Vec<ENTRY>,
    transaction_id: usize,
    address: Address,
    key: StorageKey,
) -> Result<StateLoad<StorageValue>, DB::Error> {
    let is_newly_created = account.is_created();
    let (value, is_cold) = match account.storage.entry(key) {
        Entry::Occupied(occ) => {
            let slot = occ.into_mut();
            let is_cold = slot.mark_warm_with_transaction_id(transaction_id);
            (slot.present_value, is_cold)
        }
        Entry::Vacant(vac) => {
            // if storage was cleared, we don't need to ping db.
            let value = if is_newly_created {
                StorageValue::ZERO
            } else {
                db.storage(address, key)?
            };

            vac.insert(EvmStorageSlot::new(value, transaction_id));

            (value, true)
        }
    };

    if is_cold {
        // add it to journal as cold loaded.
        journal.push(ENTRY::storage_warmed(address, key));
    }

    Ok(StateLoad::new(value, is_cold))
}

#[cfg(test)]
mod perp_tests {
    use super::{JournalInner, PerpBlob};
    use crate::journal::JournalEntry;
    use primitives::B256;

    fn new_inner() -> JournalInner<JournalEntry> {
        JournalInner::new()
    }

    fn k(n: u8) -> B256 {
        B256::repeat_byte(n)
    }

    #[test]
    fn checkpoint_revert_restores_prior_perp_write() {
        let mut j = new_inner();
        // tx1 writes a baseline value, committed at the tx boundary.
        j.perp_store(k(1), vec![1, 2, 3]);
        j.commit_tx();

        // A sub-call overwrites the key, then reverts.
        let cp = j.checkpoint();
        j.perp_store(k(1), vec![9, 9]);
        assert_eq!(j.perp_get_overlay(k(1)), Some(vec![9u8, 9]));
        j.checkpoint_revert(cp);

        // The committed baseline is restored, and depth is balanced.
        assert_eq!(j.perp_get_overlay(k(1)), Some(vec![1u8, 2, 3]));
        assert_eq!(j.depth, 0);
    }

    #[test]
    fn checkpoint_revert_removes_newly_written_key() {
        let mut j = new_inner();
        let cp = j.checkpoint();
        j.perp_store(k(2), vec![5]);
        assert_eq!(j.perp_get_overlay(k(2)), Some(vec![5u8]));
        j.checkpoint_revert(cp);
        // The key was absent before the checkpoint, so revert removes it from the overlay
        // (a later read falls through to the committed store).
        assert_eq!(j.perp_get_overlay(k(2)), None);
        assert!(j.perp.working.is_empty());
        assert!(j.perp.undo.is_empty());
    }

    #[test]
    fn discard_tx_wipes_perp_writes() {
        let mut j = new_inner();
        j.perp_store(k(1), vec![1]);
        j.perp_store(k(2), vec![2]);
        j.discard_tx();
        // A discarded transaction leaves no perp residue.
        assert_eq!(j.perp_get_overlay(k(1)), None);
        assert_eq!(j.perp_get_overlay(k(2)), None);
        assert!(j.perp.undo.is_empty());
    }

    #[test]
    fn discard_tx_preserves_prior_committed_baseline() {
        let mut j = new_inner();
        // tx1 writes a baseline value and commits at the tx boundary (working kept, undo cleared).
        j.perp_store(k(1), vec![1]);
        j.commit_tx();
        // tx2 writes another key, then is discarded.
        j.perp_store(k(2), vec![2]);
        j.discard_tx();
        // discard_tx (undo_to(0)) must revert ONLY tx2's writes, never tx1's committed baseline —
        // this distinguishes the correct undo-log replay from a naive working.clear().
        assert_eq!(j.perp_get_overlay(k(1)), Some(vec![1u8]));
        assert_eq!(j.perp_get_overlay(k(2)), None);
    }

    #[test]
    fn commit_tx_preserves_working_and_clears_undo() {
        let mut j = new_inner();
        j.perp_store(k(1), vec![1]);
        j.commit_tx();
        // Working survives across the tx boundary (intra-block visibility); the undo is spent.
        assert_eq!(j.perp_get_overlay(k(1)), Some(vec![1u8]));
        assert!(j.perp.undo.is_empty());
    }

    #[test]
    fn take_perp_delta_returns_net_writes_and_drains() {
        let mut j = new_inner();
        j.perp_store(k(1), vec![1]);
        j.perp_store(k(1), vec![2]); // last write wins
        j.perp_store(k(3), vec![]); // empty value = delete marker
        let delta = j.take_perp_delta();
        assert_eq!(delta.get(&k(1)), Some(&vec![2u8]));
        assert_eq!(delta.get(&k(3)), Some(&Vec::<u8>::new()));
        assert_eq!(delta.len(), 2);
        assert!(j.perp.working.is_empty());
        assert!(j.perp.undo.is_empty());
    }

    // #16d Phase 2 — deferred-struct overlay path.
    fn ser_u32(v: &PerpBlob) -> Vec<u8> {
        v.downcast_ref::<u32>().unwrap().to_le_bytes().to_vec()
    }
    fn clone_u32(v: &PerpBlob) -> std::boxed::Box<PerpBlob> {
        std::boxed::Box::new(*v.downcast_ref::<u32>().unwrap())
    }

    #[test]
    fn perp_store_struct_defers_serialization() {
        let mut j = new_inner();
        j.perp_store_struct(k(1), std::boxed::Box::new(7u32), ser_u32, clone_u32);

        // Typed fast-path read returns the struct (no serialization).
        assert_eq!(j.perp_get_struct(k(1)).unwrap().downcast_ref::<u32>(), Some(&7u32));
        // Byte-interface read serializes on demand (same bytes the delta will carry).
        assert_eq!(j.perp_get_overlay(k(1)), Some(7u32.to_le_bytes().to_vec()));
        // Clone (the JournalInner Clone path) preserves the deferred struct via the clone fn-ptr.
        let j2 = j.clone();
        assert_eq!(j2.perp_get_struct(k(1)).unwrap().downcast_ref::<u32>(), Some(&7u32));
        // Block-end drain serializes ONCE.
        let delta = j.take_perp_delta();
        assert_eq!(delta.get(&k(1)), Some(&7u32.to_le_bytes().to_vec()));
    }

    #[test]
    fn perp_store_struct_reverts() {
        let mut j = new_inner();
        let cp = j.checkpoint();
        j.perp_store_struct(k(1), std::boxed::Box::new(42u32), ser_u32, clone_u32);
        assert!(j.perp_get_struct(k(1)).is_some());
        j.checkpoint_revert(cp);
        // The struct write is move-undone in lock-step with the byte path.
        assert!(j.perp_get_struct(k(1)).is_none());
        assert_eq!(j.perp_get_overlay(k(1)), None);
    }

    #[test]
    fn perp_get_struct_mut_mutates_in_place_and_reverts() {
        let mut j = new_inner();
        // Seed a struct committed BEFORE the checkpoint (the revert baseline).
        j.perp_store_struct(k(1), std::boxed::Box::new(10u32), ser_u32, clone_u32);
        j.commit_tx(); // working keeps the struct; undo spent.

        let cp = j.checkpoint();
        // In-place mutation via the &mut handle — no load/store round-trip, one undo snapshot.
        *j.perp_get_struct_mut(k(1)).unwrap().downcast_mut::<u32>().unwrap() = 99;
        assert_eq!(j.perp_get_struct(k(1)).unwrap().downcast_ref::<u32>(), Some(&99u32));
        // Block-end serialization would carry the mutated value.
        assert_eq!(j.perp_get_overlay(k(1)), Some(99u32.to_le_bytes().to_vec()));

        // Revert restores the pre-mutation value (snapshot-on-mutate undo).
        j.checkpoint_revert(cp);
        assert_eq!(j.perp_get_struct(k(1)).unwrap().downcast_ref::<u32>(), Some(&10u32));

        // Absent and raw-`Bytes` keys cannot be mutated in place.
        assert!(j.perp_get_struct_mut(k(2)).is_none());
        j.perp_store(k(3), vec![1, 2, 3]);
        assert!(j.perp_get_struct_mut(k(3)).is_none());
    }

    #[test]
    fn finalize_excludes_perp_from_state_but_preserves_block_overlay() {
        let mut j = new_inner();
        j.perp_store(k(1), vec![7]);
        let state = j.finalize();
        // Perp data is NOT folded into the returned EvmState (it stays off the trie)...
        assert!(state.is_empty());
        // ...but `finalize` runs PER TX in block execution, so it must NOT wipe the block-scoped
        // overlay; the write survives for the end-of-block `take_perp_delta` harvest.
        assert_eq!(j.perp.get_bytes(k(1)), Some(vec![7u8]));
        // The tx-scoped undo log is still reset.
        assert!(j.perp.undo.is_empty());
    }

    /// Regression for the canonical_perp persistence bug: the block executor runs
    /// `transact` (= `transact_one` + `finalize`) per tx, so perp writes must accumulate across
    /// per-tx `finalize` calls and be harvested by the block-end `take_perp_delta` — otherwise a
    /// committed `initAdmin` write is dropped and every later reader sees an empty store.
    #[test]
    fn perp_writes_survive_per_tx_finalize_until_block_harvest() {
        let mut j = new_inner();
        // tx 1: write admin, then finalize (as `transact` does after the handler's commit_tx).
        j.perp_store(k(1), vec![0xAA]);
        let _ = j.finalize();
        // tx 2: write a market, then finalize.
        j.perp_store(k(2), vec![0xBB]);
        let _ = j.finalize();
        // End of block: harvest. Both committed writes must be present.
        let delta = j.take_perp_delta();
        assert_eq!(delta.get(&k(1)), Some(&vec![0xAAu8]));
        assert_eq!(delta.get(&k(2)), Some(&vec![0xBBu8]));
        assert_eq!(delta.len(), 2);
        // Drained after harvest.
        assert!(j.perp.working.is_empty());
    }

    #[test]
    fn nested_checkpoint_revert_only_undoes_inner_scope() {
        let mut j = new_inner();
        let outer = j.checkpoint();
        j.perp_store(k(1), vec![10]);
        let inner = j.checkpoint();
        j.perp_store(k(2), vec![20]);

        // Revert the inner scope: k(2) gone, k(1) survives.
        j.checkpoint_revert(inner);
        assert_eq!(j.perp_get_overlay(k(1)), Some(vec![10u8]));
        assert_eq!(j.perp_get_overlay(k(2)), None);

        // Revert the outer scope: both gone, depth balanced.
        j.checkpoint_revert(outer);
        assert_eq!(j.perp_get_overlay(k(1)), None);
        assert_eq!(j.depth, 0);
    }

    /// Property test: under an arbitrary, deterministically-generated sequence of
    /// store / checkpoint / checkpoint_commit / checkpoint_revert operations (with nesting),
    /// the perp overlay must always equal an independent reference model of the same writes.
    /// This stresses the undo log far beyond the hand-written cases above. Dependency-free
    /// (xorshift PRNG, fixed seed → reproducible).
    #[test]
    fn fuzz_overlay_matches_reference_model_under_nested_checkpoints() {
        use std::collections::HashMap as RefMap;

        let mut j = new_inner();
        // Reference overlay model, mirroring perp.working.
        let mut model: RefMap<B256, Vec<u8>> = RefMap::new();
        // Stack of (checkpoint token, model snapshot) for nested scopes.
        let mut snaps: Vec<(_, RefMap<B256, Vec<u8>>)> = Vec::new();

        // Deterministic xorshift64 PRNG.
        let mut s: u64 = 0x9E3779B97F4A7C15;
        let mut rng = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };

        let probe: Vec<B256> = (0..6u8).map(k).collect();

        for _ in 0..3000 {
            match rng() % 5 {
                // store (weighted 2/5)
                0 | 1 => {
                    let key = k((rng() % 6) as u8);
                    let len = (rng() % 4) as usize;
                    let val = vec![(rng() % 251) as u8; len];
                    j.perp_store(key, val.clone());
                    model.insert(key, val);
                }
                // checkpoint
                2 => {
                    let cp = j.checkpoint();
                    snaps.push((cp, model.clone()));
                }
                // checkpoint_commit: keep changes, drop the snapshot
                3 => {
                    if !snaps.is_empty() {
                        j.checkpoint_commit();
                        snaps.pop();
                    }
                }
                // checkpoint_revert: restore to the snapshot
                _ => {
                    if let Some((cp, snap)) = snaps.pop() {
                        j.checkpoint_revert(cp);
                        model = snap;
                    }
                }
            }

            // Invariant: overlay == reference model for all probe keys.
            for key in &probe {
                assert_eq!(
                    j.perp_get_overlay(*key),
                    model.get(key).cloned(),
                    "overlay diverged from reference model at key {key:?}"
                );
            }
        }
    }
}
