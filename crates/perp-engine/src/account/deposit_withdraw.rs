//! Deposit, withdraw, and inter-wallet transfer logic for the PerpDEX.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use crate::host::PerpHost;
use primitives::{keccak256, Address, Bytes, Log, U256};

use crate::{
        errors::{perp_err, perp_invariant_err},
    interface::IPerpDex::{
        self, depositCall, getAccountCall, getAccountReturn, transferFromPerpCall,
        transferFromPerpSignedCall, transferToPerpCall, transferToPerpSignedCall, withdrawCall,
        TransferFromPerp, TransferToPerp,
    },
    storage::{self, load_erc20_balance, save_erc20_balance},
    trading::{check_api_key_expiry, check_recv_window, gc_seen_buckets_best_effort, verify_ed25519},
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
    transfer_to_perp_core(context, caller, args.amount)
}

/// Shared body of `transferToPerp` / `transferToPerpSigned`. One implementation so the direct and
/// signed paths cannot drift on the balance check, the event, or the update reason.
fn transfer_to_perp_core<H: PerpHost>(
    context: &mut H,
    account_addr: Address,
    amount: u64,
) -> Result<Bytes, PerpError> {
    if amount == 0 {
        return Err(perp_err("transferToPerp: amount must be > 0"));
    }

    let mut account = storage::load_account(context, account_addr)?;
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
    storage::save_account(
        context,
        account_addr,
        account,
        AccountUpdateReason::AssetTransfer,
    )?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: TransferToPerp {
            user: account_addr,
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
    transfer_from_perp_core(context, caller, args.amount)
}

/// Shared body of `transferFromPerp` / `transferFromPerpSigned`. See [`transfer_to_perp_core`] for
/// why both directions route through one implementation.
fn transfer_from_perp_core<H: PerpHost>(
    context: &mut H,
    account_addr: Address,
    amount: u64,
) -> Result<Bytes, PerpError> {
    if amount == 0 {
        return Err(perp_err("transferFromPerp: amount must be > 0"));
    }

    let mut account = storage::load_account(context, account_addr)?;
    // Derived-ooIM gate. Cash leaving the perp wallet entirely: `Σ ooIM` is untouched (neither
    // the book nor any position moves), so the requirement is exactly `amount` and it must come
    // out of AVAILABLE. This is THE money-out gate — the one place where getting the basis wrong
    // lets a user strip the collateral out from under their own resting orders — so it reads
    // `perp_wallet_balance − Σ ooIM`, never the raw wallet.
    let available = crate::margin_view::derived_available_balance(context, account_addr)?;
    if !crate::margin_view::derived_can_afford(available, amount as i128) {
        return Err(perp_err(
            "transferFromPerp: insufficient perp wallet balance",
        ));
    }
    account.debit_perp(amount)?;
    let spot: U256 = account.usdc_balance.clone().into();
    account.usdc_balance = (spot + U256::from(amount)).into();
    // `AssetTransfer` — the other direction of the same wallet ↔ wallet move; see `transferToPerp`.
    storage::save_account(
        context,
        account_addr,
        account,
        AccountUpdateReason::AssetTransfer,
    )?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: TransferFromPerp {
            user: account_addr,
            amount,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `transferToPerpSigned(...)` / `transferFromPerpSigned(...)` — the ed25519-authenticated
/// versions of the two inter-wallet transfers, so an API key can fund and defund its own perp
/// wallet without the owner reaching for the master key.
///
/// Both follow the shape every signed entrypoint now shares: decode → load key → recv window →
/// key expiry → verify → seen-check → **burn unconditionally** → measure the burn → core, with the
/// core's reject tagged by that measurement so the commit-only #23 guard exempts the burn and
/// nothing else. See `trading::run_place_order_signed`'s burn site for the full argument.
///
/// # The two directions MUST NOT share a domain prefix
///
/// `"perpdex_v1_xfer_to"` and `"perpdex_v1_xfer_from"` differ, and that is load-bearing rather than
/// cosmetic: the rest of the two messages is byte-identical in layout, so a shared prefix would
/// make a signature authorising "move 100 IN" indistinguishable from one authorising "move 100
/// OUT", and a relayer could submit either against whichever selector it preferred. Neither string
/// is a proper prefix of the other, and their lengths differ, so no message of one kind can be
/// reinterpreted as the other.
#[allow(clippy::too_many_arguments)]
fn transfer_signed_message<const N: usize>(
    prefix: &[u8],
    chain_id: u64,
    account: Address,
    amount: u64,
    timestamp: u64,
    recv_window: u64,
    key_id: u8,
) -> [u8; N] {
    use crate::trading::{put, write_signed_header};
    let mut msg = [0u8; N];
    let mut c = write_signed_header(&mut msg, prefix, chain_id);
    c = put(&mut msg, c, account.as_slice());
    c = put(&mut msg, c, &amount.to_be_bytes());
    c = put(&mut msg, c, &timestamp.to_be_bytes());
    c = put(&mut msg, c, &recv_window.to_be_bytes());
    c = put(&mut msg, c, &[key_id]);
    debug_assert_eq!(c, N, "message layout and length must agree");
    msg
}

/// `transferToPerpSigned(address account, uint64 amount, uint64 timestamp, uint64 recvWindow,
/// uint8 keyId, bytes signature)` — see [`transfer_signed_message`].
pub fn run_transfer_to_perp_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = transferToPerpSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("transferToPerpSigned: invalid calldata"))?;

    let api_key = storage::load_api_key(context, args.account, args.keyId)?
        .ok_or_else(|| perp_err("transferToPerpSigned: no api key registered for account"))?;

    check_recv_window(context, args.timestamp, args.recvWindow)
        .map_err(|e| perp_err(&format!("transferToPerpSigned: {e}")))?;

    check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("transferToPerpSigned: {e}")))?;

    // "perpdex_v1_xfer_to"(18) || account(20) || amount(8) || timestamp(8) || recvWindow(8)
    //   || keyId(1) = 71 bytes  (chainId sits right after the prefix)
    let msg = transfer_signed_message::<71>(
        b"perpdex_v1_xfer_to",
        context.chain_id(),
        args.account,
        args.amount,
        args.timestamp,
        args.recvWindow,
        args.keyId,
    );

    verify_ed25519(&api_key.pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(&format!("transferToPerpSigned: {e}")))?;

    let sig_hash: [u8; 32] = keccak256(args.signature.as_ref()).0;
    if storage::is_signature_seen(context, &sig_hash)? {
        return Err(perp_err(
            "transferToPerpSigned: duplicate signature (already submitted)",
        ));
    }

    // ── last pre-write fault has passed; the first write happens here ──
    let block_ts: u64 = context.timestamp();
    let writes_before_burn = context.perp_write_count();
    storage::mark_signature_seen(context, &sig_hash, args.timestamp)?;
    gc_seen_buckets_best_effort(context, block_ts)?;
    let burn_writes = context.perp_write_count().saturating_sub(writes_before_burn);

    transfer_to_perp_core(context, args.account, args.amount)
        .map_err(|e| e.after_retained_writes(burn_writes))
}

