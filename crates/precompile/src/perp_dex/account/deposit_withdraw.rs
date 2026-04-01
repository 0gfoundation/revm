//! Deposit and withdraw logic for the PerpDEX account system.
//!
//! Both operations manipulate ERC-20 storage **directly** (no external calls)
//! and keep the user's `UserAccount` in sync.

use alloy_sol_types::SolCall;
use context::ContextTr;
use primitives::{Address, Bytes, U256};

use crate::{
    perp_dex::{
        errors::perp_err,
        interface::IPerpDex::{depositCall, getAccountCall, withdrawCall},
        storage::{self, load_erc20_balance, save_erc20_balance},
        PERP_DEX_ADDRESS, USDC_ADDRESS,
    },
    PrecompileError,
};

/// `deposit(uint256 amount)` — pull USDC from caller → DEX, credit internal account.
pub fn run_deposit<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = depositCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("deposit: invalid calldata"))?;
    let amount = args.amount;

    if amount.is_zero() {
        return Err(perp_err("deposit: amount must be > 0"));
    }

    // ── 1. Check and deduct the caller's ERC-20 USDC balance ────────────────
    let user_usdc = load_erc20_balance(context, USDC_ADDRESS, caller)?;
    if user_usdc < amount {
        return Err(perp_err("deposit: insufficient USDC balance"));
    }
    save_erc20_balance(context, USDC_ADDRESS, caller, user_usdc - amount)?;

    // ── 2. Credit the DEX's ERC-20 USDC custody ─────────────────────────────
    let dex_usdc = load_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS)?;
    save_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS, dex_usdc + amount)?;

    // ── 3. Credit the caller's internal DEX account ──────────────────────────
    let mut account = storage::load_account(context, caller)?;
    let prev: U256 = account.usdc_balance.clone().into();
    account.usdc_balance = (prev + amount).into();
    storage::save_account(context, caller, account)?;

    Ok(Bytes::new())
}

/// `withdraw(uint256 amount)` — return USDC from DEX → caller, debit internal account.
pub fn run_withdraw<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = withdrawCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("withdraw: invalid calldata"))?;
    let amount = args.amount;

    if amount.is_zero() {
        return Err(perp_err("withdraw: amount must be > 0"));
    }

    // ── 1. Debit the caller's internal DEX account ───────────────────────────
    let mut account = storage::load_account(context, caller)?;
    let prev: U256 = account.usdc_balance.clone().into();
    if prev < amount {
        return Err(perp_err("withdraw: insufficient internal balance"));
    }
    account.usdc_balance = (prev - amount).into();
    storage::save_account(context, caller, account)?;

    // ── 2. Deduct the DEX's ERC-20 USDC custody ─────────────────────────────
    let dex_usdc = load_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS)?;
    save_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS, dex_usdc - amount)?;

    // ── 3. Return USDC to the caller's wallet ────────────────────────────────
    let user_usdc = load_erc20_balance(context, USDC_ADDRESS, caller)?;
    save_erc20_balance(context, USDC_ADDRESS, caller, user_usdc + amount)?;

    Ok(Bytes::new())
}

/// `getAccount(address user)` — read-only account snapshot.
pub fn run_get_account<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getAccountCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAccount: invalid calldata"))?;

    let account = storage::load_account(context, args.user)?;
    let usdc_balance: U256 = account.usdc_balance.into();

    Ok(Bytes::from(getAccountCall::abi_encode_returns(&usdc_balance)))
}
