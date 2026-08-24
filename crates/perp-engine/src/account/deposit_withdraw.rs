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
    types::{AccountUpdateReason, MAX_PERP_WALLET_BALANCE},
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
    // `Deposit`: value crossed the venue boundary — ERC-20 USDC pulled into custody. This is the
    // DEPOSIT half of the DEPOSIT/WITHDRAW pair (see `AccountUpdateReason`); the spot ↔ perp-wallet
    // move inside the venue is `AssetTransfer`, not this.
    storage::save_account(context, caller, account, AccountUpdateReason::Deposit)?;

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
    // `Withdraw`: value leaves the venue (USDC returned to the caller's ERC-20 balance).
    storage::save_account(context, caller, account, AccountUpdateReason::Withdraw)?;
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
    // `AssetTransfer`, NOT `MarginTransfer`: this moves the same asset between two WALLETS (the spot
    // USDC ledger and the perp wallet) and touches no position. `MarginTransfer` is Binance's
    // isolated-position leg, and `add`/`removePositionMargin` is that operation exactly — see the
    // `AccountUpdateReason` docs for the full argument.
    storage::save_account(context, caller, account, AccountUpdateReason::AssetTransfer)?;

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
    // Derived-ooIM gate. Cash leaving the perp wallet entirely: `Σ ooIM` is untouched (neither
    // the book nor any position moves), so the requirement is exactly `amount` and it must come
    // out of AVAILABLE. This is THE money-out gate — the one place where getting the basis wrong
    // lets a user strip the collateral out from under their own resting orders — so it reads
    // `perp_wallet_balance − Σ ooIM`, never the raw wallet.
    let available = crate::margin_view::derived_available_balance(context, caller)?;
    if !crate::margin_view::derived_can_afford(available, amount as i128) {
        return Err(perp_err(
            "transferFromPerp: insufficient perp wallet balance",
        ));
    }
    account.debit_perp(amount)?;
    let spot: U256 = account.usdc_balance.clone().into();
    account.usdc_balance = (spot + U256::from(amount)).into();
    // `AssetTransfer` — the other direction of the same wallet ↔ wallet move; see `transferToPerp`.
    storage::save_account(context, caller, account, AccountUpdateReason::AssetTransfer)?;

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

/// `getAccount(address user)` — spot USDC plus the whole account-level margin roll-up, driven by
/// the PER-USER MARKET INDEX. See the ABI doc comment in [`crate::interface`] for the
/// field-by-field contract, the two Binance fields we deliberately omit, and the §7 porting
/// pitfalls this shape avoids.
///
/// The market set is the index (`umkt`), not a caller argument, so every total is COMPLETE — this
/// is the difference from `getAccountMargin`, and it is the ONLY difference: both call the same
/// [`crate::margin_view::account_margin_scalars`] walkers, so the arithmetic cannot drift between
/// them.
///
/// This function contains NO arithmetic: it decodes, calls
/// [`crate::margin_view::index_account_view`], and ABI-encodes. The `AccountBalanceChanged` event
/// calls the same producer and encodes the same fields into a log, which is why the two surfaces
/// cannot report different numbers for the same state.
///
/// Pure read. Every loader below is a `_ref` (cache-fill, never dirty-mark) reader, so this call
/// enters no key into the block delta and cannot move the block commitment.
pub fn run_get_account<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getAccountCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAccount: invalid calldata"))?;

    let view = crate::margin_view::index_account_view(context, args.user, "getAccount")?;
    let s = view.scalars;

    Ok(Bytes::from(getAccountCall::abi_encode_returns(
        &getAccountReturn {
            usdcBalance: view.usdc_balance,
            totalWalletBalance: s.total_wallet_balance,
            totalCrossWalletBalance: s.total_cross_wallet_balance,
            totalMarginBalance: s.total_margin_balance,
            totalUnrealizedProfit: s.total_unrealized_profit,
            totalInitialMargin: s.total_initial_margin,
            totalPositionInitialMargin: s.total_position_initial_margin,
            totalOpenOrderInitialMargin: s.total_open_order_initial_margin,
            totalMaintMargin: s.total_maint_margin,
            availableBalance: s.available_balance,
            // Structurally zero: isolated-only, so there are no cross positions to sum.
            // Present for response-shape compatibility only — see the note on `getAccount`.
            crossUnPnl: 0,
            // `max(0, availableBalance)`. Floors at zero because "withdraw a negative amount" is
            // meaningless; the un-clamped value stays visible in `availableBalance` above, so the
            // clamp costs no information. NOTE this is the perp→spot limit, not the protocol
            // withdrawal limit (`withdraw` gates on `usdcBalance`).
            maxWithdrawAmount: s.available_balance.max(0) as u64,
            // Echoed so the totals are self-checkable against `getMarginInfo` per id.
            marketIds: view.market_ids.to_vec(),
        },
    )))
}

#[cfg(test)]
#[path = "deposit_withdraw_tests.rs"]
mod tests;
