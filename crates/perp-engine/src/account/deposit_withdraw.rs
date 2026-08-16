//! Deposit, withdraw, and inter-wallet transfer logic for the PerpDEX.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use crate::host::PerpHost;
use primitives::{Address, Bytes, Log, U256};

use crate::{
        errors::{perp_err, perp_invariant_err},
    interface::IPerpDex::{
        self, depositCall, getAccountCall, getAccountReturn, transferFromPerpCall,
        transferToPerpCall, withdrawCall, TransferFromPerp, TransferToPerp,
    },
    storage::{self, load_erc20_balance, save_erc20_balance},
    types::MAX_PERP_WALLET_BALANCE,
    PERP_DEX_ADDRESS, USDC_ADDRESS,
    PerpError,
};

/// `deposit(uint256 amount)` — pull USDC from caller → DEX, credit internal account.
pub fn run_deposit<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = depositCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("deposit: invalid calldata"))?;
    let amount = args.amount;

    if amount.is_zero() {
        return Err(perp_err("deposit: amount must be > 0"));
    }

    // ── VALIDATE + COMPUTE (commit-only #23: all fallible logic BEFORE any write) ──────────
    // Load balances and run every reject up front, so a rejected deposit leaves ZERO writes
    // (previously the two ERC-20 legs moved before the MAX check → a reject stranded custody:
    // USDC-loss under commit-only).
    let user_usdc = load_erc20_balance(context, USDC_ADDRESS, caller)?;
    if user_usdc < amount {
        return Err(perp_err("deposit: insufficient USDC balance"));
    }
    let dex_usdc = load_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS)?;
    let mut account = storage::load_account(context, caller)?;
    let prev: U256 = account.usdc_balance.clone().into();
    let new_balance = prev + amount;
    if new_balance > U256::from(MAX_PERP_WALLET_BALANCE) {
        return Err(perp_err("deposit: total balance would exceed i64::MAX"));
    }

    // ── APPLY (no logic reject past this point; only DB-error `?`, which aborts the block) ──
    save_erc20_balance(context, USDC_ADDRESS, caller, user_usdc - amount)?; // 1. debit caller USDC
    save_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS, dex_usdc + amount)?; // 2. credit DEX custody
    account.usdc_balance = new_balance.into(); // 3. credit internal spot balance
    storage::save_account(context, caller, account)?;

    context.log(Log {
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
pub fn run_withdraw<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = withdrawCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("withdraw: invalid calldata"))?;
    let amount = args.amount;

    if amount.is_zero() {
        return Err(perp_err("withdraw: amount must be > 0"));
    }

    // ── VALIDATE + COMPUTE (commit-only #23: all fallible logic + loads BEFORE any write) ──
    let mut account = storage::load_account(context, caller)?;
    let prev: U256 = account.usdc_balance.clone().into();
    if prev < amount {
        return Err(perp_err("withdraw: insufficient internal balance"));
    }
    let dex_usdc = load_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS)?;
    let user_usdc = load_erc20_balance(context, USDC_ADDRESS, caller)?;
    // Custody invariant: the DEX always holds >= the sum of internal balances, so dex >= amount.
    // Reject before any write rather than underflow `dex_usdc - amount` after the account debit.
    if dex_usdc < amount {
        return Err(perp_invariant_err(
            "withdraw: DEX custody below withdrawal amount",
        ));
    }

    // ── APPLY (no logic reject past this point; only DB-error `?`, which aborts the block) ──
    account.usdc_balance = (prev - amount).into(); // 1. debit internal spot balance
    storage::save_account(context, caller, account)?;
    save_erc20_balance(context, USDC_ADDRESS, PERP_DEX_ADDRESS, dex_usdc - amount)?; // 2. debit DEX custody
    save_erc20_balance(context, USDC_ADDRESS, caller, user_usdc + amount)?; // 3. return USDC to caller

    context.log(Log {
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
pub fn run_transfer_to_perp<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
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

    context.log(Log {
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
pub fn run_transfer_from_perp<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = transferFromPerpCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("transferFromPerp: invalid calldata"))?;
    let amount = args.amount;

    if amount == 0 {
        return Err(perp_err("transferFromPerp: amount must be > 0"));
    }

    let mut account = storage::load_account(context, caller)?;
    // Derived-ooIM Phase 1 dual gate (debug only). Cash leaving the perp wallet: Σ ooIM is
    // untouched, so the derived requirement is the same `amount`; only the AVAILABLE differs
    // (`+ Σ margin_reserved − Σ ooIM`). This is the money-OUT gate — the one where an
    // over-permissive derived basis would let a user strip collateral out from under resting
    // orders — so it matters most that the two agree.
    #[cfg(debug_assertions)]
    crate::margin_view::debug_assert_gates_agree(
        context,
        caller,
        "transferFromPerp",
        None,
        amount,
        amount as i128,
    );
    if !account.has_available_perp(amount) {
        return Err(perp_err(
            "transferFromPerp: insufficient perp wallet balance",
        ));
    }
    account.debit_perp(amount)?;
    let spot: U256 = account.usdc_balance.clone().into();
    account.usdc_balance = (spot + U256::from(amount)).into();
    storage::save_account(context, caller, account)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: TransferFromPerp {
            user: caller,
            amount,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `getAccount(address user)` — returns spot, total perp collateral, and available perp.
pub fn run_get_account<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getAccountCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAccount: invalid calldata"))?;

    let account = storage::load_account_ref(context, args.user)?;
    let usdc_balance: U256 = account.usdc_balance.clone().into();
    let available_perp_balance = account.visible_perp_wallet_balance();

    Ok(Bytes::from(getAccountCall::abi_encode_returns(
        &getAccountReturn {
            usdcBalance: usdc_balance,
            availablePerpBalance: available_perp_balance,
        },
    )))
}

#[cfg(test)]
#[path = "deposit_withdraw_tests.rs"]
mod tests;
