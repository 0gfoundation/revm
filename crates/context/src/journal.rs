//! This module contains [`Journal`] struct and implements [`JournalTr`] trait for it.
//!
//! Entry submodule contains [`JournalEntry`] and [`JournalEntryTr`] traits.
//! and inner submodule contains [`JournalInner`] struct that contains state.
pub mod entry;
pub mod inner;
pub mod warm_addresses;

pub use entry::{JournalEntry, JournalEntryTr};
pub use inner::JournalInner;

use bytecode::Bytecode;
use context_interface::{
    context::{SStoreResult, SelfDestructResult, StateLoad},
    journaled_state::{AccountLoad, JournalCheckpoint, JournalTr, PerpDelta, TransferError},
};
use core::ops::{Deref, DerefMut};
use database_interface::Database;
use primitives::{hardfork::SpecId, Address, HashSet, Log, StorageKey, StorageValue, B256, U256};
use state::{Account, EvmState};
use std::vec::Vec;

/// A journal of state changes internal to the EVM
///
/// On each additional call, the depth of the journaled state is increased (`depth`) and a new journal is added.
///
/// The journal contains every state change that happens within that call, making it possible to revert changes made in a specific call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Journal<DB, ENTRY = JournalEntry>
where
    ENTRY: JournalEntryTr,
{
    /// Database
    pub database: DB,
    /// Inner journal state.
    pub inner: JournalInner<ENTRY>,
}

