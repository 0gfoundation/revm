//! Facade over the engine's storage layer (`perp_engine::storage`), plus the block-end
//! commitment anchor — the one storage operation that is genuinely the SHELL's: it writes
//! the chained off-trie commitment into 0x1003's single on-trie slot via the journal.

pub use perp_engine::storage::*;

use context::{journaled_state::PerpDelta, ContextTr, JournalTr};
use perp_core::keys::commitment_slot;
use perp_engine::PERP_DEX_ADDRESS;

use crate::{stateful_precompiles::convert_db_err, PrecompileError};

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
    delta: &PerpDelta,
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
