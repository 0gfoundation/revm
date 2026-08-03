//! PerpDEX precompile — on-chain perpetual futures exchange.
//!
//! # Address
//! `0x0000000000000000000000000000000000001003`
//!
//! # Module layout
//! ```text
//! perp_dex/
//! ├── mod.rs      ← you are here: PrecompileResult adapter over perp_engine::run_perp_dex_call
//! │                 + facade re-exports at the historical perp_dex::* paths
//! ├── storage.rs  ← facade over perp_engine::storage + the block-end commitment anchor
//! ├── errors.rs   ← PrecompileError constructors + From<PerpError>
//! └── prof.rs     ← bench-util stage profiler
//! The engine itself lives in the `perp-engine` crate; the state core in `perp-core`.
//! ```

use context::ContextTr;
use primitives::{Address, U256};

use crate::{PrecompileOutput, PrecompileResult};

pub mod errors;
#[cfg(any(feature = "bench-util", test))]
pub mod prof;
pub mod storage;

// The engine and state core live in the `perp-engine` / `perp-core` crates; re-exported
// at the historical `perp_dex::*` paths for in-crate consumers and external callers.
pub use perp_core::{math, typed_store, types};
pub use perp_engine::{account, batch, funding, interface, risk, trading};

// ── Constants ─────────────────────────────────────────────────────────────────

pub use perp_engine::{PERP_DEX_ADDRESS, USDC_ADDRESS};

pub use perp_engine::{CANCEL_ORDER_GAS, PLACE_ORDER_GAS};


// ── Entry point (adapter) ─────────────────────────────────────────────────────

/// Thin adapter over [`perp_engine::run_perp_dex_call`]: converts the engine's output and
/// error shapes into the revm precompile ABI (`PrecompileResult`). All perp logic — the
/// selector table, gas, the EOA depth gate, dispatch, and revert encoding — lives in the
/// engine crate; the revm context participates through the blanket `PerpHost` impl.
pub fn run_perp_dex_call<CTX: ContextTr>(
    input_bytes: &[u8],
    gas_limit: u64,
    caller: Address,
    value: U256,
    is_static: bool,
    context: &mut CTX,
) -> PrecompileResult {
    match perp_engine::run_perp_dex_call(input_bytes, gas_limit, caller, value, is_static, context)
    {
        Ok(o) if o.reverted => Ok(PrecompileOutput::new_reverted(o.gas_used, o.bytes)),
        Ok(o) => Ok(PrecompileOutput::new(o.gas_used, o.bytes)),
        Err(e) => Err(e.into()),
    }
}

/// Re-exported diagnostics for the commit-only write-then-revert tripwire — the query
/// function AND the two atomics themselves (both were `pub` at this path historically).
pub use perp_engine::call::{
    last_perp_write_then_revert, LAST_WRITE_THEN_REVERT_SELECTOR, PERP_WRITE_THEN_REVERT_COUNT,
};
