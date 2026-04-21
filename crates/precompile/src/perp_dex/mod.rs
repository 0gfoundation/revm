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
//! ├── math.rs           ← pure financial math functions
//! ├── types/            ← data structures (account, order, position, market)
//! ├── storage/          ← on-chain storage helpers
//! ├── account/          ← deposit / withdraw / transfers / getAccount
//! ├── trading/          ← order placement, cancellation, matching
//! └── risk/             ← markets, leverage, mark price, positions, liquidation
//! ```

use std::{collections::HashMap, sync::OnceLock};

use alloy_sol_types::SolCall;
use context::ContextTr;
use primitives::{address, Address, U256};

use crate::{
    perp_dex::{
        account::{
            run_deposit, run_get_account, run_transfer_from_perp, run_transfer_to_perp,
            run_withdraw,
        },
        interface::IPerpDex::{
            addMarketCall, cancelOrderCall, depositCall, getAccountCall, getAdminCall,
            getMarkPriceCall, getOrderCall, getPositionCall, initAdminCall, liquidateCall,
            placeOrderCall, setLeverageCall, setMarkPriceCall, transferAdminCall,
            transferFromPerpCall, transferToPerpCall, withdrawCall,
        },
        risk::{
            run_add_market, run_get_admin, run_get_mark_price, run_get_position, run_init_admin,
            run_liquidate, run_set_leverage, run_set_mark_price, run_transfer_admin,
        },
        trading::{run_cancel_order, run_get_order, run_place_order},
    },
    PrecompileError, PrecompileOutput, PrecompileResult,
};

pub mod account;
pub mod errors;
pub mod interface;
pub mod math;
pub mod risk;
pub mod storage;
pub mod trading;
pub mod types;

// ── Constants ─────────────────────────────────────────────────────────────────

/// PerpDEX precompile address.
pub const PERP_DEX_ADDRESS: Address = address!("0000000000000000000000000000000000001003");

/// USDC token address on this chain.
pub const USDC_ADDRESS: Address = address!("5ddA922Df9244b87635144e59D26f5A6e9FD90c3");

// ── Selector table ────────────────────────────────────────────────────────────

/// `(gas_cost, can_be_called_in_static_context)`
static SELECTORS: OnceLock<HashMap<[u8; 4], (u64, bool)>> = OnceLock::new();

fn selectors_map() -> &'static HashMap<[u8; 4], (u64, bool)> {
    SELECTORS.get_or_init(|| {
        let mut m = HashMap::new();
        // Admin
        m.insert(initAdminCall::SELECTOR,        (30_000, false));
        m.insert(transferAdminCall::SELECTOR,    (30_000, false));
        m.insert(getAdminCall::SELECTOR,         (5_000,  true));
        // Account
        m.insert(depositCall::SELECTOR,          (50_000, false));
        m.insert(withdrawCall::SELECTOR,         (50_000, false));
        m.insert(transferToPerpCall::SELECTOR,   (20_000, false));
        m.insert(transferFromPerpCall::SELECTOR, (20_000, false));
        m.insert(getAccountCall::SELECTOR,       (5_000,  true));
        // Market management (admin)
        m.insert(addMarketCall::SELECTOR,        (100_000, false));
        m.insert(setMarkPriceCall::SELECTOR,     (30_000,  false));
        m.insert(getMarkPriceCall::SELECTOR,     (5_000,   true));
        // Leverage
        m.insert(setLeverageCall::SELECTOR,      (20_000, false));
        // Trading
        m.insert(placeOrderCall::SELECTOR,       (200_000, false));
        m.insert(cancelOrderCall::SELECTOR,      (80_000,  false));
        m.insert(getOrderCall::SELECTOR,         (5_000,   true));
        // Positions
        m.insert(getPositionCall::SELECTOR,      (5_000, true));
        // Liquidation
        m.insert(liquidateCall::SELECTOR,        (150_000, false));
        m
    })
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_perp_dex_call<CTX: ContextTr>(
    input_bytes: &[u8],
    gas_limit: u64,
    caller: Address,
    _value: U256,
    is_static: bool,
    context: &mut CTX,
) -> PrecompileResult {
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
        // Admin
        s if s == initAdminCall::SELECTOR        => run_init_admin(input_bytes, context)?,
        s if s == transferAdminCall::SELECTOR    => run_transfer_admin(input_bytes, caller, context)?,
        s if s == getAdminCall::SELECTOR         => run_get_admin(input_bytes, context)?,
        // Account
        s if s == depositCall::SELECTOR          => run_deposit(input_bytes, caller, context)?,
        s if s == withdrawCall::SELECTOR         => run_withdraw(input_bytes, caller, context)?,
        s if s == transferToPerpCall::SELECTOR   => run_transfer_to_perp(input_bytes, caller, context)?,
        s if s == transferFromPerpCall::SELECTOR => run_transfer_from_perp(input_bytes, caller, context)?,
        s if s == getAccountCall::SELECTOR       => run_get_account(input_bytes, context)?,
        // Market management
        s if s == addMarketCall::SELECTOR        => run_add_market(input_bytes, caller, context)?,
        s if s == setMarkPriceCall::SELECTOR     => run_set_mark_price(input_bytes, caller, context)?,
        s if s == getMarkPriceCall::SELECTOR     => run_get_mark_price(input_bytes, context)?,
        // Leverage
        s if s == setLeverageCall::SELECTOR      => run_set_leverage(input_bytes, caller, context)?,
        // Trading
        s if s == placeOrderCall::SELECTOR       => run_place_order(input_bytes, caller, context)?,
        s if s == cancelOrderCall::SELECTOR      => run_cancel_order(input_bytes, caller, context)?,
        s if s == getOrderCall::SELECTOR         => run_get_order(input_bytes, context)?,
        // Positions
        s if s == getPositionCall::SELECTOR      => run_get_position(input_bytes, context)?,
        // Liquidation
        s if s == liquidateCall::SELECTOR        => run_liquidate(input_bytes, caller, context)?,
        _ => return Err(PrecompileError::StatefulInvalidInput),
    };

    Ok(PrecompileOutput::new(gas_used, output))
}
