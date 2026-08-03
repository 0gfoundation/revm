//! Per-block chained off-trie commitment (catalog #16d): the off-trie analogue of the
//! state root. The precompile shell folds this once at block end and anchors the result
//! in 0x1003's single on-trie storage slot.

use context_interface::journaled_state::PerpDelta;
use primitives::{B256, U256};

/// Version byte mixed into the per-block commitment hash (catalog #16d). Bumped to 3 at the
/// switch from the per-call chained v2 (retired) to the per-block net-delta fold; bumped to 4
/// at the switch from keccak-derived storage keys to direct-packed keys (catalog #12), so the
/// two key framings never alias across the consensus transition (a devnet wipe accompanies the
/// bump); bumped to 5 at the price-index switch from sorted `Vec<u64>` to `Vec<u64>`
/// (catalog #22) — the serialized price-level bytes change (container + order); bumped to 6 at the
/// order-lifecycle redesign (commit-only #23): delete-on-terminal removes filled/cancelled orders
/// from the map, the new per-level live-order count + lazy FIFO change the level-key set, and the
/// signed-order replay guard moved to the seen-signature namespace — all shift the net-delta bytes;
/// bumped to 7 grouping the five per-market hot scalars (mark price, best bid/ask, last traded,
/// open interest) into one `MarketHot` blob — the delta keys + framing regroup (values identical);
/// bumped to 8 folding the two per-user scalar keys (fee-rate bps, order nonce) into the account
/// blob — the account blob grows and their standalone keys disappear from the delta; bumped to 9
/// moving mark_price out of `MarketHot` into the `Market` blob (write-rare + co-read with config) —
/// both blobs' bytes change (Market gains a field, MarketHot loses one); bumped to 10 folding each
/// level's live-order count INTO its FIFO blob (`LevelBlob`, count(8 BE) prefix) — the per-level
/// count keys disappear and the level blob framing changes; bumped to 11 when total perp
/// collateral was appended to the account blob.
// #A: bumped 11→12 for the PerpPosition reservation-aggregate fields (tbq/tbn/tsq/tsn) — a CHAIN
// change (position blob layout changed → persisted state + commitment differ). Requires a golden
// re-pin (below) + a coordinated wipe on deploy.
pub const BLOCK_COMMITMENT_VERSION: u8 = 13;

/// Computes the per-BLOCK off-trie commitment over the block's NET delta (catalog #16d).
///
/// `C_block = blake3(C_prev ‖ BLOCK_COMMITMENT_VERSION ‖ Σ_sorted(key(32) ‖ len(u32 BE) ‖ value))`,
/// keys ascending. `delta` is the net block writes from [`JournalTr::take_perp_delta`] (already one
/// value per key, post-revert), so no coalescing is needed — only deterministic key-sorting (keys
/// are a total order; the HashMap is never iterated for the hash). An empty value is a deleted key,
/// framed with len 0 (same convention as the per-call path). Pure: the caller reads `C_prev` and
/// sstores the result. This commits the block's net STATE CHANGE; chained onto the previous block's
/// `C` it forms a block-granular running commitment, the off-trie analogue of the state root.
pub fn compute_block_commitment(prev: U256, delta: &PerpDelta) -> U256 {
    let mut keys: Vec<&B256> = delta.keys().collect();
    keys.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&prev.to_be_bytes::<32>());
    hasher.update(&[BLOCK_COMMITMENT_VERSION]);
    for key in keys {
        // Fold the canonical BYTES only — `decoded` never feeds the commitment (选项A), so the
        // byte-stream (and the on-trie anchor) is identical to the pre-Arc pipeline.
        let value = &delta[key].bytes;
        hasher.update(key.as_slice());
        hasher.update(&(value.len() as u32).to_be_bytes());
        hasher.update(value);
    }
    U256::from_be_bytes(*hasher.finalize().as_bytes())
}

