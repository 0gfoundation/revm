//! The engine⇄host seam.
//!
//! Everything the matching engine needs from its execution environment is behind
//! [`PerpHost`]: the typed live store, the byte-tier off-trie overlay with committed
//! fall-through, event emission, the block timestamp, the external (ERC20) balance
//! bridge used by deposit/withdraw, a scoped EVM-side checkpoint for the liquidation
//! sweep, and the commit-only write witness.
//!
//! Two host implementations ship here:
//!
//! - a **blanket impl for every `CTX: ContextTr`** (the revm context) — engine entry
//!   points keep their historical `(input, caller, &mut ctx)` call shape, so the
//!   precompile shell, the engine tests (against a real `Journal`), and the bench
//!   harness need no adapter;
//! - [`InMemoryHost`] — a self-contained host (typed store + byte overlay + committed
//!   map + log sink) for standalone use and perf benches, mirroring reth's
//!   `canonical_perp` cold-read shape.

use std::sync::Arc;

use context_interface::{
    journaled_state::{JournalCheckpoint, PerpBlob, PerpDelta, PerpDeltaEntry},
    Block, ContextTr, JournalTr,
};
use perp_core::{
    error::{perp_err, PerpError},
    keys::erc20_balance_slot,
    typed_store::TypedPerpStore,
};
use primitives::{Address, Log, B256, U256};

/// Host services required by the PerpDEX matching engine.
///
/// Engine functions are generic over `H: PerpHost`; parameter names keep the historical
/// `context` so the engine body reads unchanged.
pub trait PerpHost {
    /// Opaque token for the scoped EVM-side revert used by the liquidation sweep.
    type Checkpoint;

    /// The strongly-typed live store for hot namespaces (installed lazily on first touch).
    fn live(&mut self) -> &mut TypedPerpStore;

    /// Byte-tier off-trie read: in-block overlay first, then the committed cross-block
    /// store. Empty bytes = absent.
    fn perp_load(&mut self, key: B256) -> Result<Vec<u8>, PerpError>;

    /// Byte-tier off-trie write into the in-block overlay. Empty bytes = delete.
    fn perp_store(&mut self, key: B256, value: Vec<u8>);

    /// Cross-block cold read of an already-decoded blob (skips deserialization when the
    /// committed store holds the decoded `Arc`). `None` → fall back to [`Self::perp_load`].
    fn perp_load_arc(&mut self, key: B256) -> Result<Option<Arc<PerpBlob>>, PerpError>;

    /// Emits an engine event (EVM log at the precompile boundary).
    fn log(&mut self, log: Log);

    /// Current block timestamp (seconds).
    fn timestamp(&self) -> u64;

    /// External (ERC20) balance read for the deposit/withdraw bridge.
    fn external_balance(&mut self, token: Address, owner: Address) -> Result<U256, PerpError>;

    /// External (ERC20) balance write for the deposit/withdraw bridge.
    fn set_external_balance(
        &mut self,
        token: Address,
        owner: Address,
        value: U256,
    ) -> Result<(), PerpError>;

    /// Scoped checkpoint for the liquidation sweep: balances EVM-side effects (logs,
    /// warmed accounts) of a probed-then-skipped candidate. Perp writes are commit-only
    /// and are NOT covered — the sweep only reverts candidates that made zero perp writes.
    fn checkpoint(&mut self) -> Self::Checkpoint;
    /// Commits the innermost checkpoint.
    fn checkpoint_commit(&mut self);
    /// Reverts to `cp`.
    fn checkpoint_revert(&mut self, cp: Self::Checkpoint);

    /// Monotonic count of perp writes this transaction (typed store + byte overlay) —
    /// the commit-only write witness used by the batch driver.
    fn perp_write_count(&self) -> u64;

    /// Call-frame depth of the current execution. The entry shell rejects `> 1`
    /// (EOA-only, commit-only #23): no enclosing frame may revert after a perp write.
    fn call_depth(&self) -> usize;
}

// ── Blanket host: any revm context ────────────────────────────────────────────

/// Maps a journal/database error into a clean engine reject, mirroring the historical
/// `convert_db_err` (`PrecompileError::Other`) behavior.
fn db_err<E: core::fmt::Debug>(e: E) -> PerpError {
    perp_err(format!("Database error: {e:?}"))
}

impl<CTX: ContextTr> PerpHost for CTX {
    type Checkpoint = JournalCheckpoint;

    fn live(&mut self) -> &mut TypedPerpStore {
        let journal = self.journal_mut();
        if journal.perp_live_get_mut().is_none() {
            journal.perp_live_init(std::boxed::Box::new(TypedPerpStore::default()));
        }
        journal
            .perp_live_get_mut()
            .expect("perp live store just initialized")
            .as_any_mut()
            .downcast_mut::<TypedPerpStore>()
            .expect("perp live store is TypedPerpStore")
    }

    fn perp_load(&mut self, key: B256) -> Result<Vec<u8>, PerpError> {
        self.journal_mut().perp_load(key).map_err(db_err)
    }

    fn perp_store(&mut self, key: B256, value: Vec<u8>) {
        self.journal_mut().perp_store(key, value);
    }

    fn perp_load_arc(&mut self, key: B256) -> Result<Option<Arc<PerpBlob>>, PerpError> {
        self.journal_mut().perp_load_arc(key).map_err(db_err)
    }

    fn log(&mut self, log: Log) {
        self.journal_mut().log(log);
    }

    fn timestamp(&self) -> u64 {
        self.block().timestamp().saturating_to()
    }