/// `transferFromPerpSigned(address account, uint64 amount, uint64 timestamp, uint64 recvWindow,
/// uint8 keyId, bytes signature)` — the money-OUT direction; see [`transfer_signed_message`].
pub fn run_transfer_from_perp_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = transferFromPerpSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("transferFromPerpSigned: invalid calldata"))?;

    let api_key = storage::load_api_key(context, args.account, args.keyId)?
        .ok_or_else(|| perp_err("transferFromPerpSigned: no api key registered for account"))?;

    check_recv_window(context, args.timestamp, args.recvWindow)
        .map_err(|e| perp_err(&format!("transferFromPerpSigned: {e}")))?;

    check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("transferFromPerpSigned: {e}")))?;

    // "perpdex_v1_xfer_from"(20) || account(20) || amount(8) || timestamp(8) || recvWindow(8)
    //   || keyId(1) = 73 bytes  (chainId sits right after the prefix)
    let msg = transfer_signed_message::<73>(
        b"perpdex_v1_xfer_from",
        context.chain_id(),
        args.account,
        args.amount,
        args.timestamp,
        args.recvWindow,
        args.keyId,
    );

    verify_ed25519(&api_key.pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(&format!("transferFromPerpSigned: {e}")))?;

    let sig_hash: [u8; 32] = keccak256(args.signature.as_ref()).0;
    if storage::is_signature_seen(context, &sig_hash)? {
        return Err(perp_err(
            "transferFromPerpSigned: duplicate signature (already submitted)",
        ));
    }

    // ── last pre-write fault has passed; the first write happens here ──
    let block_ts: u64 = context.timestamp();
    let writes_before_burn = context.perp_write_count();
    storage::mark_signature_seen(context, &sig_hash, args.timestamp)?;
    gc_seen_buckets_best_effort(context, block_ts)?;
    let burn_writes = context.perp_write_count().saturating_sub(writes_before_burn);

    transfer_from_perp_core(context, args.account, args.amount)
        .map_err(|e| e.after_retained_writes(burn_writes))
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
/// # ONE CALL, ONE STATE
///
/// The return carries `positions[]` — the full per-market `AccountPosition` for every market in the
/// index — because the walk behind the totals already computes exactly that and used to discard it.
/// The reason to publish it is not brevity but CONSISTENCY: assembling this from
/// `getAccount` + N × `getPositionRisk` is `N + 1` `eth_call`s that can straddle blocks, so
/// `totalWalletBalance == totalCrossWalletBalance + Σ isolatedWallet` can fail for a healthy
/// account and the caller cannot distinguish that race from a bug in this precompile. Read together,
/// the identity holds by construction.
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
            totalCrossUnPnl: 0,
            // `max(0, availableBalance)`. Floors at zero because "withdraw a negative amount" is
            // meaningless; the un-clamped value stays visible in `availableBalance` above, so the
            // clamp costs no information. NOTE this is the perp→spot limit, not the protocol
            // withdrawal limit (`withdraw` gates on `usdcBalance`).
            maxWithdrawAmount: s.available_balance.max(0) as u64,
            // The per-market detail behind every total above, one row per market in the index, from
            // the SAME walk that produced them — so the totals are self-checkable against the rows
            // with no second call, and the balance identities hold within this one response. See
            // `margin_view::AccountPositionRow`; the rows subsume the `marketIds` array this
            // replaced (each carries its own `marketId`).
            positions: view.positions.iter().map(|r| r.to_abi()).collect(),
        },
    )))
}

#[cfg(test)]
#[path = "deposit_withdraw_tests.rs"]
mod tests;
