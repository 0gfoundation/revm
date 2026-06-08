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
            run_deposit, run_get_account, run_get_api_key, run_get_api_keys,
            run_get_user_fee_rates, run_register_api_key, run_revoke_api_key,
            run_set_user_fee_rates, run_transfer_from_perp, run_transfer_to_perp, run_withdraw,
        },
        interface::IPerpDex::{
            addMarketCall, addPositionMarginCall, cancelOrderCall, cancelOrderSignedCall,
            depositCall, depositInsuranceFundCall, getAccountCall, getAdminCall, getApiKeyCall,
            getApiKeysCall, getAveragePremiumIndexCall, getBookLevelCall, getBookPricesCall,
            getFundingStateCall, getIndexPriceCall, getInsuranceFundCall, getMarkPriceCall,
            getMarketCall, getMarketFeeTotalCall, getMarketManagerAddressCall, getOpenOrdersCall,
            getOracleAddressCall, getOrderCall, getPositionCall, getUserFeeRatesCall, initAdminCall,
            liquidateCall, placeOrderCall, placeOrderSignedCall, registerApiKeyCall,
            removePositionMarginCall, revokeApiKeyCall, setLeverageCall, setLeverageSignedCall,
            setMarketManagerAddressCall, setOracleAddressCall, setUserFeeRatesCall,
            transferAdminCall, transferFromPerpCall,
            transferToPerpCall, updateIndexPriceCall, updateMarketCall, withdrawCall,
            withdrawInsuranceFundCall,
        },
        risk::{
            run_add_market, run_add_position_margin, run_deposit_insurance_fund, run_get_admin,
            run_get_average_premium_index, run_get_funding_state, run_get_index_price,
            run_get_insurance_fund, run_get_mark_price, run_get_market, run_get_market_manager,
            run_get_oracle_address, run_get_position, run_init_admin, run_liquidate,
            run_remove_position_margin, run_set_leverage, run_set_leverage_signed,
            run_set_market_manager, run_set_oracle_address, run_transfer_admin,
            run_update_index_price, run_update_market, run_withdraw_insurance_fund,
        },
        trading::{
            run_cancel_order, run_cancel_order_signed, run_get_book_level, run_get_book_prices,
            run_get_market_fee_total, run_get_open_orders, run_get_order, run_place_order,
            run_place_order_signed,
        },
    },
    PrecompileError, PrecompileOutput, PrecompileResult,
};

pub mod account;
pub mod errors;
pub mod funding;
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
        m.insert(initAdminCall::SELECTOR, (30_000, false));
        m.insert(transferAdminCall::SELECTOR, (30_000, false));
        m.insert(getAdminCall::SELECTOR, (5_000, true));
        // Account
        m.insert(depositCall::SELECTOR, (50_000, false));
        m.insert(withdrawCall::SELECTOR, (50_000, false));
        m.insert(transferToPerpCall::SELECTOR, (20_000, false));
        m.insert(transferFromPerpCall::SELECTOR, (20_000, false));
        m.insert(getAccountCall::SELECTOR, (5_000, true));
        m.insert(setUserFeeRatesCall::SELECTOR, (30_000, false));
        m.insert(getUserFeeRatesCall::SELECTOR, (5_000, true));
        m.insert(getMarketFeeTotalCall::SELECTOR, (5_000, true));
        // Roles
        m.insert(setMarketManagerAddressCall::SELECTOR, (30_000, false));
        m.insert(getMarketManagerAddressCall::SELECTOR, (5_000, true));
        m.insert(setOracleAddressCall::SELECTOR, (30_000, false));
        m.insert(getOracleAddressCall::SELECTOR, (5_000, true));
        // Market management
        m.insert(addMarketCall::SELECTOR, (100_000, false));
        m.insert(updateMarketCall::SELECTOR, (50_000, false));
        m.insert(getMarkPriceCall::SELECTOR, (5_000, true));
        m.insert(getMarketCall::SELECTOR, (5_000, true));
        // Leverage
        m.insert(setLeverageCall::SELECTOR, (20_000, false));
        m.insert(setLeverageSignedCall::SELECTOR, (20_000, false));
        // Trading
        m.insert(placeOrderCall::SELECTOR, (200_000, false));
        m.insert(cancelOrderCall::SELECTOR, (80_000, false));
        m.insert(getOrderCall::SELECTOR, (5_000, true));
        m.insert(getOpenOrdersCall::SELECTOR, (20_000, true));
        m.insert(getBookPricesCall::SELECTOR, (20_000, true));
        m.insert(getBookLevelCall::SELECTOR, (20_000, true));
        // Positions
        m.insert(getPositionCall::SELECTOR, (5_000, true));
        m.insert(addPositionMarginCall::SELECTOR, (30_000, false));
        m.insert(removePositionMarginCall::SELECTOR, (30_000, false));
        // Liquidation
        m.insert(liquidateCall::SELECTOR, (150_000, false));
        // API key management
        m.insert(registerApiKeyCall::SELECTOR, (30_000, false));
        m.insert(revokeApiKeyCall::SELECTOR, (20_000, false));
        m.insert(getApiKeyCall::SELECTOR, (5_000, true));
        m.insert(getApiKeysCall::SELECTOR, (10_000, true));
        // Signed order submission (relayer path)
        m.insert(placeOrderSignedCall::SELECTOR, (200_000, false));
        m.insert(cancelOrderSignedCall::SELECTOR, (80_000, false));
        // Insurance Fund
        m.insert(depositInsuranceFundCall::SELECTOR, (30_000, false));
        m.insert(withdrawInsuranceFundCall::SELECTOR, (30_000, false));
        m.insert(getInsuranceFundCall::SELECTOR, (5_000, true));
        // Index price
        m.insert(updateIndexPriceCall::SELECTOR, (50_000, false));
        m.insert(getIndexPriceCall::SELECTOR, (5_000, true));
        m.insert(getFundingStateCall::SELECTOR, (5_000, true));
        m.insert(getAveragePremiumIndexCall::SELECTOR, (5_000, true));
        m
    })
}

