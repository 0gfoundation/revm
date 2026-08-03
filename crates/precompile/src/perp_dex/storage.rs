//! Facade over the engine's storage layer (`perp_engine::storage`), plus the block-end
//! commitment anchor — the one storage operation that is genuinely the SHELL's: it writes
//! the chained off-trie commitment into 0x1003's single on-trie slot via the journal.

pub use perp_engine::storage::*;

use context::{journaled_state::PerpDelta, ContextTr};

use crate::PrecompileError;

/// Block-end hook: delegates to [`perp_engine::storage::finalize_block_commitment`] (the
/// single implementation, pinned by the engine's golden chain test) and converts the error
/// shape at the precompile boundary. Import path kept stable for alloy-evm.
pub fn finalize_block_commitment<CTX: ContextTr>(
    context: &mut CTX,
    delta: &PerpDelta,
) -> Result<(), PrecompileError> {
    perp_engine::storage::finalize_block_commitment(context, delta).map_err(Into::into)
}
