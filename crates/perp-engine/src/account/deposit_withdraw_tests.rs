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
/// twelve scalars plus `positions[]` (which replaced the `marketIds` array), so word 1 is
/// `totalWalletBalance` and hand-slicing would silently read the wrong field. The full roll-up is
/// exercised in `margin_view_tests`.
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

/// **The four wallet-moving selectors report four different reasons**, and the split between them is
/// the one decision on this field that needed an argument rather than a lookup.
///
/// Binance has two transfer-shaped values and they describe *which two pots*. `MARGIN_TRANSFER` is
/// their isolated-position leg (their *Modify Isolated Position Margin*), and we have that operation
/// exactly — `addPositionMargin` / `removePositionMargin` — so it is reserved for it, or the one
/// value with a precise analogue here would be the ambiguous one. `transferToPerp` /
/// `transferFromPerp` move the same asset between two WALLETS and touch no position, which is the
/// `ASSET_TRANSFER` shape; that in turn leaves `DEPOSIT` / `WITHDRAW` to mean what they say — value
/// crossing the venue boundary, in and out of ERC-20 custody.
///
/// Every row here is a 0-position group, which is why the reason is the ONLY thing that distinguishes
/// four of them: an indexer watching `usdcBalance` move by 1_000_000 cannot otherwise tell a deposit
/// from a transfer back out of the perp wallet.
#[test]
fn the_four_wallet_selectors_each_report_their_own_reason() {
    use crate::events::stream_test_support::account_update_reasons;
    use crate::types::AccountUpdateReason as R;

    let amount = U256::from(2_000_000u64);
    let mut ctx = make_ctx(amount);

    /// Drives one handler inside an explicit call boundary (these tests bypass the shell, which is
    /// what normally opens and drains the coalescing set) and returns that call's reasons.
    fn reasons_of<F>(ctx: &mut TestCtx, f: F) -> Vec<R>
    where
        F: FnOnce(&mut TestCtx),
    {
        let _ = JournalTr::take_logs(ctx.journal_mut());
        storage::begin_perp_call(ctx);
        f(ctx);
        storage::flush_account_snapshots(ctx).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());
        account_update_reasons(&logs)
            .into_iter()
            .map(|(u, r)| {
                assert_eq!(u, ALICE);
                r
            })
            .collect()
    }

    assert_eq!(
        reasons_of(&mut ctx, |ctx| {
            run_deposit(&depositCall { amount }.abi_encode(), ALICE, ctx).unwrap();
        }),
        vec![R::Deposit],
        "USDC pulled into custody is value ENTERING the venue"
    );
    assert_eq!(
        reasons_of(&mut ctx, |ctx| {
            run_transfer_to_perp(
                &transferToPerpCall { amount: 1_000_000 }.abi_encode(),
                ALICE,
                ctx,
            )
            .unwrap();
        }),
        vec![R::AssetTransfer],
        "spot ledger → perp wallet is a WALLET-to-WALLET move; MARGIN_TRANSFER is the isolated \
         position leg and belongs to add/removePositionMargin"
    );
    assert_eq!(
        reasons_of(&mut ctx, |ctx| {
            run_transfer_from_perp(
                &transferFromPerpCall { amount: 1_000_000 }.abi_encode(),
                ALICE,
                ctx,
            )
            .unwrap();
        }),
        vec![R::AssetTransfer],
        "…and the same going the other way"
    );
    assert_eq!(
        reasons_of(&mut ctx, |ctx| {
            run_withdraw(&withdrawCall { amount }.abi_encode(), ALICE, ctx).unwrap();
        }),
        vec![R::Withdraw],
        "USDC returned to the caller is value LEAVING the venue"
    );
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
    // This write MARKS, and the end-of-call drain folds the (empty) index — so an empty index that
    // reverted would take the call down with it. Driven directly here (no shell), so the call
    // boundary is explicit.
    storage::begin_perp_call(&mut ctx);
    storage::save_account(&mut ctx, ALICE, account, crate::types::AccountUpdateReason::Adjustment).unwrap();
    storage::flush_account_snapshots(&mut ctx).unwrap();

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
    assert!(a.positions.is_empty());

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
        (e.totalCrossWalletBalance, e.totalWalletBalance),
        (-1_000_000, -1_000_000),
        "the deficit reaches the LOG STREAM now; the retired uint64 field reported 0 here"
    );
    // Empty index ⇒ every fold is over nothing. Asserted on `getAccount`: these five totals left the
    // event when its payload narrowed to the three balances (they are REST fields on Binance, not
    // `ACCOUNT_UPDATE` fields), and `getAccount` is now their only surface. The empty-index-must-not-
    // revert property still covers the EVENT path, because the drain above ran the lean fold over the
    // same empty index and the call did not fail.
    assert_eq!(
        (
            a.totalUnrealizedProfit,
            a.totalInitialMargin,
            a.totalPositionInitialMargin,
            a.totalOpenOrderInitialMargin,
            a.totalMaintMargin
        ),
        (0, 0, 0, 0, 0)
    );
    // ...and the event matches `getAccount` on its whole payload, which is the anti-divergence pin.
    assert_eq!(
        (
            e.usdcBalance,
            e.totalWalletBalance,
            e.totalCrossWalletBalance,
        ),
        (
            a.usdcBalance,
            a.totalWalletBalance,
            a.totalCrossWalletBalance,
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
        crate::types::AccountUpdateReason::Adjustment,
    )
    .unwrap();
    // A REAL position: non-zero `amount` in a market that exists. It used to be written flat with
    // `margin: 40`, which is a state the engine cannot produce (every close zeroes `margin` alongside
    // `amount`) and which `storage::save_position` now `debug_assert`s against — and a flat position
    // holding margin would additionally make the two published `totalWalletBalance` surfaces disagree,
    // since `getAccount` sums `pos.margin` over the per-user market index while the stored
    // `total_position_margin` aggregate covers every market. The market has to exist because the
    // position puts ALICE into the per-user index, which `getAccount` walks.
    storage::save_market(
        &mut ctx,
        &crate::types::Market {
            market_id: 1,
            base_decimals: 0,
            price_decimals: 0,
            tick_size: 1,
            step_size: 1,
            min_quantity: 1,
            max_quantity: 1_000_000,
            max_price: 100_000_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 0,
            mark_price: 1,
            tiers: crate::types::MarginTiers::default(),
        },
    )
    .unwrap();
    storage::save_position(
        &mut ctx,
        ALICE,
        1,
        &crate::types::PerpPosition {
            amount: 1,
            margin: 40,
            leverage: 1,
            ..Default::default()
        },
        crate::types::AccountUpdateReason::Adjustment,
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
    // emits (per-event metering is rejected outright). 50_000 → 70_000 when the event grew into the
    // account-level roll-up (one `getAccount`-equivalent fold), then 70_000 → 60_000 when the payload
    // narrowed to the three balances: the emit fold is now one position load per market with no tier
    // walk, no uPnL and no ooIM, i.e. about half a `getAccount`. See the `SELECTORS` account block.
    assert_eq!(output.gas_used, 60_000);

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
    // A deposit only moves SPOT USDC, so both perp-side balances on the event are still 0 — and the
    // account has no market index at all, which is the "empty index folds to zero, no revert" case
    // for the lean fold behind the event.
    assert_eq!(events[0].totalCrossWalletBalance, 0);
    assert_eq!(events[0].totalWalletBalance, 0);
    // The seven margin totals are `getAccount`-only now; the same "all zeros over an empty index"
    // property is asserted there, on the wide fold.
    let a = getAccountCall::abi_decode_returns(
        &run_get_account(&getAccountCall { user: ALICE }.abi_encode(), &mut ctx).unwrap(),
    )
    .unwrap();
    assert_eq!(
        (
            a.totalMarginBalance,
            a.totalUnrealizedProfit,
            a.availableBalance
        ),
        (0, 0, 0)
    );
    assert_eq!(
        (
            a.totalInitialMargin,
            a.totalPositionInitialMargin,
            a.totalOpenOrderInitialMargin,
            a.totalMaintMargin
        ),
        (0, 0, 0, 0)
    );
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
    // A reverted call still pays the flat selector price (60_000 with the narrowed snapshot), and
    // emits nothing.
    assert_eq!(output.gas_used, 60_000);
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

// ── Signed inter-wallet transfers ────────────────────────────────────────────────
//
// `transferToPerpSigned` / `transferFromPerpSigned` let an API key fund and defund its own perp
// wallet. They follow the shape every signed entrypoint shares — verify, seen-check, burn
// unconditionally, measure the burn, tag the core's reject with that allowance — so these pin the
// two things specific to THIS pair: the shared core cannot drift from the direct path, and the two
// directions cannot be confused for one another.
mod signed_transfers {
    use super::*;
    use crate::interface::IPerpDex::{transferFromPerpSignedCall, transferToPerpSignedCall};
    use crate::types::ApiKey;
    use ed25519_dalek::{Signer, SigningKey};

    const SIGNED_TS: u64 = 1; // == block timestamp
    const SIGNED_RECV: u64 = 60;
    /// Anyone may relay a signed call; the authority is the signature, not the sender.
    const RELAYER: Address = address!("3333333333333333333333333333333333333333");

    /// The chain id `make_ctx` reports — every signed message is bound to it.
    const TEST_CHAIN_ID: u64 = 1;

    fn signed_msg(prefix: &[u8], amount: u64) -> Vec<u8> {
        let mut msg = prefix.to_vec();
        msg.extend_from_slice(&TEST_CHAIN_ID.to_be_bytes());
        msg.extend_from_slice(ALICE.as_slice());
        msg.extend_from_slice(&amount.to_be_bytes());
        msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
        msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
        msg.push(0); // keyId
        msg
    }

    /// `sign_prefix` is what gets SIGNED; the calldata is always built for `to`/`from` as named.
    /// Splitting them is what lets the cross-direction confusion test exist at all.
    fn to_input(sk: &SigningKey, amount: u64, sign_prefix: &[u8], tamper: bool) -> Vec<u8> {
        let mut sig = sk.sign(&signed_msg(sign_prefix, amount)).to_bytes().to_vec();
        if tamper {
            sig[0] ^= 0xff;
        }
        transferToPerpSignedCall {
            account: ALICE,
            amount,
            timestamp: SIGNED_TS,
            recvWindow: SIGNED_RECV,
            keyId: 0,
            signature: sig.into(),
        }
        .abi_encode()
    }

    fn from_input(sk: &SigningKey, amount: u64, sign_prefix: &[u8], tamper: bool) -> Vec<u8> {
        let mut sig = sk.sign(&signed_msg(sign_prefix, amount)).to_bytes().to_vec();
        if tamper {
            sig[0] ^= 0xff;
        }
        transferFromPerpSignedCall {
            account: ALICE,
            amount,
            timestamp: SIGNED_TS,
            recvWindow: SIGNED_RECV,
            keyId: 0,
            signature: sig.into(),
        }
        .abi_encode()
    }

    const TO: &[u8] = b"perpdex_v1_xfer_to";
    const FROM: &[u8] = b"perpdex_v1_xfer_from";

    fn call(ctx: &mut TestCtx, input: &[u8]) -> (bool, String) {
        let out = crate::run_perp_dex_call(input, 1_000_000, RELAYER, U256::ZERO, false, ctx)
            .expect("call must not hard-fail");
        (out.reverted, String::from_utf8_lossy(&out.bytes).to_string())
    }

    /// ALICE with `spot` USDC already inside the DEX and a registered ed25519 key.
    fn fixture(spot: u64) -> (TestCtx, SigningKey) {
        let amount = U256::from(spot);
        let mut ctx = make_ctx(amount);
        run_deposit(&depositCall { amount }.abi_encode(), ALICE, &mut ctx).unwrap();
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        storage::save_api_key(
            &mut ctx,
            ALICE,
            0,
            ApiKey {
                pubkey: sk.verifying_key().to_bytes(),
                expiry: 0,
            },
        )
        .unwrap();
        (ctx, sk)
    }

    fn balances(ctx: &mut TestCtx) -> (U256, i64) {
        let ret = run_get_account(&getAccountCall { user: ALICE }.abi_encode(), ctx).unwrap();
        decode_get_account(&ret)
    }

    #[test]
    fn a_relayer_can_move_funds_both_ways_for_the_key_owner() {
        let (mut ctx, sk) = fixture(2_000_000);

        let (reverted, reason) = call(&mut ctx, &to_input(&sk, 1_000_000, TO, false));
        assert!(!reverted, "transferToPerpSigned must succeed: {reason}");
        let (usdc, available) = balances(&mut ctx);
        assert_eq!(usdc, U256::from(1_000_000u64), "spot debited");
        assert_eq!(available, 1_000_000, "perp credited");

        let (reverted, reason) = call(&mut ctx, &from_input(&sk, 400_000, FROM, false));
        assert!(!reverted, "transferFromPerpSigned must succeed: {reason}");
        let (usdc, available) = balances(&mut ctx);
        assert_eq!(usdc, U256::from(1_400_000u64), "spot credited back");
        assert_eq!(available, 600_000, "perp debited");
    }

    /// ⚠️ The reason the two directions carry different domain prefixes. Everything after the
    /// prefix is byte-identical in layout, so a shared prefix would make "move 100 IN" and
    /// "move 100 OUT" the same signed bytes, and a relayer could pick the selector.
    #[test]
    fn a_signature_for_one_direction_cannot_drive_the_other() {
        let (mut ctx, sk) = fixture(2_000_000);
        call(&mut ctx, &to_input(&sk, 1_000_000, TO, false));
        let before = balances(&mut ctx);

        // A genuine "move 500_000 OUT" signature, replayed against the IN selector…
        let (reverted, reason) = call(&mut ctx, &to_input(&sk, 500_000, FROM, false));
        assert!(reverted, "cross-direction reuse must fail: {reason}");
        assert!(reason.contains("signature verification failed"), "got {reason}");

        // …and the mirror image.
        let (reverted, reason) = call(&mut ctx, &from_input(&sk, 500_000, TO, false));
        assert!(reverted, "cross-direction reuse must fail: {reason}");
        assert!(reason.contains("signature verification failed"), "got {reason}");

        assert_eq!(balances(&mut ctx), before, "no balance may have moved");
    }

    #[test]
    fn signatures_are_single_use_in_both_directions() {
        let (mut ctx, sk) = fixture(2_000_000);

        let input = to_input(&sk, 1_000_000, TO, false);
        assert!(!call(&mut ctx, &input).0);
        let (reverted, reason) = call(&mut ctx, &input);
        assert!(reverted && reason.contains("duplicate signature"), "got {reason}");
        assert_eq!(balances(&mut ctx).1, 1_000_000, "replay must not double-credit");

        let input = from_input(&sk, 100_000, FROM, false);
        assert!(!call(&mut ctx, &input).0);
        let (reverted, reason) = call(&mut ctx, &input);
        assert!(reverted && reason.contains("duplicate signature"), "got {reason}");
        assert_eq!(balances(&mut ctx).1, 900_000, "replay must not double-debit");
    }

    /// Spending by SUBMISSION, not by success — the property that makes a transient rejection
    /// un-exploitable. `transferFromPerp`'s rejection is exactly that kind: it is gated on derived
    /// `available`, which moves as orders rest and positions open.
    #[test]
    fn a_rejected_transfer_still_spends_its_signature() {
        let (mut ctx, sk) = fixture(1_000_000);
        // Nothing in the perp wallet yet, so the money-out gate refuses.
        let input = from_input(&sk, 500_000, FROM, false);
        let (reverted, reason) = call(&mut ctx, &input);
        assert!(reverted, "must be rejected on its merits");
        assert!(reason.contains("insufficient perp wallet balance"), "got {reason}");

        // Conditions change — the wallet is funded — and the same signature must STILL be refused.
        assert!(!call(&mut ctx, &to_input(&sk, 1_000_000, TO, false)).0);
        let (reverted, reason) = call(&mut ctx, &input);
        assert!(reverted && reason.contains("duplicate signature"), "got {reason}");
        assert_eq!(
            balances(&mut ctx).1,
            1_000_000,
            "a spent signature must not move money once it would succeed"
        );
    }

    /// The burn must stay after `verify_ed25519`, or anyone could write replay markers from
    /// arbitrary bytes.
    #[test]
    fn an_unverified_signature_writes_nothing() {
        let (mut ctx, sk) = fixture(1_000_000);
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

        for input in [
            to_input(&sk, 1, TO, true),
            from_input(&sk, 1, FROM, true),
        ] {
            let (reverted, reason) = call(&mut ctx, &input);
            assert!(reverted, "tampered signature must fail verification");
            assert!(reason.contains("signature verification failed"), "got {reason}");
        }
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "signature failure is pre-write — no marker may be burned"
        );
    }

    /// The signed path must reject for the same reasons and with the same words as the direct one —
    /// they share a core precisely so they cannot drift.
    #[test]
    fn signed_and_direct_paths_reject_identically() {
        let (mut ctx, sk) = fixture(1_000_000);

        let (reverted, reason) = call(&mut ctx, &to_input(&sk, 0, TO, false));
        assert!(reverted && reason.contains("transferToPerp: amount must be > 0"), "got {reason}");

        let (reverted, reason) = call(&mut ctx, &to_input(&sk, 9_999_999, TO, false));
        assert!(
            reverted && reason.contains("transferToPerp: insufficient spot balance"),
            "got {reason}"
        );
    }
}
