//! # perp-engine
//!
//! The 0G PerpDEX matching engine — order placement / cancellation / matching,
//! settlement, liquidation, risk and funding — extracted from the `revm-precompile`
//! `perp_dex` module and made generic over a [`host::PerpHost`].
//!
//! ## Consuming the engine
//!
//! - **On-chain (revm)**: every `CTX: ContextTr` *is* a `PerpHost` via the blanket impl
//!   in [`host`], so the precompile shell and any revm context call the `run_*` entry
//!   points directly. The shell (selector routing, gas, EOA depth gate, revert encoding,
//!   the block-end commitment anchor) stays in `revm-precompile`.
//! - **Standalone (perf benches, other services)**: use [`host::InMemoryHost`] — no EVM
//!   crates are built beyond the small `revm-context-interface` seam.
//!
//! Errors are [`perp_core::PerpError`] (`Reject` = clean revert at the precompile
//! boundary, `Fatal` = halt). State lives in `perp-core` (`types`, `typed_store`,
//! codec, keys, block commitment).

pub mod account;
pub mod batch;
pub mod call;
mod events;
pub mod funding;
pub mod host;
pub mod interface;
pub mod risk;
pub mod storage;
pub mod trading;

pub use call::{run_perp_dex_call, PerpOutput};
pub use host::{InMemoryHost, PerpHost};
pub use perp_core::PerpError;

// The state core, re-exported at the historical module paths so engine-internal
// `crate::{math, types, typed_store, errors}` imports resolve unchanged.
pub use perp_core::{math, typed_store, types};

/// Error constructors (historical `errors::perp_err` path).
pub mod errors {
    pub use perp_core::error::{perp_err, perp_fatal_invariant_err, perp_invariant_err};
    pub use perp_core::PerpError;
}

use primitives::{address, Address};

/// PerpDEX precompile address (the engine emits its events from this address).
pub const PERP_DEX_ADDRESS: Address = address!("0000000000000000000000000000000000001003");

/// USDC token address on this chain (the deposit/withdraw external-balance bridge).
pub const USDC_ADDRESS: Address = address!("5ddA922Df9244b87635144e59D26f5A6e9FD90c3");

/// Flat gas of a single `cancelOrder` / `cancelOrderSigned`.
///
/// Defined once because it is ALSO the batch per-item unit: `batchCancelOrders` charges
/// `BASE_BATCH_GAS + N * CANCEL_ORDER_GAS`, so the two can never drift apart.
pub const CANCEL_ORDER_GAS: u64 = 80_000;

/// Flat gas of a single `placeOrder` / `placeOrderSigned` (also the batch per-item unit).
pub const PLACE_ORDER_GAS: u64 = 200_000;
