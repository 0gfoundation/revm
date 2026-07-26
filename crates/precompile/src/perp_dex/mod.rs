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

use std::{collections::HashMap, sync::OnceLock};

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use context::{ContextTr, JournalTr};
use primitives::{address, Address, Log, U256};

use crate::{
    perp_dex::{
        account::{
            run_deposit, run_get_account, run_get_api_key, run_get_api_keys,
            run_get_user_fee_rates, run_register_api_key, run_revoke_api_key,
            run_set_user_fee_rates, run_transfer_from_perp, run_transfer_to_perp, run_withdraw,
        },
        interface::IPerpDex::{
            self, addMarketCall, addPositionMarginCall, batchCancelOrdersCall,
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
    },
    PrecompileError, PrecompileOutput, PrecompileResult,
};

pub mod account;
pub mod batch;
pub mod errors;
pub mod funding;
pub mod interface;
pub mod math;
pub mod risk;
pub mod storage;
pub mod trading;
pub mod typed_store;
pub mod types;

// ── Constants ─────────────────────────────────────────────────────────────────

/// PerpDEX precompile address.
pub const PERP_DEX_ADDRESS: Address = address!("0000000000000000000000000000000000001003");

/// USDC token address on this chain.
pub const USDC_ADDRESS: Address = address!("5ddA922Df9244b87635144e59D26f5A6e9FD90c3");

/// Flat gas of a single `cancelOrder` / `cancelOrderSigned`.
///
/// Defined once because it is ALSO the batch per-item unit: `batchCancelOrders` charges
/// `BASE_BATCH_GAS + N * CANCEL_ORDER_GAS`, so the two can never drift apart. The value is the
/// pre-existing table cost, unchanged (`batch_unit_matches_single_selector_cost` pins it).
pub const CANCEL_ORDER_GAS: u64 = 80_000;

/// Flat gas of a single `placeOrder` / `placeOrderSigned`.
///
/// Same rule as [`CANCEL_ORDER_GAS`]: this is ALSO the per-item unit of `batchPlaceOrders`
/// (`BASE_BATCH_GAS + N * PLACE_ORDER_GAS`), so the batch can never drift from the single selector.
/// The value is the pre-existing table cost, unchanged.
pub const PLACE_ORDER_GAS: u64 = 200_000;

// ── Selector table ────────────────────────────────────────────────────────────

/// `(gas_cost, can_be_called_in_static_context)`
///
/// For the `batch*` selectors `gas_cost` is only the **envelope floor** ([`batch::BASE_BATCH_GAS`]):
/// it drives the early `gas_cost > gas_limit` rejection and the static-context check, while the real
/// charge `BASE_BATCH_GAS + N * unit` is computed from the pre-decode array length in
/// [`run_perp_dex_call`] and is what lands in the returned [`PrecompileOutput`].
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
        m.insert(placeOrderCall::SELECTOR, (PLACE_ORDER_GAS, false));
        m.insert(cancelOrderCall::SELECTOR, (CANCEL_ORDER_GAS, false));
        // Batch place / cancel: floor only — see the doc comment on SELECTORS.
        m.insert(
            batchCancelOrdersCall::SELECTOR,
            (batch::BASE_BATCH_GAS, false),
        );
        m.insert(
            batchPlaceOrdersCall::SELECTOR,
            (batch::BASE_BATCH_GAS, false),
        );
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
        m.insert(placeOrderSignedCall::SELECTOR, (PLACE_ORDER_GAS, false));
        m.insert(cancelOrderSignedCall::SELECTOR, (CANCEL_ORDER_GAS, false));
        // Batch place / cancel (signed): floor only — see the doc comment on SELECTORS.
        m.insert(
            batchCancelOrdersSignedCall::SELECTOR,
            (batch::BASE_BATCH_GAS, false),
        );
        m.insert(
            batchPlaceOrdersSignedCall::SELECTOR,
            (batch::BASE_BATCH_GAS, false),
        );
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

/// Emits one final `AccountBalanceChanged` after-image per account whose public balance changed
/// during the call, in deterministic address order. These events are FREE — the call is charged
/// only the flat per-selector gas, exactly like `Trade` / `PositionChanged`.
fn emit_account_balance_after_images<CTX: ContextTr>(
    context: &mut CTX,
) -> Result<(), PrecompileError> {
    let initial_balances = storage::take_balance_tracking(context);
    for (user, initial) in initial_balances {
        let final_balance = storage::load_account_ref(context, user)?.public_balance();
        if final_balance == initial {
            continue;
        }
        context.journal_mut().log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::AccountBalanceChanged {
                user,
                usdcBalance: final_balance.usdc_balance,
                perpWalletBalance: final_balance.total_perp_collateral,
                availablePerpBalance: final_balance.available_perp_balance,
            }
            .to_log_data(),
        });
    }
    Ok(())
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
    // #16d: the perp commitment is folded ONCE at block end (`finalize_block_commitment` from the
    // block executor), not per call — so there is no per-call commitment work in this entry point.
    let selector: [u8; 4] = input_bytes
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or(PrecompileError::StatefulInvalidInput)?;

    let base_gas_used = match selectors_map().get(&selector) {
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

    // Dynamic-gas batch selectors. The table cost checked above is only the envelope FLOOR; the real
    // charge is `BASE_BATCH_GAS + N * unit`, where `N` is read PRE-DECODE from the calldata (this
    // decision has to precede ABI decoding). Done here — before the dispatch, before any write — so
    // an insufficient `gas_limit` is a clean `OutOfGas` with zero writes, exactly like the fixed-cost
    // check above. `None` means "not a batch selector", or "the length word is untrustworthy", in
    // which case only the floor is charged and the handler produces the clean revert.
    let mut charged_gas = base_gas_used;
    if let Some(cost) = batch::batch_dynamic_gas(selector, input_bytes) {
        if cost > gas_limit {
            return Err(PrecompileError::OutOfGas);
        }
        charged_gas = cost;
    }

    // commit-only #23: EOA-direct calls only. Perp writes are commit-only (the per-op undo is
    // being removed), so no enclosing frame that could revert AFTER a successful perp call may
    // exist. Frame depth: a top-level (tx-level) call executes at depth 1 (0 when unit tests
    // invoke this entry directly); ANY contract-mediated call — CALL or DELEGATECALL (which
    // spoofs caller==origin but still adds a frame) — is deeper and is rejected. The in-process
    // liquidation sweep is a plain function call inside this same frame, unaffected.
    if context.journal_mut().depth() > 1 {
        return Err(errors::perp_err("perpdex: EOA direct calls only"));
    }

    // commit-only #23 residual-write-then-error tripwire: snapshot the journal's perp-write counter
    // before dispatch. A call that ends REVERTED must not have written the overlay
    // (validate-then-apply); if it did, undo is gone and the write leaked. Diagnostic only.
    let writes_before = context.journal_mut().perp_write_count();
    storage::begin_balance_tracking(context);

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
        s if s == batchCancelOrdersCall::SELECTOR => {
            run_batch_cancel_orders(input_bytes, caller, context)
        }
        s if s == batchCancelOrdersSignedCall::SELECTOR => {
            run_batch_cancel_orders_signed(input_bytes, context)
        }
        s if s == batchPlaceOrdersCall::SELECTOR => {
            run_batch_place_orders(input_bytes, caller, context)
        }
        s if s == batchPlaceOrdersSignedCall::SELECTOR => {
            run_batch_place_orders_signed(input_bytes, context)
        }
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
        _ => Err(PrecompileError::StatefulInvalidInput),
    };

    match result {
        Ok(bytes) => {
            // Balance after-image events are FREE — the call is charged only `charged_gas`
            // (the flat per-selector cost, or `BASE_BATCH_GAS + N * unit` for a batch selector).
            emit_account_balance_after_images(context)?;
            Ok(PrecompileOutput::new(charged_gas, bytes))
        }
        // Fatal errors propagate as-is (storage / system bugs).
        Err(PrecompileError::Fatal(error)) => {
            storage::discard_balance_tracking(context);
            Err(PrecompileError::Fatal(error))
        }
        // All other errors become a clean REVERT with an ABI-encoded reason
        // string, so ethers.js exposes `e.reason` to the caller.
        Err(error) => {
            storage::discard_balance_tracking(context);
            // Tripwire: a reverting call that WROTE the overlay is a residual write-then-error
            // (commit-only #23 — the write leaks with no undo). Record the offending selector +
            // count into a global so the exact path can be surfaced (read via
            // [`last_perp_write_then_revert`]); diagnostic only, not a halt.
            let writes_after = context.journal_mut().perp_write_count();
            if writes_after != writes_before {
                let sel = u32::from_be_bytes(selector);
                LAST_WRITE_THEN_REVERT_SELECTOR.store(sel, core::sync::atomic::Ordering::Relaxed);
                PERP_WRITE_THEN_REVERT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            Ok(PrecompileOutput::new_reverted(
                charged_gas,
                encode_revert_string(&error.to_string()),
            ))
        }
    }
}

/// commit-only #23 tripwire state: the last selector that reverted AFTER writing the overlay
/// (a residual write-then-error), and how many such events have occurred process-wide.
pub static LAST_WRITE_THEN_REVERT_SELECTOR: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);
pub static PERP_WRITE_THEN_REVERT_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Returns `(count, last_selector)` of residual write-then-error reverts seen so far.
pub fn last_perp_write_then_revert() -> (u64, u32) {
    (
        PERP_WRITE_THEN_REVERT_COUNT.load(core::sync::atomic::Ordering::Relaxed),
        LAST_WRITE_THEN_REVERT_SELECTOR.load(core::sync::atomic::Ordering::Relaxed),
    )
}
