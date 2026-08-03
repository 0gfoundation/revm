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
//! ├── batch.rs          ← batch-call shell (pre-decode length, gas, statuses, abort-forward)
//! ├── math.rs           ← pure financial math functions
//! ├── types/            ← data structures (account, order, position, market)
//! ├── storage/          ← on-chain storage helpers
//! ├── account/          ← deposit / withdraw / transfers / getAccount
//! ├── trading/          ← order placement, cancellation, matching
//! └── risk/             ← markets, leverage, mark price, positions, liquidation
//! ```

use context::ContextTr;
use primitives::{Address, U256};

use perp_engine::{
    account::{
        run_deposit, run_get_account, run_get_api_key, run_get_api_keys,
        run_get_user_fee_rates, run_register_api_key, run_revoke_api_key,
        run_set_user_fee_rates, run_transfer_from_perp, run_transfer_to_perp, run_withdraw,
    },
    interface::IPerpDex::{
        addMarketCall, addPositionMarginCall, batchCancelOrdersCall,
            batchCancelOrdersSignedCall, batchPlaceOrdersCall, batchPlaceOrdersSignedCall,
            cancelOrderCall, cancelOrderSignedCall, depositCall, depositInsuranceFundCall,
            getAccountCall, getAdminCall, getApiKeyCall, getApiKeysCall,
            getAveragePremiumIndexCall, getBookLevelCall, getBookPricesCall, getFundingStateCall,
            getIndexPriceCall, getInsuranceFundCall, getMarkPriceCall, getMarketCall,
            getMarketFeeTotalCall, getMarketManagerAddressCall, getOpenOrdersCall,
            getOracleAddressCall, getOrderCall, getPositionCall, getUserFeeRatesCall,
            initAdminCall, liquidateCall, placeOrderCall, placeOrderSignedCall, registerApiKeyCall,
            removePositionMarginCall, revokeApiKeyCall, setLeverageCall, setLeverageSignedCall,
            setMarketManagerAddressCall, setOracleAddressCall, setUserFeeRatesCall,
            transferAdminCall, transferFromPerpCall, transferToPerpCall, updateIndexPriceCall,
            updateMarketCall, withdrawCall, withdrawInsuranceFundCall,
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
        run_batch_cancel_orders, run_batch_cancel_orders_signed, run_batch_place_orders,
        run_batch_place_orders_signed, run_cancel_order, run_cancel_order_signed,
        run_get_book_level, run_get_book_prices, run_get_market_fee_total, run_get_open_orders,
        run_get_order, run_place_order, run_place_order_signed,
    },
};

use crate::{PrecompileError, PrecompileOutput, PrecompileResult};

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

/// Re-exported diagnostics for the commit-only write-then-revert tripwire.
pub use perp_engine::call::last_perp_write_then_revert;
