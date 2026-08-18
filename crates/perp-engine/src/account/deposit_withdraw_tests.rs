use super::*;
use alloy_sol_types::{SolCall, SolEvent};
use context::ContextTr;
use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId};

use crate::{
    account::{run_get_user_fee_rates, run_set_user_fee_rates},
    interface::IPerpDex::{
        depositCall, getAccountCall, getUserFeeRatesCall, setUserFeeRatesCall,
        transferFromPerpCall, transferToPerpCall, withdrawCall, AccountBalanceChanged,
    },
    storage,
    storage::keys::erc20_balance_slot,
    USDC_ADDRESS,
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

/// `(usdcBalance, availableBalance)` — the two fields these deposit/withdraw/transfer tests care
/// about, decoded through the real ABI decoder rather than by slicing words. It used to slice
/// `bytes[32..64]` for the (then-second, then-`uint64`) available balance; `getAccount` now returns
/// ten scalars plus `marketIds`, so word 1 is `totalWalletBalance` and hand-slicing would silently
/// read the wrong field. The full roll-up is exercised in `margin_view_tests`.
fn decode_get_account(bytes: &Bytes) -> (U256, i64) {
    let ret = getAccountCall::abi_decode_returns(bytes).expect("getAccount returns must decode");
    (ret.usdcBalance, ret.availableBalance)
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
    let (usdc, _available) = decode_get_account(&ret);
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

/// The fee-rate ceiling is 1_000 bps (10%), not the old 100%-of-notional `FEE_BPS_DENOMINATOR`.
/// Defence in depth only: the binding rule is K9 at fill time (the fee now leaves the position
/// margin, so `f > 1/(2·L_max)` simply makes a max-leverage open fail maintenance).
#[test]
fn set_user_fee_rates_accepts_1000_bps_and_rejects_1001() {
    let mut ctx = make_ctx(U256::ZERO);
    storage::save_admin(&mut ctx, ADMIN).unwrap();

    run_set_user_fee_rates(
        &setUserFeeRatesCall {
            user: ALICE,
            makerFeeBps: 1_000,
            takerFeeBps: 1_000,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    let ret = run_get_user_fee_rates(&getUserFeeRatesCall { user: ALICE }.abi_encode(), &mut ctx)
        .unwrap();
    assert_eq!(decode_user_fee_rates(&ret), (1_000, 1_000));

    for (maker, taker) in [(1_001u64, 0u64), (0, 1_001)] {
        let err = run_set_user_fee_rates(
            &setUserFeeRatesCall {
                user: ALICE,
                makerFeeBps: maker,
                takerFeeBps: taker,
            }
            .abi_encode(),
            ADMIN,
            &mut ctx,
        )
        .unwrap_err();
        assert!(err.to_string().contains("fee bps exceeds 1000"), "{err}");
    }
    // The rejected calls left the accepted rates in place.
    let ret = run_get_user_fee_rates(&getUserFeeRatesCall { user: ALICE }.abi_encode(), &mut ctx)
        .unwrap();
    assert_eq!(decode_user_fee_rates(&ret), (1_000, 1_000));
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
    let (usdc, _) = decode_get_account(&ret);
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
    let (usdc, available) = decode_get_account(&ret);
    assert_eq!(usdc, U256::from(1_000_000u64));
    assert_eq!(available, transfer_amt as i64);

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
    let (usdc2, available2) = decode_get_account(&ret);
    assert_eq!(usdc2, deposit_amt);
    assert_eq!(available2, 0);
}

#[test]
fn get_account_returns_zero_for_new_user() {
    let mut ctx = make_ctx(U256::ZERO);
    let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
    let (usdc, available) = decode_get_account(&ret);
    assert_eq!(usdc, U256::ZERO);
    assert_eq!(available, 0);
}

/// A NEGATIVE cross wallet shows through `getAccount` as a negative number.
///
/// This test used to be `get_account_clamps_negative_perp_wallet_to_zero` and asserted `0`. That
/// clamp was the blind spot the signed roll-up removed: floored at 0, this call could not tell
/// "exactly covered" from "under-covered by a dollar", which is exactly the verdict
/// `misc/binance-v3-account-balance-field-reference.md` §1 reaches about Binance's own clamped
/// `availableBalance` (reported `0.00000000` against a true `-0.00085981`). Every balance-like
/// field is now `int64` and reports the sign.
///
/// The EVENT reports it too, and this test pins that: `AccountBalanceChanged` used to project the
/// wallet through a `uint64` floored at 0, so a deficit never reached the log stream at all.
///
/// Doubles as the **empty-market-index** case: ALICE holds no position and no order, so `umkt` is
/// empty and every Σ folds over nothing. That must produce all-zero totals and NOT revert — on both
/// surfaces, including the one now reached from a WRITE path.
#[test]
fn get_account_on_a_bare_account_reports_a_negative_cross_wallet_unclamped() {
    let mut ctx = make_ctx(U256::ZERO);
    let mut account = storage::load_account(&mut ctx, ALICE).unwrap();
    account.perp_wallet_balance = -1_000_000;
    // This write EMITS, and the emission itself folds the (empty) index — so an empty index that
    // reverted would take the write down with it.
    storage::save_account(&mut ctx, ALICE, account).unwrap();

    let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
    let a = getAccountCall::abi_decode_returns(&ret).unwrap();
    assert_eq!(
        a.availableBalance, -1_000_000,
        "the sign is the whole point"
    );
    assert_eq!(a.totalCrossWalletBalance, -1_000_000);
    // No markets, so no silos and no unrealized PnL: gross == cross, and equity == gross.
    assert_eq!(a.totalWalletBalance, -1_000_000);
    assert_eq!(a.totalMarginBalance, -1_000_000);
    assert!(a.marketIds.is_empty());

    // ── The same numbers on the EVENT, unclamped, from the write above ───────────────────────
    let events = JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
        .map(|log| {
            AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    let e = &events[0];
    assert_eq!(
        (
            e.totalCrossWalletBalance,
            e.totalWalletBalance,
            e.totalMarginBalance,
            e.availableBalance
        ),
        (-1_000_000, -1_000_000, -1_000_000, -1_000_000),
        "the deficit reaches the LOG STREAM now; the retired uint64 field reported 0 here"
    );
    // Empty index ⇒ every fold is over nothing.
    assert_eq!(
        (
            e.totalUnrealizedProfit,
            e.totalInitialMargin,
            e.totalPositionInitialMargin,
            e.totalOpenOrderInitialMargin,
            e.totalMaintMargin
        ),
        (0, 0, 0, 0, 0)
    );
    // ...and it matches `getAccount` field for field, which is the anti-divergence pin.
    assert_eq!(
        (
            e.usdcBalance,
            e.totalWalletBalance,
            e.totalCrossWalletBalance,
            e.totalMarginBalance,
            e.totalUnrealizedProfit,
            e.totalInitialMargin,
            e.totalPositionInitialMargin,
            e.totalOpenOrderInitialMargin,
            e.totalMaintMargin,
            e.availableBalance
        ),
        (
            a.usdcBalance,
            a.totalWalletBalance,
            a.totalCrossWalletBalance,
            a.totalMarginBalance,
            a.totalUnrealizedProfit,
            a.totalInitialMargin,
            a.totalPositionInitialMargin,
            a.totalOpenOrderInitialMargin,
            a.totalMaintMargin,
            a.availableBalance
        )
    );
    // What a CLAMPED reading would have said — no published surface uses it any more.
    assert_eq!(
        storage::load_account_ref(&mut ctx, ALICE)
            .unwrap()
            .visible_perp_wallet_balance(),
        0
    );
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
    let (usdc_internal_after, _) = decode_get_account(
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
fn get_account_reports_available_wallet_net_of_allocations() {
    let mut ctx = make_ctx(U256::ZERO);
    storage::save_account(
        &mut ctx,
        ALICE,
        crate::types::UserAccount {
            perp_wallet_balance: 100,
            ..Default::default()
        },
    )
    .unwrap();
    storage::save_position(
        &mut ctx,
        ALICE,
        1,
        &crate::types::PerpPosition {
            margin: 40,
            ..Default::default()
        },
    )
    .unwrap();
    storage::mutate_account(&mut ctx, ALICE, |account| account.debit_perp(55))
        .unwrap()
        .unwrap();

    // getAccount reports the DERIVED available: `perp_wallet_balance - Σ ooIM`. Position margin
    // is already out of the wallet (it was debited at open), and this fixture has no resting
    // orders, so `Σ ooIM` is 0 and the answer is the wallet itself. The former
    // `total_perp_collateral` aggregate is no longer stored or returned — consumers derive it
    // from getAccount + getPosition off-chain.
    let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap();
    let (_, available) = decode_get_account(&ret);
    assert_eq!(available, 45);
}

#[test]
fn successful_call_emits_one_final_balance_after_image() {
    let amount = U256::from(1_000_000_u64);
    let mut ctx = make_ctx(amount);
    let output = crate::run_perp_dex_call(
        &depositCall { amount }.abi_encode(),
        1_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!output.reverted);
    // Gas is FLAT per selector — one number for the whole call regardless of how many events it
    // emits (per-event metering is rejected outright). The number went 50_000 → 70_000 when the
    // event grew into the account-level roll-up: the emission now folds the user's market index,
    // which is exactly the work `getAccount` is priced at 20_000 for, so the flat constant absorbs
    // one `getAccount`-equivalent. See the note on the `SELECTORS` account block.
    assert_eq!(output.gas_used, 70_000);

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
    // A deposit only moves SPOT USDC, so every perp-side total is still 0 — and the account has no
    // market index at all, which is the "empty index folds to all zeros, no revert" case.
    assert_eq!(events[0].totalCrossWalletBalance, 0);
    assert_eq!(events[0].totalWalletBalance, 0);
    assert_eq!(events[0].totalMarginBalance, 0);
    assert_eq!(events[0].totalUnrealizedProfit, 0);
    assert_eq!(events[0].totalInitialMargin, 0);
    assert_eq!(events[0].totalPositionInitialMargin, 0);
    assert_eq!(events[0].totalOpenOrderInitialMargin, 0);
    assert_eq!(events[0].totalMaintMargin, 0);
    assert_eq!(events[0].availableBalance, 0);
}

#[test]
fn reverted_call_emits_no_balance_after_image() {
    let mut ctx = make_ctx(U256::ZERO);
    let output = crate::run_perp_dex_call(
        &depositCall { amount: U256::ZERO }.abi_encode(),
        1_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(output.reverted);
    // A reverted call still pays the flat selector price (70_000 post-roll-up), and emits nothing.
    assert_eq!(output.gas_used, 70_000);
    assert!(JournalTr::take_logs(ctx.journal_mut())
        .iter()
        .all(|log| log.data.topics().first() != Some(&AccountBalanceChanged::SIGNATURE_HASH)));
}

#[test]
fn metadata_only_call_emits_no_balance_after_image() {
    let mut ctx = make_ctx(U256::ZERO);
    storage::save_admin(&mut ctx, ADMIN).unwrap();

    let output = crate::run_perp_dex_call(
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
