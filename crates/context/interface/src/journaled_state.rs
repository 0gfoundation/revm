//! Journaled state trait [`JournalTr`] and related types.
use crate::context::{SStoreResult, SelfDestructResult};
use core::ops::{Deref, DerefMut};
use database_interface::Database;
use primitives::{
    hardfork::SpecId, Address, Bytes, HashMap, HashSet, Log, StorageKey, StorageValue, B256, U256,
};
use state::{Account, Bytecode};
use std::vec::Vec;

/// Net off-trie PerpDEX writes produced during execution, keyed by domain key.
///
/// This is the channel by which the in-memory orderbook ("PerpState") leaves the EVM:
/// it is drained via [`JournalTr::take_perp_delta`] and is deliberately NOT folded into
/// the [`JournalTr::State`] returned by [`JournalTr::finalize`], so perp data never enters
/// the state trie. An empty value means the key is absent/deleted. Carries no pre-images
/// (single-node, no-reorg scope). See `docs/perpstate-journal集成方案.md`.
pub type PerpDelta = HashMap<B256, Vec<u8>>;

/// One pre-computed PerpDEX trading-call result, replayed by the precompile during the serial EVM
/// pass under canonical parallel execution (catalog #21 spike, step 4b).
///
/// The parallel pre-phase already ran every trading op against the shared book, so during the serial
/// EVM loop the `0x…1003` precompile returns this verbatim (gas is flat per selector) instead of
/// re-verifying / re-matching / re-writing. `output` is the precompile return bytes (the ABI-encoded
/// orderId for a successful place, the revert reason for a revert); `reverted` is the call status.
/// Only `reverted` is consensus-relevant — the return data is not folded into the receipts root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PerpReplayResult {
    /// Whether the original trading call reverted (drives the receipt status).
    pub reverted: bool,
    /// The precompile return bytes to hand back (ABI orderId / revert reason).
    pub output: Vec<u8>,
}

/// Trait that contains database and journal of all changes that were made to the state.
pub trait JournalTr {
    /// Database type that is used in the journal.
    type Database: Database;
    /// State type that is returned by the journal after finalization.
    type State;

    /// Creates new Journaled state.
    ///
    /// Dont forget to set spec_id.
    fn new(database: Self::Database) -> Self;

    /// Returns a mutable reference to the database.
    fn db_mut(&mut self) -> &mut Self::Database;

    /// Returns an immutable reference to the database.
    fn db(&self) -> &Self::Database;

    /// Returns the storage value from Journal state.
    ///
    /// Loads the storage from database if not found in Journal state.
    fn sload(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<StateLoad<StorageValue>, <Self::Database as Database>::Error>;

    /// Stores the storage value in Journal state.
    fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, <Self::Database as Database>::Error>;

    /// Loads transient storage value.
    fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue;

    /// Stores transient storage value.
    fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue);

    /// Logs the log in Journal state.
    fn log(&mut self, log: Log);

