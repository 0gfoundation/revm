//! The insurance fund: admin deposit / withdraw and the balance read.

use super::*;

// ── Insurance Fund ────────────────────────────────────────────────────────────

/// `depositInsuranceFund(uint64 amount)` — admin only.
/// Debits `amount` from the admin's perp wallet and credits it to the insurance fund.
pub fn run_deposit_insurance_fund<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = depositInsuranceFundCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("depositInsuranceFund: invalid calldata"))?;

    require_admin(caller, context)?;

    if args.amount == 0 {
        return Err(perp_err("depositInsuranceFund: amount must be > 0"));
    }

    let mut account = storage::load_account(context, caller)?;
    // Derived money-out gate — a pure cash-out, see `addPositionMargin` / `transferFromPerp`.
    let available = crate::margin::view::derived_withdrawable_balance(context, caller)?;
    if !crate::margin::view::derived_can_afford(available, args.amount as i128) {
        return Err(perp_err(
            "depositInsuranceFund: insufficient perp wallet balance",
        ));
    }
    account.debit_perp(args.amount)?;
    // `InsuranceClear` — the admin wallet's counterparty is the insurance fund, same as the
    // liquidation clearance fee. Only the direction differs.
    storage::save_account(context, caller, account, AccountUpdateReason::InsuranceClear)?;

    let delta = checked_u64_to_i64(args.amount, "depositInsuranceFund: delta")?;
    let old_balance = storage::load_insurance_fund(context)?;
    let new_balance = old_balance
        .checked_add(args.amount)
        .ok_or_else(|| perp_err("depositInsuranceFund: balance overflow"))?;
    storage::save_insurance_fund(context, new_balance)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::InsuranceFundChanged {
            delta,
            newBalance: new_balance,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `withdrawInsuranceFund(uint64 amount)` — admin only.
/// Withdraws `amount` from the insurance fund back to the admin's perp wallet.
pub fn run_withdraw_insurance_fund<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = withdrawInsuranceFundCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("withdrawInsuranceFund: invalid calldata"))?;

    require_admin(caller, context)?;

    if args.amount == 0 {
        return Err(perp_err("withdrawInsuranceFund: amount must be > 0"));
    }

    let delta = checked_u64_to_i64(args.amount, "withdrawInsuranceFund: delta")?;
    let balance = storage::load_insurance_fund(context)?;
    if args.amount > balance {
        return Err(perp_err(
            "withdrawInsuranceFund: amount exceeds fund balance",
        ));
    }
    let new_balance = balance - args.amount;
    storage::save_insurance_fund(context, new_balance)?;

    let mut account = storage::load_account(context, caller)?;
    account.credit_perp(args.amount)?;
    // `InsuranceClear` — see `depositInsuranceFund`.
    storage::save_account(context, caller, account, AccountUpdateReason::InsuranceClear)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::InsuranceFundChanged {
            delta: -delta,
            newBalance: new_balance,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `getInsuranceFund()` — returns the current insurance fund balance.
pub fn run_get_insurance_fund<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    getInsuranceFundCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getInsuranceFund: invalid calldata"))?;
    let balance = storage::load_insurance_fund(context)?;
    Ok(Bytes::from(getInsuranceFundCall::abi_encode_returns(
        &balance,
    )))
}
