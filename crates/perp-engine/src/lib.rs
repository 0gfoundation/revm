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
pub mod algo;
pub mod batch;
pub mod call;
/// Derived event payloads — the single `PositionChanged` derivation shared by all seven emit
/// sites. Crate-private: nothing outside the engine constructs its own log.
mod events;
pub mod funding;
pub mod host;
pub mod interface;
pub mod reduce_only;
pub mod margin_type;
pub mod margin_view;
/// The independent cross-margin P0/P1 spec suite (`SPEC_cross_margin.md`).
#[cfg(test)]
mod cross_margin_spec_tests;
/// Cross margin P2a: the accounting core (funding mirror, account fields, the cross fold) and its
/// read surfaces.
#[cfg(test)]
mod cross_accounting_tests;
/// Cross margin P2b: money movement on a CROSSED position — fills, funding, liquidation and ADL
/// next to cross positions, J6 and the negative-wallet audit — under the custody identity.
#[cfg(test)]
mod cross_settlement_tests;
/// The independent cross-margin P2 spec suite (`SPEC_cross_margin.md` P2 ground rules, report §3.2
/// model): every surface against a model, every gate to the unit, cross liquidation and ADL.
#[cfg(test)]
mod cross_margin_p2_spec_tests;
/// Cross margin P3a: cross liquidation — account-level detection, the episode, its budget and
/// continuation, manual `liquidate()`, totality — under the custody identity.
#[cfg(test)]
mod cross_liquidation_tests;
/// The cross golden scenario (cross P3, R8): the commitment pin and business snapshot of cross
/// margin as it ships, beside the isolated `trading::tests::golden`.
#[cfg(test)]
mod cross_golden_tests;
/// The independent cross-margin P3 spec suite (`SPEC_cross_margin.md` R1–R8, report §1.5–§1.7 and
/// §3.4): Binance's liquidation-price examples, the episode, ADL, the negative-wallet clearance,
/// conservation after every call, and the never-`Err` fuzz.
#[cfg(test)]
mod cross_margin_p3_spec_tests;
/// Cross margin P4: the account's cross-leg summary (`types::CrossLegs`) on every route, and the
/// health check priced from it (`margin_view::cross_health`).
#[cfg(test)]
mod cross_legs_tests;
/// Cross margin P4: an independent spec suite — the cross legs and the health check recomputed
/// from the position records after every call, on every route, and at random.
#[cfg(test)]
mod cross_legs_spec_tests;
/// Real-engine event fixtures for the perpdex-perf harness: scenarios driven through the call
/// entry point on an `InMemoryHost`, recorded as receipts plus getter reads (`#[ignore]`d tool).
#[cfg(test)]
mod fixture_export_tests;
pub mod risk;
pub mod storage;
pub mod trading;
/// Trailing stops (`TRAILING_STOP_MARKET`): the grouped-lazy per-market structure and its sweeps.
pub mod trailing;

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