    /// Marks the account for selfdestruction and transfers all the balance to the target.
    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
    ) -> Result<StateLoad<SelfDestructResult>, <Self::Database as Database>::Error>;

    /// Warms the account and storage.
    fn warm_account_and_storage(
        &mut self,
        address: Address,
        storage_keys: impl IntoIterator<Item = StorageKey>,
    ) -> Result<(), <Self::Database as Database>::Error>;

    /// Warms the account. Internally calls [`JournalTr::warm_account_and_storage`] with empty storage keys.
    fn warm_account(
        &mut self,
        address: Address,
    ) -> Result<(), <Self::Database as Database>::Error> {
        self.warm_account_and_storage(address, [])
    }

    /// Warms the coinbase account.
    fn warm_coinbase_account(&mut self, address: Address);

    /// Warms the precompiles.
    fn warm_precompiles(&mut self, addresses: HashSet<Address>);

    /// Returns the addresses of the precompiles.
    fn precompile_addresses(&self) -> &HashSet<Address>;

    /// Sets the spec id.
    fn set_spec_id(&mut self, spec_id: SpecId);

    /// Touches the account.
    fn touch_account(&mut self, address: Address);

    /// Transfers the balance from one account to another.
    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, <Self::Database as Database>::Error>;

    /// Increments the balance of the account.
    fn caller_accounting_journal_entry(
        &mut self,
        address: Address,
        old_balance: U256,
        bump_nonce: bool,
    );

    /// Increments the balance of the account.
    fn balance_incr(
        &mut self,
        address: Address,
        balance: U256,
    ) -> Result<(), <Self::Database as Database>::Error>;

    /// Decrease the balance of the account.
    fn balance_decr(
        &mut self,
        address: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, <Self::Database as Database>::Error>;

    /// Increments the nonce of the account.
    fn nonce_bump_journal_entry(&mut self, address: Address);

    /// Loads the account.
    fn load_account(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, <Self::Database as Database>::Error>;

    /// Loads the account code.
    fn load_account_code(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, <Self::Database as Database>::Error>;

    /// Loads the account delegated.
    fn load_account_delegated(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<AccountLoad>, <Self::Database as Database>::Error>;

    /// Sets bytecode with hash. Assume that account is warm.
    fn set_code_with_hash(&mut self, address: Address, code: Bytecode, hash: B256);

    /// Sets bytecode and calculates hash.
    ///
    /// Assume account is warm.
    #[inline]
    fn set_code(&mut self, address: Address, code: Bytecode) {
        let hash = code.hash_slow();
        self.set_code_with_hash(address, code, hash);
    }

    /// Returns account code bytes and if address is cold loaded.
    #[inline]
    fn code(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<Bytes>, <Self::Database as Database>::Error> {
        let a = self.load_account_code(address)?;
        // SAFETY: Safe to unwrap as load_code will insert code if it is empty.
        let code = a.info.code.as_ref().unwrap();
        let code = code.original_bytes();

        Ok(StateLoad::new(code, a.is_cold))
    }

    /// Gets code hash of account.
    fn code_hash(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<B256>, <Self::Database as Database>::Error> {
        let acc = self.load_account_code(address)?;
        if acc.is_empty() {
            return Ok(StateLoad::new(B256::ZERO, acc.is_cold));
        }
        // SAFETY: Safe to unwrap as load_code will insert code if it is empty.
        let _code = acc.info.code.as_ref().unwrap();

        let hash = acc.info.code_hash;

        Ok(StateLoad::new(hash, acc.is_cold))
    }

    /// Called at the end of the transaction to clean all residue data from journal.
    fn clear(&mut self) {
        let _ = self.finalize();
    }

    /// Creates a checkpoint of the current state. State can be revert to this point
    /// if needed.
    fn checkpoint(&mut self) -> JournalCheckpoint;

    /// Commits the changes made since the last checkpoint.
    fn checkpoint_commit(&mut self);

    /// Reverts the changes made since the last checkpoint.
    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint);

    /// Creates a checkpoint of the account creation.
    fn create_account_checkpoint(
        &mut self,
        caller: Address,
        address: Address,
        balance: U256,
        spec_id: SpecId,
    ) -> Result<JournalCheckpoint, TransferError>;

    /// Returns the depth of the journal.
    fn depth(&self) -> usize;

    /// Take logs from journal.
    fn take_logs(&mut self) -> Vec<Log>;

    /// Commit current transaction journal and returns transaction logs.
    fn commit_tx(&mut self);

    /// Discard current transaction journal by removing journal entries and logs and incrementing the transaction id.
    ///
    /// This function is useful to discard intermediate state that is interrupted by error and it will not revert
    /// any already committed changes and it is safe to call it multiple times.
    fn discard_tx(&mut self);

    /// Reads an off-trie PerpDEX blob by domain key (off-trie "PerpState").
    ///
    /// Returns the in-block overlay value if the key was written during this block, otherwise
    /// falls through to the committed off-trie store. The default implementation returns empty
    /// (no perp store is wired). See `docs/perpstate-journal集成方案.md` §4.4.
    fn perp_load(&mut self, key: B256) -> Result<Vec<u8>, <Self::Database as Database>::Error> {
        let _ = key;
        Ok(Vec::new())
    }

    /// Writes an off-trie PerpDEX blob, journaled so it reverts in lock-step with the surrounding
    /// checkpoint / `discard_tx`. An empty value marks the key absent. The default implementation
    /// is a no-op.
    fn perp_store(&mut self, key: B256, value: Vec<u8>) {
        let _ = (key, value);
    }

    /// Whether this journal is routing off-trie perp ops to a concurrent shared book (parallel block
    /// execution). The precompile uses this to skip the in-body, order-DEPENDENT mid-price sampling
    /// (which would race across slots / diverge from serial) and instead let the parallel driver's
    /// barrier record an order-INDEPENDENT block-end sample. Default backend (serial): `false`.
    fn perp_is_parallel(&self) -> bool {
        false
    }

    /// Whether this journal is in REPLAY mode (canonical parallel execution, step 4b): the parallel
    /// pre-phase already ran every trading op, so during the serial EVM loop the `0x…1003` precompile
    /// returns the pre-computed results in block order instead of re-verifying / re-matching. The
    /// precompile checks this before dispatching a trading selector. Default backend (serial): `false`.
    fn perp_is_replay(&self) -> bool {
        false
    }

    /// Pop the next pre-computed trading-call result (replay mode, block order). The precompile calls
    /// this once per trading-selector call; because the pre-phase produced results in block order and
    /// the serial loop hits trading calls in the same order, a simple cursor aligns them. `None` means
    /// the queue is EXHAUSTED — the precompile fail-stops (a trading call the pre-phase did not see,
    /// e.g. an internal contract call to `0x…1003`; the scheme assumes top-level perp txs only).
    /// Default: `None`.
    fn perp_replay_next(&mut self) -> Option<PerpReplayResult> {
        None
    }

    /// Loads the pre-computed trading-call results for REPLAY mode (canonical parallel execution,
    /// step 4b) and resets the cursor. The node sets this on the block's journal BEFORE executing the
    /// transactions (via the `Evm`). Default backend (serial / no perp journal): no-op.
    fn set_perp_replay(&mut self, results: Vec<PerpReplayResult>) {
        let _ = results;
    }

    /// Writes a deferred deserialized off-trie blob (#16d Phase 2): the value is kept type-erased
    /// and serialized to bytes ONCE at block end (`take_perp_delta`) via `ser`, instead of on every
    /// write. `clone` keeps the overlay `Clone`. Both fns are monomorphized by the caller (the
    /// precompile), so this crate needs no blob types or codec. Default backend is a no-op.
    fn perp_store_struct(
        &mut self,
        key: B256,
        val: std::boxed::Box<dyn core::any::Any>,
        ser: fn(&dyn core::any::Any) -> Vec<u8>,
        clone: fn(&dyn core::any::Any) -> std::boxed::Box<dyn core::any::Any>,
    ) {
        let _ = (key, val, ser, clone);
    }

    /// Whether `key` holds a deferred `Struct` overlay value written this block (the precompile's
    /// in-place fast path). Gate [`Self::perp_with_struct_mut`] on this. Default backend: `false`.
    fn perp_contains_struct(&mut self, key: B256) -> bool {
        let _ = key;
        false
    }

    /// Runs `f` against the deferred `Struct` overlay value at `key`, downcast to `&T` — the typed
    /// read fast path (the caller clones inside `f`, skipping deserialization). `None` if absent,
    /// written as raw bytes, or not a `T`. Closure form (not a returned `&T`) so a concurrent backend
    /// can hold its shard guard only for the duration of `f`. Default backend returns `None`.
    fn perp_with_struct<T: core::any::Any, R>(
        &mut self,
        key: B256,
        f: impl FnOnce(&T) -> R,
    ) -> Option<R> {
        let _ = (key, f);
        None
    }

    /// Runs `f` against the deferred `Struct` overlay value at `key`, downcast to `&mut T`, for
    /// IN-PLACE mutation (catalog #21) — the backend snapshots the prior value into its revert log
    /// first. MUST only be called when [`Self::perp_contains_struct`] returned `true` for `key`, so
    /// `f` always runs and a caller's `FnOnce` is never dropped unused on a miss (letting the caller
    /// reuse it on the load-then-store fall-through). Closure form so a concurrent backend holds its
    /// shard guard only across `f`. Default backend: unreachable (it has no struct overlay).
    fn perp_with_struct_mut<T: core::any::Any, R>(
        &mut self,
        key: B256,
        f: impl FnOnce(&mut T) -> R,
    ) -> R {
        let _ = (key, f);
        unreachable!("perp_with_struct_mut requires perp_contains_struct(key) == true")
    }

    /// Reads the block-scoped deserialized off-trie blob cache (catalog #14): a per-block
    /// accelerator that lets the precompile skip re-deserializing a blob it already decoded this
    /// block. Type-erased (this crate does not know the blob types); the caller downcasts and
    /// clones. The default backend keeps no cache and returns `None`.
    fn perp_cache_get(&mut self, key: B256) -> Option<&dyn core::any::Any> {
        let _ = key;
        None
    }

    /// Inserts a deserialized off-trie blob into the block-scoped read cache (no-op by default).
    /// Automatically invalidated on the next [`JournalTr::perp_store`] of the same key.
    fn perp_cache_put(&mut self, key: B256, value: std::boxed::Box<dyn core::any::Any>) {
        let _ = (key, value);
    }

    /// Drains and returns the block's net off-trie PerpDEX writes ([`PerpDelta`]).
    ///
    /// Called by the block executor while the EVM/journal is still alive (before it is consumed),
    /// so the orderbook delta can be applied to the committed off-trie store. NOT part of
    /// [`JournalTr::finalize`]'s output (perp stays off the state trie). The default implementation
    /// returns an empty delta.
    fn take_perp_delta(&mut self) -> PerpDelta {
        PerpDelta::default()
    }

    /// Appends framed bytes for one off-trie write to the per-call PerpDEX commitment log.
    ///
    /// The precompile accumulates every write of the current call into this in-memory log
    /// (`key ‖ len ‖ value` per write) and hashes it ONCE at call exit
    /// (`C_new = H(C_prev ‖ ver ‖ log)`), instead of sload+sstore per write. The default is a
    /// no-op (no perp wired). See the PerpDEX precompile's `store_blob` / `flush_commitment`.
    fn perp_fold_append(&mut self, bytes: &[u8]) {
        let _ = bytes;
    }

    /// Takes (clears) the per-call PerpDEX commitment log, returning it for the call-exit hash.
    /// An empty result means the call performed no off-trie write. Default: empty.
    fn perp_fold_take_log(&mut self) -> Vec<u8> {
        Vec::new()
    }

    /// Current length of the per-call PerpDEX commitment log (0 = nothing accumulated). Used to
    /// assert the call-scoped invariant that the log is empty at call entry. Default: 0.
    fn perp_fold_log_len(&mut self) -> usize {
        0
    }

    /// Clear current journal resetting it to initial state and return changes state.
    fn finalize(&mut self) -> Self::State;
}

/// Transfer and creation result
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TransferError {
    /// Caller does not have enough funds
    OutOfFunds,
    /// Overflow in target account
    OverflowPayment,
    /// Create collision.
    CreateCollision,
}

/// SubRoutine checkpoint that will help us to go back from this
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct JournalCheckpoint {
    /// Checkpoint to where on revert we will go back to.
    pub log_i: usize,
    /// Checkpoint to where on revert we will go back to and revert other journal entries.
    pub journal_i: usize,
    /// Checkpoint into the off-trie PerpDEX undo log; on revert, perp overlay writes made
    /// after this index are undone in lock-step with the EVM journal entries.
    pub perp_journal_i: usize,
    /// Length of the per-call PerpDEX commitment log at this checkpoint. On revert the log is
    /// truncated back to this length, dropping writes made after the checkpoint in lock-step with
    /// the perp overlay (the log is append-only within a call and hashed only at call exit).
    pub perp_commitment_log_len: usize,
}

/// State load information that contains the data and if the account or storage is cold loaded
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StateLoad<T> {
    /// Returned data
    pub data: T,
    /// Is account is cold loaded
    pub is_cold: bool,
}

impl<T> Deref for StateLoad<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<T> DerefMut for StateLoad<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl<T> StateLoad<T> {
    /// Returns a new [`StateLoad`] with the given data and cold load status.
    pub fn new(data: T, is_cold: bool) -> Self {
        Self { data, is_cold }
    }

    /// Maps the data of the [`StateLoad`] to a new value.
    ///
    /// Useful for transforming the data of the [`StateLoad`] without changing the cold load status.
    pub fn map<B, F>(self, f: F) -> StateLoad<B>
    where
        F: FnOnce(T) -> B,
    {
        StateLoad::new(f(self.data), self.is_cold)
    }
}

/// Result of the account load from Journal state
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AccountLoad {
    /// Does account have delegate code and delegated account is cold loaded
    pub is_delegate_account_cold: Option<bool>,
    /// Is account empty, if `true` account is not created
    pub is_empty: bool,
}
