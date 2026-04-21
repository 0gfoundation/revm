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
            transferToPerpCall, withdrawCall,
        },
        storage::{self, load_erc20_balance, save_erc20_balance},
        PERP_DEX_ADDRESS, USDC_ADDRESS,
    },
    PrecompileError,
};

// Note: TransferToPerp and TransferFromPerp do not emit events — they are
// internal wallet movements with no corresponding REST API endpoint.

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
    if new_balance > U256::from(u64::MAX) {
        return Err(perp_err("deposit: total balance would exceed u64::MAX"));
    }
    account.usdc_balance = new_balance.into();
    storage::save_account(context, caller, account)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Deposit { user: caller, amount }.to_log_data(),
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
        data: IPerpDex::Withdraw { user: caller, amount }.to_log_data(),
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
    account.perp_wallet_balance = account
        .perp_wallet_balance
        .checked_add(amount)
        .ok_or_else(|| perp_err("transferToPerp: overflow"))?;
    storage::save_account(context, caller, account)?;
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
    if account.perp_wallet_balance < amount {
        return Err(perp_err("transferFromPerp: insufficient perp wallet balance"));
    }
    account.perp_wallet_balance -= amount;
    let spot: U256 = account.usdc_balance.clone().into();
    account.usdc_balance = (spot + U256::from(amount)).into();
    storage::save_account(context, caller, account)?;
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
    let usdc_balance: U256 = account.usdc_balance.into();
    let perp_wallet_balance = account.perp_wallet_balance;

    Ok(Bytes::from(getAccountCall::abi_encode_returns(
        &getAccountReturn { usdcBalance: usdc_balance, perpWalletBalance: perp_wallet_balance },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolCall;
    use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
    use database::InMemoryDB;
    use primitives::{address, hardfork::SpecId};

    use crate::perp_dex::{
        interface::IPerpDex::{depositCall, getAccountCall, transferFromPerpCall, transferToPerpCall, withdrawCall},
        storage::keys::erc20_balance_slot,
        USDC_ADDRESS,
    };

    const ALICE: Address = address!("1111111111111111111111111111111111111111");

    type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

    fn make_ctx(alice_usdc: U256) -> TestCtx {
        let mut db = InMemoryDB::default();
        db.insert_account_storage(
            USDC_ADDRESS,
            erc20_balance_slot(ALICE).into(),
            alice_usdc,
        )
        .unwrap();
        let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
        for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE] {
            JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
        }
        ctx
    }

    fn decode_get_account(bytes: &Bytes) -> (U256, u64) {
        let usdc = U256::from_be_slice(&bytes[..32]);
        let perp = U256::from_be_slice(&bytes[32..64]).to::<u64>();
        (usdc, perp)
    }

    #[test]
    fn deposit_moves_usdc_to_internal_account() {
        let amount = U256::from(1_000_000u64);
        let mut ctx = make_ctx(amount);
        run_deposit(&depositCall { amount }.abi_encode(), ALICE, &mut ctx).unwrap();
        let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
        let (usdc, _perp) = decode_get_account(&ret);
        assert_eq!(usdc, amount);
    }

    #[test]
    fn deposit_rejects_zero_amount() {
        let mut ctx = make_ctx(U256::from(1_000_000u64));
        let err = run_deposit(&depositCall { amount: U256::ZERO }.abi_encode(), ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("amount must be > 0"), "{err}");
    }

    #[test]
    fn deposit_rejects_when_total_would_exceed_u64_max() {
        // Two deposits of 2/3 * u64::MAX each: individually valid, cumulatively overflows.
        let two_thirds = U256::from(u64::MAX / 3 * 2);
        let mut ctx = make_ctx(U256::MAX);
        run_deposit(&depositCall { amount: two_thirds }.abi_encode(), ALICE, &mut ctx).unwrap();
        let err = run_deposit(&depositCall { amount: two_thirds }.abi_encode(), ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("exceed u64::MAX"), "{err}");
    }

    #[test]
    fn deposit_rejects_insufficient_erc20_balance() {
        let mut ctx = make_ctx(U256::from(500_000u64));
        let err = run_deposit(&depositCall { amount: U256::from(1_000_000u64) }.abi_encode(), ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("insufficient USDC balance"), "{err}");
    }

    #[test]
    fn withdraw_returns_usdc_to_wallet() {
        let amount = U256::from(1_000_000u64);
        let mut ctx = make_ctx(amount);
        run_deposit(&depositCall { amount }.abi_encode(), ALICE, &mut ctx).unwrap();
        run_withdraw(&withdrawCall { amount }.abi_encode(), ALICE, &mut ctx).unwrap();
        let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
        let (usdc, _) = decode_get_account(&ret);
        assert_eq!(usdc, U256::ZERO);
    }

    #[test]
    fn withdraw_rejects_overdraft() {
        let mut ctx = make_ctx(U256::ZERO);
        let err = run_withdraw(&withdrawCall { amount: U256::from(1_000_000u64) }.abi_encode(), ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("insufficient internal balance"), "{err}");
    }

    #[test]
    fn transfer_to_and_from_perp_wallet() {
        let deposit_amt = U256::from(2_000_000u64);
        let transfer_amt: u64 = 1_000_000;
        let mut ctx = make_ctx(deposit_amt);
        run_deposit(&depositCall { amount: deposit_amt }.abi_encode(), ALICE, &mut ctx).unwrap();
        run_transfer_to_perp(&transferToPerpCall { amount: transfer_amt }.abi_encode(), ALICE, &mut ctx).unwrap();

        let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
        let (usdc, perp) = decode_get_account(&ret);
        assert_eq!(usdc, U256::from(1_000_000u64));
        assert_eq!(perp, transfer_amt);

        run_transfer_from_perp(&transferFromPerpCall { amount: transfer_amt }.abi_encode(), ALICE, &mut ctx).unwrap();
        let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
        let (usdc2, perp2) = decode_get_account(&ret);
        assert_eq!(usdc2, deposit_amt);
        assert_eq!(perp2, 0);
    }

    #[test]
    fn get_account_returns_zero_for_new_user() {
        let mut ctx = make_ctx(U256::ZERO);
        let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
        let (usdc, perp) = decode_get_account(&ret);
        assert_eq!(usdc, U256::ZERO);
        assert_eq!(perp, 0);
    }
}