// ── Revert encoding ───────────────────────────────────────────────────────────

/// Encode `msg` as a Solidity `Error(string)` revert payload so that ethers.js
/// can decode `e.reason` from the returned bytes.
///
/// Layout: selector(4) | offset(32) | length(32) | data(padded to 32)
/// selector = keccak256("Error(string)")[0..4] = 0x08c379a0
fn encode_revert_string(msg: &str) -> primitives::Bytes {
    const SELECTOR: [u8; 4] = [0x08, 0xc3, 0x79, 0xa0];
    let msg_bytes = msg.as_bytes();
    let msg_len = msg_bytes.len();
    let padded = (msg_len + 31) / 32 * 32;

    let mut data = Vec::with_capacity(4 + 32 + 32 + padded);
    data.extend_from_slice(&SELECTOR);
    // offset to the string data = 0x20 (32)
    let mut buf = [0u8; 32];
    buf[31] = 0x20;
    data.extend_from_slice(&buf);
    // string length
    let mut buf = [0u8; 32];
    buf[24..].copy_from_slice(&(msg_len as u64).to_be_bytes());
    data.extend_from_slice(&buf);
    // string bytes, right-padded with zeros
    data.extend_from_slice(msg_bytes);
    data.resize(4 + 32 + 32 + padded, 0u8);

    primitives::Bytes::from(data)
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

    // Dispatch — note: no `?` here; errors are caught below and converted to
    // clean REVERT output so ethers.js can read `e.reason`.
    let result = match selector {
        // Admin
        s if s == initAdminCall::SELECTOR => run_init_admin(input_bytes, context),
        s if s == transferAdminCall::SELECTOR => run_transfer_admin(input_bytes, caller, context),
        s if s == getAdminCall::SELECTOR => run_get_admin(input_bytes, context),
        // Account
        s if s == depositCall::SELECTOR => run_deposit(input_bytes, caller, context),
        s if s == withdrawCall::SELECTOR => run_withdraw(input_bytes, caller, context),
        s if s == transferToPerpCall::SELECTOR => {
            run_transfer_to_perp(input_bytes, caller, context)
        }
        s if s == transferFromPerpCall::SELECTOR => {
            run_transfer_from_perp(input_bytes, caller, context)
        }
        s if s == getAccountCall::SELECTOR => run_get_account(input_bytes, context),
        s if s == setUserFeeRatesCall::SELECTOR => {
            run_set_user_fee_rates(input_bytes, caller, context)
        }
        s if s == getUserFeeRatesCall::SELECTOR => run_get_user_fee_rates(input_bytes, context),
        s if s == getMarketFeeTotalCall::SELECTOR => run_get_market_fee_total(input_bytes, context),
        // Roles
        s if s == setMarketManagerAddressCall::SELECTOR => {
            run_set_market_manager(input_bytes, caller, context)
        }
        s if s == getMarketManagerAddressCall::SELECTOR => {
            run_get_market_manager(input_bytes, context)
        }
        s if s == setOracleAddressCall::SELECTOR => {
            run_set_oracle_address(input_bytes, caller, context)
        }
        s if s == getOracleAddressCall::SELECTOR => run_get_oracle_address(input_bytes, context),
        // Market management
        s if s == addMarketCall::SELECTOR => run_add_market(input_bytes, caller, context),
        s if s == updateMarketCall::SELECTOR => run_update_market(input_bytes, caller, context),
        s if s == getMarkPriceCall::SELECTOR => run_get_mark_price(input_bytes, context),
        s if s == getMarketCall::SELECTOR => run_get_market(input_bytes, context),
        // Leverage
        s if s == setLeverageCall::SELECTOR => run_set_leverage(input_bytes, caller, context),
        s if s == setLeverageSignedCall::SELECTOR => run_set_leverage_signed(input_bytes, context),
        // Trading
        s if s == placeOrderCall::SELECTOR => run_place_order(input_bytes, caller, context),
        s if s == cancelOrderCall::SELECTOR => run_cancel_order(input_bytes, caller, context),
        s if s == getOrderCall::SELECTOR => run_get_order(input_bytes, context),
        s if s == getOpenOrdersCall::SELECTOR => run_get_open_orders(input_bytes, context),
        s if s == getBookPricesCall::SELECTOR => run_get_book_prices(input_bytes, context),
        s if s == getBookLevelCall::SELECTOR => run_get_book_level(input_bytes, context),
        // Positions
        s if s == getPositionCall::SELECTOR => run_get_position(input_bytes, context),
        s if s == addPositionMarginCall::SELECTOR => {
            run_add_position_margin(input_bytes, caller, context)
        }
        s if s == removePositionMarginCall::SELECTOR => {
            run_remove_position_margin(input_bytes, caller, context)
        }
        // Liquidation
        s if s == liquidateCall::SELECTOR => run_liquidate(input_bytes, caller, context),
        // API key management
        s if s == registerApiKeyCall::SELECTOR => {
            run_register_api_key(input_bytes, caller, context)
        }
        s if s == revokeApiKeyCall::SELECTOR => run_revoke_api_key(input_bytes, caller, context),
        s if s == getApiKeyCall::SELECTOR => run_get_api_key(input_bytes, context),
        s if s == getApiKeysCall::SELECTOR => run_get_api_keys(input_bytes, context),
        // Signed order submission (relayer path)
        s if s == placeOrderSignedCall::SELECTOR => run_place_order_signed(input_bytes, context),
        s if s == cancelOrderSignedCall::SELECTOR => run_cancel_order_signed(input_bytes, context),
        // Insurance Fund
        s if s == depositInsuranceFundCall::SELECTOR => {
            run_deposit_insurance_fund(input_bytes, caller, context)
        }
        s if s == withdrawInsuranceFundCall::SELECTOR => {
            run_withdraw_insurance_fund(input_bytes, caller, context)
        }
        s if s == getInsuranceFundCall::SELECTOR => run_get_insurance_fund(input_bytes, context),
        // Index price
        s if s == updateIndexPriceCall::SELECTOR => {
            run_update_index_price(input_bytes, caller, context)
        }
        s if s == getIndexPriceCall::SELECTOR => run_get_index_price(input_bytes, context),
        s if s == getFundingStateCall::SELECTOR => run_get_funding_state(input_bytes, context),
        s if s == getAveragePremiumIndexCall::SELECTOR => {
            run_get_average_premium_index(input_bytes, context)
        }
        _ => return Err(PrecompileError::StatefulInvalidInput),
    };

    match result {
        Ok(bytes) => Ok(PrecompileOutput::new(gas_used, bytes)),
        // Fatal errors propagate as-is (storage / system bugs).
        Err(PrecompileError::Fatal(e)) => Err(PrecompileError::Fatal(e)),
        // All other errors become a clean REVERT with an ABI-encoded reason
        // string, so ethers.js exposes `e.reason` to the caller.
        Err(e) => Ok(PrecompileOutput::new_reverted(
            gas_used,
            encode_revert_string(&e.to_string()),
        )),
    }
}
