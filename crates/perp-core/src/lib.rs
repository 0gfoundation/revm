//! # perp-core
//!
//! 0G PerpDEX state core, extracted from the `revm-precompile` `perp_dex` module so the
//! matching engine can be consumed and benchmarked without building the EVM stack:
//!
//! - [`types`] — Market / Order / Position / UserAccount / oracle & funding state
//! - [`math`] — pure fixed-point financial math
//! - [`keys`] — packed off-trie storage keys (one `B256` per entity)
//! - [`codec`] — canonical positional-msgpack blob codec + the raw price-level codec
//! - [`typed_store`] — the strongly-typed per-execution off-trie store (`TypedPerpStore`)
//! - [`commitment`] — the per-block chained off-trie commitment fold
//!
//! The journal seam types ([`PerpStore`](context_interface::journaled_state::PerpStore),
//! `PerpDelta`, `PerpBlob`) stay in `revm-context-interface` — this crate depends on that
//! (small, logic-free) interface crate, so `revm-context` and reth are untouched.
//!
//! Canonical-bytes invariant: every serialized blob produced here feeds the chained block
//! commitment; field ORDER in [`types`] is layout-significant (positional msgpack — append
//! only, never reorder).

pub mod as_bin;
pub mod codec;
pub mod commitment;
pub mod error;
pub mod keys;
pub mod math;
pub mod typed_store;
pub mod types;

pub use commitment::{compute_block_commitment, BLOCK_COMMITMENT_VERSION};
pub use error::{perp_err, perp_fatal_invariant_err, perp_invariant_err, PerpError};
pub use typed_store::TypedPerpStore;
