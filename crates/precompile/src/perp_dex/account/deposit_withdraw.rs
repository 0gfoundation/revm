//! Deposit, withdraw, and inter-wallet transfer logic for the PerpDEX.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use context::{ContextTr, JournalTr};
use primitives::{Address, Bytes, Log, U256};

use crate::{
    perp_dex::{
        errors::perp_err,
        interface::IPerpDex::{
            self, depositCall, getAccountCall, getAccountReturn, transferFromPerpCall,
            transferToPerpCall, withdrawCall, TransferFromPerp, TransferToPerp,
        },
        storage::{self, load_erc20_balance, save_erc20_balance},
        types::MAX_PERP_WALLET_BALANCE,
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

    // 1. Check and deduct the caller's ERC-20 USDC balance.
    let user_usdc = load_erc20_balance(context, USDC_ADDRESS, caller)?;
    if user_usdc < amount {
        return Err(perp_err("deposit: insufficient USDC balance"));
    }
    save_erc20_balance(context, USDC_ADDRESS, caller, user_usdc - amount)?;

    // 2. Credit the DEX's ERC-20 USDC custody.
    let dex_usdc = load_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS)?;
    save_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS, dex_usdc + amount)?;

    // 3. Credit the caller's internal spot balance.
    let mut account = storage::load_account(context, caller)?;
    let prev: U256 = account.usdc_balance.clone().into();
    let new_balance = prev + amount;
    if new_balance > U256::from(MAX_PERP_WALLET_BALANCE) {
        return Err(perp_err("deposit: total balance would exceed i64::MAX"));
    }
    account.usdc_balance = new_balance.into();
    storage::save_account(context, caller, account)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Deposit {
            user: caller,
            amount,
        }
        .to_log_data(),
    });

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

    // 1. Debit the caller's internal spot balance.
    let mut account = storage::load_account(context, caller)?;
    let prev: U256 = account.usdc_balance.clone().into();
    if prev < amount {
        return Err(perp_err("withdraw: insufficient internal balance"));
    }
    account.usdc_balance = (prev - amount).into();
    storage::save_account(context, caller, account)?;

    // 2. Deduct the DEX's ERC-20 USDC custody.
    let dex_usdc = load_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS)?;
    save_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS, dex_usdc - amount)?;

    // 3. Return USDC to the caller's wallet.
    let user_usdc = load_erc20_balance(context, USDC_ADDRESS, caller)?;
    save_erc20_balance(context, USDC_ADDRESS, caller, user_usdc + amount)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Withdraw {
            user: caller,
            amount,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `transferToPerp(uint64 amount)` — move USDC from spot balance → perp trading wallet.
pub fn run_transfer_to_perp<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = transferToPerpCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("transferToPerp: invalid calldata"))?;
    let amount = args.amount;

    if amount == 0 {
        return Err(perp_err("transferToPerp: amount must be > 0"));
    }

    let mut account = storage::load_account(context, caller)?;
    let spot: U256 = account.usdc_balance.clone().into();
    let amount_u256 = U256::from(amount);
    if spot < amount_u256 {
        return Err(perp_err("transferToPerp: insufficient spot balance"));
    }
    account.usdc_balance = (spot - amount_u256).into();
    account.credit_perp(amount)?;
    storage::save_account(context, caller, account)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: TransferToPerp {
            user: caller,
            amount,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `transferFromPerp(uint64 amount)` — move USDC from perp trading wallet → spot balance.
pub fn run_transfer_from_perp<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = transferFromPerpCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("transferFromPerp: invalid calldata"))?;
    let amount = args.amount;

    if amount == 0 {
        return Err(perp_err("transferFromPerp: amount must be > 0"));
    }

    let mut account = storage::load_account(context, caller)?;
    if !account.has_available_perp(amount) {
        return Err(perp_err(
            "transferFromPerp: insufficient perp wallet balance",
        ));
    }
    account.debit_perp(amount)?;
    let spot: U256 = account.usdc_balance.clone().into();
    account.usdc_balance = (spot + U256::from(amount)).into();
    storage::save_account(context, caller, account)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: TransferFromPerp {
            user: caller,
            amount,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `getAccount(address user)` — returns `(usdcBalance, perpWalletBalance)`.
pub fn run_get_account<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getAccountCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAccount: invalid calldata"))?;

    let account = storage::load_account(context, args.user)?;
    let usdc_balance: U256 = account.usdc_balance.clone().into();
    let perp_wallet_balance = account.visible_perp_wallet_balance();

    Ok(Bytes::from(getAccountCall::abi_encode_returns(
        &getAccountReturn {
            usdcBalance: usdc_balance,
            perpWalletBalance: perp_wallet_balance,
        },
    )))
}

#[cfg(test)]
#[path = "deposit_withdraw_tests.rs"]
mod tests;