    fn external_balance(&mut self, token: Address, owner: Address) -> Result<U256, PerpError> {
        let slot = erc20_balance_slot(owner);
        // Warm the token account first — sload panics on a journal-absent account.
        self.journal_mut().warm_account(token).map_err(db_err)?;
        Ok(self
            .journal_mut()
            .sload(token, slot.into())
            .map_err(db_err)?
            .data)
    }

    fn set_external_balance(
        &mut self,
        token: Address,
        owner: Address,
        value: U256,
    ) -> Result<(), PerpError> {
        let slot = erc20_balance_slot(owner);
        self.journal_mut().warm_account(token).map_err(db_err)?;
        self.journal_mut()
            .sstore(token, slot.into(), value)
            .map_err(db_err)?;
        // Touch so the storage change enters the BundleState transition — untouched
        // accounts are skipped by apply_account_state() and the sstore is silently lost.
        self.journal_mut().touch_account(token);
        Ok(())
    }

    fn checkpoint(&mut self) -> JournalCheckpoint {
        self.journal_mut().checkpoint()
    }

    fn checkpoint_commit(&mut self) {
        self.journal_mut().checkpoint_commit();
    }

    fn checkpoint_revert(&mut self, cp: JournalCheckpoint) {
        self.journal_mut().checkpoint_revert(cp);
    }

    fn perp_write_count(&self) -> u64 {
        JournalTr::perp_write_count(self.journal_ref())
    }

    fn call_depth(&self) -> usize {
        self.journal_ref().depth()
    }
}

// ── In-memory host ────────────────────────────────────────────────────────────

/// Self-contained [`PerpHost`]: typed live store + byte overlay + committed cross-block
/// map (bytes + decoded-Arc, mirroring reth's `canonical_perp`) + log sink. Intended for
/// standalone engine use and perf benches; block boundaries are driven explicitly via
/// [`InMemoryHost::end_block`].
#[derive(Debug, Default)]
pub struct InMemoryHost {
    /// Hot typed state (what the journal would hold on-chain).
    pub live: TypedPerpStore,
    /// In-block byte overlay for cold namespaces.
    pub overlay: std::collections::HashMap<B256, Vec<u8>>,
    /// Committed cross-block store: canonical bytes + optional decoded Arc.
    pub committed: std::collections::HashMap<B256, (Vec<u8>, Option<Arc<PerpBlob>>)>,
    /// External (ERC20) balances keyed by (token, owner).
    pub balances: std::collections::HashMap<(Address, Address), U256>,
    /// Emitted events.
    pub logs: Vec<Log>,
    /// Block timestamp returned by [`PerpHost::timestamp`].
    pub now: u64,
    /// Running perp-write witness (overlay writes; the typed store keeps its own count).
    overlay_writes: u64,
}

impl InMemoryHost {
    /// Fresh host with the given block timestamp.
    pub fn new(now: u64) -> Self {
        Self { now, ..Self::default() }
    }

    /// Ends the simulated block: harvests the net perp delta (typed-store dirty set +
    /// byte overlay), merges it into the committed store, and resets the live store —
    /// the same lifecycle the block executor drives on-chain. Returns the delta so the
    /// caller can fold it into a chained commitment (`perp_core::compute_block_commitment`).
    pub fn end_block(&mut self) -> PerpDelta {
        use context_interface::journaled_state::PerpStore;
        let mut delta = PerpStore::take_delta(&mut self.live);
        for (k, v) in self.overlay.drain() {
            delta.insert(k, PerpDeltaEntry { decoded: None, bytes: v });
        }
        for (k, e) in &delta {
            if e.bytes.is_empty() {
                self.committed.remove(k);
            } else {
                self.committed.insert(*k, (e.bytes.clone(), e.decoded.clone()));
            }
        }
        self.live = TypedPerpStore::default();
        delta
    }
}

impl PerpHost for InMemoryHost {
    /// Log-stream length: reverting truncates events emitted since the checkpoint.
    type Checkpoint = usize;

    fn live(&mut self) -> &mut TypedPerpStore {
        &mut self.live
    }

    fn perp_load(&mut self, key: B256) -> Result<Vec<u8>, PerpError> {
        if let Some(v) = self.overlay.get(&key) {
            return Ok(v.clone());
        }
        Ok(self.committed.get(&key).map(|(b, _)| b.clone()).unwrap_or_default())
    }

    fn perp_store(&mut self, key: B256, value: Vec<u8>) {
        self.overlay_writes += 1;
        self.overlay.insert(key, value);
    }

    fn perp_load_arc(&mut self, key: B256) -> Result<Option<Arc<PerpBlob>>, PerpError> {
        Ok(self.committed.get(&key).and_then(|(_, d)| d.clone()))
    }

    fn log(&mut self, log: Log) {
        self.logs.push(log);
    }

    fn timestamp(&self) -> u64 {
        self.now
    }

    fn external_balance(&mut self, token: Address, owner: Address) -> Result<U256, PerpError> {
        Ok(self.balances.get(&(token, owner)).copied().unwrap_or_default())
    }

    fn set_external_balance(
        &mut self,
        token: Address,
        owner: Address,
        value: U256,
    ) -> Result<(), PerpError> {
        self.balances.insert((token, owner), value);
        Ok(())
    }

    fn checkpoint(&mut self) -> usize {
        self.logs.len()
    }

    fn checkpoint_commit(&mut self) {}

    fn checkpoint_revert(&mut self, cp: usize) {
        self.logs.truncate(cp);
    }

    fn perp_write_count(&self) -> u64 {
        use context_interface::journaled_state::PerpStore;
        self.overlay_writes + self.live.write_count()
    }

    fn call_depth(&self) -> usize {
        // Standalone host: always a top-level call (the depth gate never fires).
        1
    }
}
