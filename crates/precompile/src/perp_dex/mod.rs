//! PerpDEX precompile — on-chain perpetual futures exchange.
//!
//! # Address
//! `0x0000000000000000000000000000000000001003`
//!
//! # Module layout
//! ```text
//! perp_dex/
//! ├── mod.rs            ← you are here (selector routing, entry point)
//! ├── interface.rs      ← Solidity ABI (sol! macro)
//! ├── errors.rs         ← perp_err helper
//! ├── types/
//! │   ├── account.rs    ← UserAccount (msgpack-serialised)
//! │   ├── order.rs      ← Order types (Step 2)
//! │   └── position.rs   ← Position types (Step 2)
//! ├── storage/
//! │   ├── keys.rs       ← storage key derivation
//! │   └── mod.rs        ← account + ERC-20 storage helpers
//! ├── account/
//! │   ├── deposit_withdraw.rs  ← deposit / withdraw / getAccount
//! │   └── mod.rs
//! ├── trading/mod.rs    ← (Step 2) order book, matching
//! └── risk/mod.rs       ← (Step 3) margin, liquidation, funding
//! ```

use std::{collections::HashMap, sync::OnceLock};

use alloy_sol_types::SolCall;
use context::ContextTr;
use primitives::{address, Address, U256};

use crate::{
    perp_dex::{
        account::{run_deposit, run_get_account, run_withdraw},
        interface::IPerpDex::{depositCall, getAccountCall, withdrawCall},
    },
    PrecompileError, PrecompileOutput, PrecompileResult,
};

pub mod account;
pub mod errors;
pub mod interface;
pub mod risk;
pub mod storage;
pub mod trading;
pub mod types;

// ── Constants ────────────────────────────────────────────────────────────────

/// PerpDEX precompile address.
pub const PERP_DEX_ADDRESS: Address = address!("0000000000000000000000000000000000001003");

/// USDC token address on this chain.
///
/// **Update this to the actual deployed USDC address before going to mainnet.**
pub const USDC_ADDRESS: Address = address!("06eFdBFf2a14a7c8E15944D1F4A48F9F95F663A4");

// ── Selector table ────────────────────────────────────────────────────────────

/// `selector → (gas_cost, can_be_called_in_static_context)`
static SELECTORS: OnceLock<HashMap<[u8; 4], (u64, bool)>> = OnceLock::new();

fn selectors_map() -> &'static HashMap<[u8; 4], (u64, bool)> {
    SELECTORS.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert(depositCall::SELECTOR, (50_000, false));
        m.insert(withdrawCall::SELECTOR, (50_000, false));
        m.insert(getAccountCall::SELECTOR, (5_000, true));
        m
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Main dispatch function called by `stateful_precompiles::run_stateful_precompile`.
pub fn run_perp_dex_call<CTX: ContextTr>(
    input_bytes: &[u8],
    gas_limit: u64,
    caller: Address,
    _value: U256,
    is_static: bool,
    context: &mut CTX,
) -> PrecompileResult {
    // Need at least a 4-byte selector.
    let selector: [u8; 4] = input_bytes
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or(PrecompileError::StatefulInvalidInput)?;

    let gas_used = match selectors_map().get(&selector) {
        Some(&(gas_cost, can_be_static)) => {
            if gas_cost > gas_limit {
                return Err(PrecompileError::OutOfGas);
            }
            if is_static && !can_be_static {
                return Err(PrecompileError::StaticRestrictionViolation);
            }
            gas_cost
        }
        None => return Err(PrecompileError::StatefulInvalidInput),
    };

    let output = match selector {
        s if s == depositCall::SELECTOR => run_deposit(input_bytes, caller, context)?,
        s if s == withdrawCall::SELECTOR => run_withdraw(input_bytes, caller, context)?,
        s if s == getAccountCall::SELECTOR => run_get_account(input_bytes, context)?,
        _ => return Err(PrecompileError::StatefulInvalidInput),
    };

    Ok(PrecompileOutput::new(gas_used, output))
}