impl<DB, ENTRY> Deref for Journal<DB, ENTRY>
where
    ENTRY: JournalEntryTr,
{
    type Target = JournalInner<ENTRY>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<DB, ENTRY> DerefMut for Journal<DB, ENTRY>
where
    ENTRY: JournalEntryTr,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<DB, ENTRY: JournalEntryTr> Journal<DB, ENTRY> {
    /// Creates a new JournaledState by copying state data from a JournalInit and provided database.
    /// This allows reusing the state, logs, and other data from a previous execution context while
    /// connecting it to a different database backend.
    pub fn new_with_inner(database: DB, inner: JournalInner<ENTRY>) -> Self {
        Self { database, inner }
    }

    /// Consumes the [`Journal`] and returns [`JournalInner`].
    ///
    /// If you need to preserve the original journal, use [`Self::to_inner`] instead which clones the state.
    pub fn into_init(self) -> JournalInner<ENTRY> {
        self.inner
    }
}

impl<DB, ENTRY: JournalEntryTr + Clone> Journal<DB, ENTRY> {
    /// Creates a new [`JournalInner`] by cloning all internal state data (state, storage, logs, etc)
    /// This allows creating a new journaled state with the same state data but without
    /// carrying over the original database.
    ///
    /// This is useful when you want to reuse the current state for a new transaction or
    /// execution context, but want to start with a fresh database.
    pub fn to_inner(&self) -> JournalInner<ENTRY> {
        self.inner.clone()
    }
}

impl<DB: Database, ENTRY: JournalEntryTr> JournalTr for Journal<DB, ENTRY> {
    type Database = DB;
    type State = EvmState;

    fn new(database: DB) -> Journal<DB, ENTRY> {
        Self {
            inner: JournalInner::new(),
            database,
        }
    }

    fn db(&self) -> &Self::Database {
        &self.database
    }

    fn db_mut(&mut self) -> &mut Self::Database {
        &mut self.database
    }

    fn sload(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<StateLoad<StorageValue>, <Self::Database as Database>::Error> {
        self.inner.sload(&mut self.database, address, key)
    }

    fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, <Self::Database as Database>::Error> {
        self.inner.sstore(&mut self.database, address, key, value)
    }

    fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue {
        self.inner.tload(address, key)
    }

    fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue) {
        self.inner.tstore(address, key, value)
    }

    fn log(&mut self, log: Log) {
        self.inner.log(log)
    }

    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
    ) -> Result<StateLoad<SelfDestructResult>, DB::Error> {
        self.inner.selfdestruct(&mut self.database, address, target)
    }

    fn warm_coinbase_account(&mut self, address: Address) {
        self.inner.warm_addresses.set_coinbase(address);
    }

    fn warm_precompiles(&mut self, precompiles: HashSet<Address>) {
        self.inner
            .warm_addresses
            .set_precompile_addresses(precompiles);
    }

    #[inline]
    fn precompile_addresses(&self) -> &HashSet<Address> {
        self.inner.warm_addresses.precompiles()
    }

    /// Returns call depth.
    #[inline]
    fn depth(&self) -> usize {
        self.inner.depth
    }

    #[inline]
    fn warm_account_and_storage(
        &mut self,
        address: Address,
        storage_keys: impl IntoIterator<Item = StorageKey>,
    ) -> Result<(), <Self::Database as Database>::Error> {
        self.inner
            .load_account_optional(&mut self.database, address, false, storage_keys)?;
        Ok(())
    }

    #[inline]
    fn set_spec_id(&mut self, spec_id: SpecId) {
        self.inner.spec = spec_id;
    }

    #[inline]
    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, DB::Error> {
        self.inner.transfer(&mut self.database, from, to, balance)
    }

    #[inline]
    fn touch_account(&mut self, address: Address) {
        self.inner.touch(address);
    }

    #[inline]
    fn caller_accounting_journal_entry(
        &mut self,
        address: Address,
        old_balance: U256,
        bump_nonce: bool,
    ) {
        self.inner
            .caller_accounting_journal_entry(address, old_balance, bump_nonce);
    }

    /// Increments the balance of the account.
    #[inline]
    fn balance_incr(
        &mut self,
        address: Address,
        balance: U256,
    ) -> Result<(), <Self::Database as Database>::Error> {
        self.inner
            .balance_incr(&mut self.database, address, balance)
    }

    /// Increments the balance of the account.
    #[inline]
    fn balance_decr(
        &mut self,
        address: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, <Self::Database as Database>::Error> {
        self.inner
            .balance_decr(&mut self.database, address, balance)
    }

    /// Increments the nonce of the account.
    #[inline]
    fn nonce_bump_journal_entry(&mut self, address: Address) {
        self.inner.nonce_bump_journal_entry(address)
    }

    #[inline]
    fn load_account(&mut self, address: Address) -> Result<StateLoad<&mut Account>, DB::Error> {
        self.inner.load_account(&mut self.database, address)
    }

    #[inline]
    fn load_account_code(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&mut Account>, DB::Error> {
        self.inner.load_code(&mut self.database, address)
    }

    #[inline]
    fn load_account_delegated(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<AccountLoad>, DB::Error> {
        self.inner
            .load_account_delegated(&mut self.database, address)
    }

    #[inline]
    fn checkpoint(&mut self) -> JournalCheckpoint {
        self.inner.checkpoint()
    }

    #[inline]
    fn checkpoint_commit(&mut self) {
        self.inner.checkpoint_commit()
    }

    #[inline]
    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        self.inner.checkpoint_revert(checkpoint)
    }

    #[inline]
    fn set_code_with_hash(&mut self, address: Address, code: Bytecode, hash: B256) {
        self.inner.set_code_with_hash(address, code, hash);
    }

    #[inline]
    fn create_account_checkpoint(
        &mut self,
        caller: Address,
        address: Address,
        balance: U256,
        spec_id: SpecId,
    ) -> Result<JournalCheckpoint, TransferError> {
        // Ignore error.
        self.inner
            .create_account_checkpoint(caller, address, balance, spec_id)
    }

    #[inline]
    fn take_logs(&mut self) -> Vec<Log> {
        self.inner.take_logs()
    }

    #[inline]
    fn commit_tx(&mut self) {
        self.inner.commit_tx()
    }

    #[inline]
    fn discard_tx(&mut self) {
        self.inner.discard_tx();
    }

    #[inline]
    fn perp_load(&mut self, key: B256) -> Result<Vec<u8>, <Self::Database as Database>::Error> {
        // Overlay first: a key written earlier in this block.
        if let Some(value) = self.inner.perp_get_overlay(key) {
            return Ok(value.to_vec());
        }
        // Cold miss: fall through to the committed off-trie perp store via the database —
        // exactly as `sload` falls through to `Database::storage` for trie slots.
        //
        // `Database::perp_storage` defaults to empty, so a chain executing from genesis
        // in-process still behaves correctly. The embedder (e.g. reth) MUST override
        // `Database::perp_storage` on its execution DB to read the committed `canonical_perp`
        // store, otherwise cross-block perp reads return empty. See
        // `docs/perpstate-journal集成方案.md` §4.4 (cold-read pass-through).
        self.database.perp_storage(key)
    }

    #[inline]
    fn perp_store(&mut self, key: B256, value: Vec<u8>) {
        self.inner.perp_store(key, value);
    }

    #[inline]
    fn take_perp_delta(&mut self) -> PerpDelta {
        self.inner.take_perp_delta()
    }

    #[inline]
    fn perp_fold_append(&mut self, bytes: &[u8]) {
        self.inner.perp_fold_append(bytes);
    }

    #[inline]
    fn perp_fold_take_log(&mut self) -> Vec<u8> {
        self.inner.perp_fold_take_log()
    }

    #[inline]
    fn perp_fold_log_len(&mut self) -> usize {
        self.inner.perp_fold_log_len()
    }

    /// Clear current journal resetting it to initial state and return changes state.
    #[inline]
    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }
}

#[cfg(test)]
mod perp_passthrough_tests {
    use super::Journal;
    use bytecode::Bytecode;
    use context_interface::JournalTr;
    use database_interface::Database;
    use primitives::{Address, HashMap, StorageKey, StorageValue, B256};
    use state::AccountInfo;
    use std::vec::Vec;

    /// Minimal DB whose `perp_storage` is backed by a map — stands in for reth's committed
    /// off-trie `canonical_perp` store, letting us test the cold-read pass-through without reth.
    #[derive(Default)]
    struct PerpBackedDb {
        perp: HashMap<B256, Vec<u8>>,
    }

    impl Database for PerpBackedDb {
        type Error = core::convert::Infallible;
        fn basic(&mut self, _: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(None)
        }
        fn code_by_hash(&mut self, _: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }
        fn storage(&mut self, _: Address, _: StorageKey) -> Result<StorageValue, Self::Error> {
            Ok(StorageValue::ZERO)
        }
        fn block_hash(&mut self, _: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
        fn perp_storage(&mut self, key: B256) -> Result<Vec<u8>, Self::Error> {
            Ok(self.perp.get(&key).cloned().unwrap_or_default())
        }
    }

    fn k(n: u8) -> B256 {
        B256::repeat_byte(n)
    }

    #[test]
    fn cold_perp_read_falls_through_to_committed_store() {
        let mut db = PerpBackedDb::default();
        db.perp.insert(k(1), vec![7, 8, 9]); // committed by a prior block
        let mut j: Journal<PerpBackedDb> = Journal::new(db);

        // Empty overlay (fresh per-block journal) → cold read reaches the committed store.
        // This is exactly what the inline Phase-1 stub could NOT do (it returned empty).
        assert_eq!(j.perp_load(k(1)).unwrap(), vec![7, 8, 9]);
        // A genuinely absent key returns empty (the default).
        assert_eq!(j.perp_load(k(2)).unwrap(), Vec::<u8>::new());
        // An in-block write shadows the committed value (overlay wins).
        j.perp_store(k(1), vec![1]);
        assert_eq!(j.perp_load(k(1)).unwrap(), vec![1]);
    }

    #[test]
    fn revert_of_overlay_write_re_exposes_committed_value() {
        let mut db = PerpBackedDb::default();
        db.perp.insert(k(1), vec![7]); // committed value
        let mut j: Journal<PerpBackedDb> = Journal::new(db);

        let cp = j.checkpoint();
        j.perp_store(k(1), vec![99]); // overlay shadows committed
        assert_eq!(j.perp_load(k(1)).unwrap(), vec![99]);

        j.checkpoint_revert(cp); // overlay write removed (prev == None)
        // Overlay miss again → falls through to the committed store, NOT to empty.
        assert_eq!(j.perp_load(k(1)).unwrap(), vec![7]);
    }
}
