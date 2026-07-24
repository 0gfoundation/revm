use super::*;
use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId};

use crate::perp_dex::{
    account::{run_get_user_fee_rates, run_set_user_fee_rates},
    interface::IPerpDex::{
        depositCall, getAccountCall, getUserFeeRatesCall, setUserFeeRatesCall,
        transferFromPerpCall, transferToPerpCall, withdrawCall, AccountBalanceChanged,
    },
    storage,
    storage::keys::erc20_balance_slot,
    ACCOUNT_BALANCE_CHANGED_GAS, USDC_ADDRESS,
};

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

fn make_ctx(alice_usdc: U256) -> TestCtx {
    let mut db = InMemoryDB::default();
    db.insert_account_storage(USDC_ADDRESS, erc20_balance_slot(ALICE).into(), alice_usdc)
        .unwrap();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

fn decode_get_account(bytes: &Bytes) -> (U256, U256, u64) {
    let usdc = U256::from_be_slice(&bytes[..32]);
    let total = U256::from_be_slice(&bytes[32..64]);
    let available = U256::from_be_slice(&bytes[64..96]).to::<u64>();
    (usdc, total, available)
}

fn decode_user_fee_rates(bytes: &Bytes) -> (u64, u64) {
    let maker = U256::from_be_slice(&bytes[..32]).to::<u64>();
    let taker = U256::from_be_slice(&bytes[32..64]).to::<u64>();
    (maker, taker)
}

#[test]
fn deposit_moves_usdc_to_internal_account() {
    let amount = U256::from(1_000_000u64);
    let mut ctx = make_ctx(amount);
    run_deposit(&depositCall { amount }.abi_encode(), ALICE, &mut ctx).unwrap();
    let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
    let (usdc, _total, _available) = decode_get_account(&ret);
    assert_eq!(usdc, amount);
}

#[test]
fn get_user_fee_rates_defaults_to_zero() {
    let mut ctx = make_ctx(U256::ZERO);

    let ret = run_get_user_fee_rates(&getUserFeeRatesCall { user: ALICE }.abi_encode(), &mut ctx)
        .unwrap();

    assert_eq!(decode_user_fee_rates(&ret), (0, 0));
}

#[test]
fn admin_can_set_user_fee_rates() {
    let mut ctx = make_ctx(U256::ZERO);
    storage::save_admin(&mut ctx, ADMIN).unwrap();

    run_set_user_fee_rates(
        &setUserFeeRatesCall {
            user: ALICE,
            makerFeeBps: 2,
            takerFeeBps: 5,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    let ret = run_get_user_fee_rates(&getUserFeeRatesCall { user: ALICE }.abi_encode(), &mut ctx)
        .unwrap();

    assert_eq!(decode_user_fee_rates(&ret), (2, 5));
}

#[test]
fn deposit_rejects_zero_amount() {
    let mut ctx = make_ctx(U256::from(1_000_000u64));
    let err = run_deposit(
        &depositCall { amount: U256::ZERO }.abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap_err();
    assert!(err.to_string().contains("amount must be > 0"), "{err}");
}

#[test]
fn deposit_rejects_when_total_would_exceed_i64_max() {
    // Two deposits of 2/3 * i64::MAX each: individually valid, cumulatively overflows.
    let two_thirds = U256::from(i64::MAX as u64 / 3 * 2);
    let mut ctx = make_ctx(U256::MAX);
    run_deposit(
        &depositCall { amount: two_thirds }.abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap();
    // Capture state after the first (valid) deposit.
    let caller_usdc_before = storage::load_erc20_balance(&mut ctx, USDC_ADDRESS, ALICE).unwrap();
    let dex_usdc_before =
        storage::load_erc20_balance(&mut ctx, USDC_ADDRESS, PERP_DEX_ADDRESS).unwrap();
    let internal_before = storage::load_account(&mut ctx, ALICE)
        .unwrap()
        .usdc_balance
        .clone();

    let err = run_deposit(
        &depositCall { amount: two_thirds }.abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap_err();
    assert!(err.to_string().contains("exceed i64::MAX"), "{err}");

    // commit-only #23 (validate-then-apply): the rejected deposit must leave ZERO writes. This
    // unit test never invokes checkpoint_revert, so any pre-error write is VISIBLE here — under
    // the old write-then-error ordering the caller/DEX USDC legs had already moved before the MAX
    // reject (a USDC-loss under commit-only). This asserts the reject touches nothing.
    assert_eq!(
        storage::load_erc20_balance(&mut ctx, USDC_ADDRESS, ALICE).unwrap(),
        caller_usdc_before,
        "rejected deposit moved caller USDC"
    );
    assert_eq!(
        storage::load_erc20_balance(&mut ctx, USDC_ADDRESS, PERP_DEX_ADDRESS).unwrap(),
        dex_usdc_before,
        "rejected deposit moved DEX custody"
    );
    assert_eq!(
        storage::load_account(&mut ctx, ALICE).unwrap().usdc_balance,
        internal_before,
        "rejected deposit changed internal balance"
    );
}

#[test]
fn deposit_rejects_insufficient_erc20_balance() {
    let mut ctx = make_ctx(U256::from(500_000u64));
    let err = run_deposit(
        &depositCall {
            amount: U256::from(1_000_000u64),
        }
        .abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("insufficient USDC balance"),
        "{err}"
    );
}

#[test]
fn withdraw_returns_usdc_to_wallet() {
    let amount = U256::from(1_000_000u64);
    let mut ctx = make_ctx(amount);
    run_deposit(&depositCall { amount }.abi_encode(), ALICE, &mut ctx).unwrap();
    run_withdraw(&withdrawCall { amount }.abi_encode(), ALICE, &mut ctx).unwrap();
    let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
    let (usdc, _, _) = decode_get_account(&ret);
    assert_eq!(usdc, U256::ZERO);
}

#[test]
fn withdraw_rejects_overdraft() {
    let mut ctx = make_ctx(U256::ZERO);
    let err = run_withdraw(
        &withdrawCall {
            amount: U256::from(1_000_000u64),
        }
        .abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("insufficient internal balance"),
        "{err}"
    );
}

#[test]
fn transfer_to_and_from_perp_wallet() {
    let deposit_amt = U256::from(2_000_000u64);
    let transfer_amt: u64 = 1_000_000;
    let mut ctx = make_ctx(deposit_amt);
    run_deposit(
        &depositCall {
            amount: deposit_amt,
        }
        .abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap();
    run_transfer_to_perp(
        &transferToPerpCall {
            amount: transfer_amt,
        }
        .abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap();

    let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
    let (usdc, total, available) = decode_get_account(&ret);
    assert_eq!(usdc, U256::from(1_000_000u64));
    assert_eq!(total, U256::from(transfer_amt));
    assert_eq!(available, transfer_amt);

    run_transfer_from_perp(
        &transferFromPerpCall {
            amount: transfer_amt,
        }
        .abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap();
    let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
    let (usdc2, total2, available2) = decode_get_account(&ret);
    assert_eq!(usdc2, deposit_amt);
    assert_eq!(total2, U256::ZERO);
    assert_eq!(available2, 0);
}

#[test]
fn get_account_returns_zero_for_new_user() {
    let mut ctx = make_ctx(U256::ZERO);
    let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
    let (usdc, total, available) = decode_get_account(&ret);
    assert_eq!(usdc, U256::ZERO);
    assert_eq!(total, U256::ZERO);
    assert_eq!(available, 0);
}

#[test]
fn get_account_clamps_negative_perp_wallet_to_zero() {
    let mut ctx = make_ctx(U256::ZERO);
    let mut account = storage::load_account(&mut ctx, ALICE).unwrap();
    account.perp_wallet_balance = -1_000_000;
    storage::save_account(&mut ctx, ALICE, account).unwrap();

    let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
    let (_usdc, total, available) = decode_get_account(&ret);
    assert_eq!(total, U256::ZERO);
    assert_eq!(available, 0);
}

/// A deposit writes the USDC ERC-20 balance (on-trie, EVM journal) AND the internal perp
/// account (off-trie, perp section). Reverting the surrounding checkpoint must roll BOTH
/// back in lock-step — the one place the EVM journal and the perp undo log must agree.
#[test]
fn deposit_commit_only_revert_semantics() {
    // commit-only (#23): the off-trie perp write SURVIVES an enclosing frame revert while the
    // on-trie ERC-20 legs (EVM journal) roll back. Exactly this divergence is why the
    // EOA-direct depth guard forbids enclosing frames on-chain — this unit test constructs the
    // forbidden situation directly (unit calls run at depth 0, below the guard) to document it.
    let amount = U256::from(1_000_000u64);
    let mut ctx = make_ctx(amount); // ALICE holds `amount` USDC (ERC-20, on-trie)

    let cp = ctx.journal_mut().checkpoint();
    run_deposit(&depositCall { amount }.abi_encode(), ALICE, &mut ctx).unwrap();
    ctx.journal_mut().checkpoint_revert(cp);

    // Off-trie internal balance persists (commit-only)...
    let (usdc_internal_after, _, _) = decode_get_account(
        &run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap(),
    );
    assert_eq!(usdc_internal_after, amount, "off-trie write is commit-only");
    // ...while the on-trie ERC-20 balance reverts with the EVM journal.
    assert_eq!(
        storage::load_erc20_balance(&mut ctx, USDC_ADDRESS, ALICE).unwrap(),
        amount,
        "on-trie ERC-20 balance reverts with the EVM journal"
    );
}

#[test]
fn account_total_tracks_available_and_allocated_collateral() {
    let mut ctx = make_ctx(U256::ZERO);
    storage::save_account(
        &mut ctx,
        ALICE,
        crate::perp_dex::types::UserAccount {
            perp_wallet_balance: 100,
            ..Default::default()
        },
    )
    .unwrap();
    storage::save_position(
        &mut ctx,
        ALICE,
        1,
        &crate::perp_dex::types::PerpPosition {
            margin: 40,
            margin_reserved: 10,
            fee_reserved: 5,
            ..Default::default()
        },
    )
    .unwrap();
    storage::mutate_account(&mut ctx, ALICE, |account| account.debit_perp(55))
        .unwrap()
        .unwrap();

    let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
    let (_, total, available) = decode_get_account(&ret);
    assert_eq!(total, U256::from(100));
    assert_eq!(available, 45);
}

#[test]
fn successful_call_emits_one_final_balance_after_image() {
    let amount = U256::from(1_000_000_u64);
    let mut ctx = make_ctx(amount);
    let output = crate::perp_dex::run_perp_dex_call(
        &depositCall { amount }.abi_encode(),
        1_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!output.reverted);
    assert_eq!(output.gas_used, 50_000 + ACCOUNT_BALANCE_CHANGED_GAS);

    let events = JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
        .map(|log| {
            AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].user, ALICE);
    assert_eq!(events[0].usdcBalance, amount);
    assert_eq!(events[0].perpWalletBalance, U256::ZERO);
    assert_eq!(events[0].availablePerpBalance, 0);
}

#[test]
fn balance_after_image_gas_is_reserved_before_deposit_writes() {
    let amount = U256::from(1_000_000_u64);
    let mut ctx = make_ctx(amount);
    let writes_before = ctx.journal_mut().perp_write_count();

    let err = crate::perp_dex::run_perp_dex_call(
        &depositCall { amount }.abi_encode(),
        50_000 + ACCOUNT_BALANCE_CHANGED_GAS - 1,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap_err();

    assert!(matches!(err, crate::PrecompileError::OutOfGas));
    assert_eq!(ctx.journal_mut().perp_write_count(), writes_before);
    assert_eq!(
        storage::load_erc20_balance(&mut ctx, USDC_ADDRESS, ALICE).unwrap(),
        amount
    );
    assert_eq!(
        storage::load_account(&mut ctx, ALICE)
            .unwrap()
            .public_balance(),
        crate::perp_dex::types::PublicAccountBalance::default()
    );
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
}

#[test]
fn reverted_call_emits_no_balance_after_image() {
    let mut ctx = make_ctx(U256::ZERO);
    let output = crate::perp_dex::run_perp_dex_call(
        &depositCall { amount: U256::ZERO }.abi_encode(),
        1_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(output.reverted);
    assert_eq!(output.gas_used, 50_000);
    assert!(JournalTr::take_logs(ctx.journal_mut())
        .iter()
        .all(|log| log.data.topics().first() != Some(&AccountBalanceChanged::SIGNATURE_HASH)));
}

#[test]
fn metadata_only_call_emits_no_balance_after_image() {
    let mut ctx = make_ctx(U256::ZERO);
    storage::save_admin(&mut ctx, ADMIN).unwrap();

    let output = crate::perp_dex::run_perp_dex_call(
        &setUserFeeRatesCall {
            user: ALICE,
            makerFeeBps: 1,
            takerFeeBps: 2,
        }
        .abi_encode(),
        1_000_000,
        ADMIN,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!output.reverted);
    assert_eq!(output.gas_used, 30_000);
    assert!(JournalTr::take_logs(ctx.journal_mut())
        .iter()
        .all(|log| log.data.topics().first() != Some(&AccountBalanceChanged::SIGNATURE_HASH)));
}
