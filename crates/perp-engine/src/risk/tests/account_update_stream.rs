use super::*;
use crate::events::stream_test_support::{
    account_update_reasons, assert_account_update_groups, stream_shape,
};
use crate::types::AccountUpdateReason as R;

/// **A liquidation is SEVERAL `ACCOUNT_UPDATE` pushes, not one settled end state.**
///
/// The liquidated user's close goes through `match_order` as a taker, so it publishes the taker
/// group like any other order — the header immediately before the close's own position row. That
/// emit used to be SUPPRESSED for a `liquidation_close` (on the grounds that the account is only
/// half-liquidated at that point), which under the adjacency rule left the close's
/// `PositionChanged` an ORPHAN trailing the MAKER's group: an indexer would have booked the
/// liquidated user's flattened position onto the maker's account.
///
/// The later legs (here: nothing further moves ALICE's wallet, since
/// `liquidation_fee_rate_bps == 0`) re-mark the user where they exist and the drain publishes the
/// settled end state as a 0-position group.
#[test]
fn a_liquidation_close_publishes_the_liquidated_users_group_not_an_orphan_row() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    storage::begin_perp_call(&mut ctx);
    liquidate(&mut ctx, ALICE).unwrap();
    storage::flush_account_snapshots(&mut ctx).unwrap();

    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_account_update_groups(&logs);
    assert_eq!(
        stream_shape(&logs),
        vec![
            // the synthetic close order the liquidation places on ALICE's behalf
            ("OrderPlaced", Some(ALICE)),
            // the book leg: `Trade` outside the groups, MAKER's group, then ALICE's close group
            ("Trade", Some(ALICE)),
            ("AccountBalanceChanged", Some(MAKER)),
            ("PositionChanged", Some(MAKER)),
            ("AccountBalanceChanged", Some(ALICE)),
            ("PositionChanged", Some(ALICE)),
            ("Liquidation", Some(ALICE)),
            // …and NOTHING after it. `liquidation_fee_rate_bps == 0` here, so the clearance fee
            // writes nothing and re-marks nobody, and the book fill zeroed both position legs
            // exactly, so `execute_liquidation_market_order`'s residual cleanup has nothing to
            // write either. With a non-zero rate the clearance fee would add one legitimate
            // 0-position group carrying the debited wallet.
        ],
        "the liquidated user's close is her OWN push — the row must not trail the maker's group"
    );
    // Both rows report `ORDER`, and that is not a shortcut: the liquidation close really IS an
    // order — `execute_liquidation_market_order` builds one and runs it through `match_order`,
    // so the maker leg and the liquidated user's taker leg are the same code path any fill takes.
    // Binance reports its own liquidations the same way (their liquidation engine places orders;
    // their `a.m` vocabulary has no LIQUIDATION value at all).
    assert_eq!(
        account_update_reasons(&logs),
        vec![(MAKER, R::Order), (ALICE, R::Order)],
    );
}

/// **The clearance fee is `INSURANCE_CLEAR`.** Same fixture as the test above with a non-zero
/// `liquidation_fee_rate_bps`, which adds the one leg that test's comment predicts: a 0-position
/// drain group for the liquidated user carrying the fee debited to the insurance fund.
///
/// `INSURANCE_CLEAR` is assigned by a rule that is checkable rather than a vibe — *the
/// counterparty of this wallet move is the insurance fund* — which is why the same value covers
/// both directions (`depositInsuranceFund` / `withdrawInsuranceFund` move the admin's wallet
/// against the same pot).
#[test]
fn the_liquidation_clearance_fee_reports_insurance_clear() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // 5% of the notional closed, charged out of ALICE's wallet to the fund.
    let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    m.liquidation_fee_rate_bps = 500;
    storage::save_market(&mut ctx, &m).unwrap();
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64);
    let wallet_before = storage::load_account(&mut ctx, ALICE)
        .unwrap()
        .perp_wallet_balance;
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());

    storage::begin_perp_call(&mut ctx);
    liquidate(&mut ctx, ALICE).unwrap();
    storage::flush_account_snapshots(&mut ctx).unwrap();

    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_account_update_groups(&logs);
    assert_eq!(
        account_update_reasons(&logs),
        vec![
            (MAKER, R::Order),
            (ALICE, R::Order),
            // the drain row: nothing but the clearance fee moved ALICE after her close
            (ALICE, R::InsuranceClear),
        ],
        "the fee leg is its own push, and it names the insurance fund as the counterparty"
    );
    // …and the fee was real, so the row is not an artefact of a zero move: 5% of the $900
    // NOTIONAL closed (10 @ $90 — cross P3, Q3: the rate is on the notional, no longer on the
    // $200 silo), under its J5 cap (the $100 the close returned to the wallet), credited to
    // the fund.
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap() - if_before,
        900_000_000u64 * 500 / 10_000,
        "the fund really was credited the clearance fee"
    );
    assert!(
        storage::load_account(&mut ctx, ALICE)
            .unwrap()
            .perp_wallet_balance
            != wallet_before,
        "…and it came out of ALICE's wallet"
    );
}

/// **`ADJUSTMENT` for a SOLVENT residual ADL'd at MARK.** The book absorbs half the position and
/// the rest is a forced trade at the mark price — a real counterparty, but no order on either
/// side, so `ORDER` would send a consumer looking for an order id and a `Trade` row that do not
/// exist for this leg.
///
/// The sibling below pins the same two-group shape for an INSOLVENT residual (filled at the
/// bankruptcy price). The two differ only in price, which is exactly the claim worth pinning
/// twice: solvent residuals used to take a completely separate, counterparty-free code path.
#[test]
fn a_solvent_residual_adld_at_mark_reports_adjustment() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    // Only HALF the size is bid for, so the other half becomes the residual. The position is
    // below maintenance but not bankrupt at this mark, so the residual is SOLVENT and fills at
    // mark rather than at the bankruptcy price.
    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64 / 2);
    seed_absorbing_short(&mut ctx, 1);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    storage::begin_perp_call(&mut ctx);
    liquidate(&mut ctx, ALICE).unwrap();
    storage::flush_account_snapshots(&mut ctx).unwrap();

    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_account_update_groups(&logs);
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "fixture: the residual really did have to be ADL'd"
    );
    assert_eq!(
        account_update_reasons(&logs),
        vec![
            // the book leg, an ordinary fill for both sides
            (MAKER, R::Order),
            (ALICE, R::Order),
            // the residual: a forced trade at mark — one group per party, no order
            (ALICE, R::Adjustment),
            (WHALE, R::Adjustment),
        ],
    );
}

/// **One ADL fill is TWO pushes, one per party.** The `Adl` row sits outside both groups; the
/// loser's header is derived from the in-memory working copies (its position/account are written
/// once after the fill loop) and the winner's off the settled store its own two writes just made
/// current.
///
/// Merging the two was never an option: under the adjacency rule the second row would belong to
/// the first header, i.e. the counterparty's position booked onto the liquidated user's account.
/// This is the same fixture as
/// `adl_closes_insolvent_residual_against_opposite_holder_conserving_no_if`, asserted on the
/// stream instead of on the balances.
#[test]
fn an_adl_fill_publishes_two_groups_one_per_party() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(
        &mut ctx,
        ALICE,
        QTY,
        -ENTRY_VALUE,
        MARGIN,
        5,
        USER_WALLET as i64,
    );
    seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 5, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    // Crash to $75: ALICE is insolvent (bankruptcy price $80) and the book is empty, so the
    // whole residual goes to ADL against KEEPER. `run_update_index_price` is the shell-free
    // sweep entry, so the drain has to be driven by hand.
    storage::begin_perp_call(&mut ctx);
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: 7_500,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    storage::flush_account_snapshots(&mut ctx).unwrap();

    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_account_update_groups(&logs);
    assert_eq!(
        stream_shape(&logs),
        vec![
            // the synthetic close order: the book is empty, so it fills nothing and the whole
            // position becomes the insolvent residual ADL closes
            ("OrderPlaced", Some(ALICE)),
            ("Adl", Some(ALICE)),
            ("AccountBalanceChanged", Some(ALICE)),
            ("PositionChanged", Some(ALICE)),
            ("AccountBalanceChanged", Some(KEEPER)),
            ("PositionChanged", Some(KEEPER)),
            ("Liquidation", Some(ALICE)),
            // the oracle update that drove the sweep, emitted after it — ONE price event,
            // not the `IndexPriceUpdated` + `MarkPriceUpdated` pair it used to be
            ("MarkPriceUpdated", None),
        ],
        "one ADL fill → two pushes, each owning exactly its own party's position row"
    );
    // BOTH ADL legs are `ADJUSTMENT`. The winner is the clearer half of the argument: candidates
    // holding ANY resting order are excluded, so KEEPER provably placed no order, and the only
    // log naming them is `Adl` — there is no `Trade` row and no order id for a consumer to look
    // up, which is exactly what `ORDER` would promise.
    assert_eq!(
        account_update_reasons(&logs),
        vec![(ALICE, R::Adjustment), (KEEPER, R::Adjustment)],
    );
}

/// The admin's own two insurance-fund selectors report `INSURANCE_CLEAR` as well — the same rule
/// the clearance fee is assigned by (*the counterparty of this wallet move is the fund*), just
/// with the admin on the other end and both directions of it.
///
/// Driven directly rather than through the shell, so the call boundary that drains the coalescing
/// set is explicit; both selectors produce a plain 0-position group.
#[test]
fn the_insurance_fund_selectors_report_insurance_clear() {
    use crate::interface::IPerpDex::{depositInsuranceFundCall, withdrawInsuranceFundCall};
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // The fund is never written directly (that would be a mint); it is seeded out of the admin's
    // own perp wallet, which is exactly the move under test.
    storage::save_account(
        &mut ctx,
        ADMIN,
        UserAccount {
            perp_wallet_balance: 1_000_000,
            ..UserAccount::default()
        },
        R::Adjustment,
    )
    .unwrap();

    for (call, label) in [
        (depositInsuranceFundCall { amount: 400_000 }.abi_encode(), "deposit"),
        (
            withdrawInsuranceFundCall { amount: 400_000 }.abi_encode(),
            "withdraw",
        ),
    ] {
        let _ = JournalTr::take_logs(ctx.journal_mut());
        storage::begin_perp_call(&mut ctx);
        if label == "deposit" {
            run_deposit_insurance_fund(&call, ADMIN, &mut ctx).unwrap();
        } else {
            run_withdraw_insurance_fund(&call, ADMIN, &mut ctx).unwrap();
        }
        storage::flush_account_snapshots(&mut ctx).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_account_update_groups(&logs);
        assert_eq!(
            account_update_reasons(&logs),
            vec![(ADMIN, R::InsuranceClear)],
            "{label}InsuranceFund moves the admin's wallet against the fund"
        );
    }
}

/// **The one genuinely REACHABLE `Multiple`.** The drain publishes one row per user per call, so
/// a user moved by two different causes with neither row published inline gets a single row that
/// no single label describes — and rather than pick one and lie about the other, it says
/// `Multiple`.
///
/// ⚠️ **RE-POINTED.** This used to reach `Multiple` through funding-plus-clearance-fee, on a
/// fixture where the liquidation closed NOTHING (empty book, insolvent residual, no eligible
/// ADL counterparty): nothing published for ALICE, so the `FundingFee` mark was still standing
/// when the clearance fee marked her `InsuranceClear`.
///
/// That shape is gone, because the clearance fee is now levied on what the call actually
/// CLOSED. A liquidation that closed nothing charges nothing, so there is no second cause —
/// and the two paths that DO close (the book leg and ADL) both publish for the user
/// inline, which clears the funding mark. The combination is unreachable by construction.
///
/// So this now pins the new rule where the old one was observable, which is the same corner
/// from the other side: nothing closed, therefore no fee, therefore exactly ONE cause. The
/// remaining candidate for a genuine `Multiple` is the rounding write-off in
/// `execute_liquidation_market_order`'s full-fill path meeting the fee's `InsuranceClear` (that
/// site's own comment says so); it needs a hand-tuned residue and has no fixture yet.
/// `AccountUpdateReason::merge` itself is a four-line pure function with its wire values pinned
/// in `perp-core`.
#[test]
fn a_liquidation_that_closes_nothing_charges_no_fee_and_reports_one_cause() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    m.liquidation_fee_rate_bps = 500;
    storage::save_market(&mut ctx, &m).unwrap();
    // A long that is insolvent at $75 (bankruptcy price $80) with a positive wallet, so the
    // clearance fee has something to bite. No KEEPER position at all → no ADL candidate.
    seed_position_account(
        &mut ctx,
        ALICE,
        QTY,
        -ENTRY_VALUE,
        MARGIN,
        5,
        USER_WALLET as i64,
    );
    // An epoch a LONG owes against, so `compute_funding_settlement` really moves `pos.margin`
    // and `liquidate_position`'s post-funding position write marks her `FundingFee`.
    storage::save_funding_state(
        &mut ctx,
        MARKET_ID,
        &FundingState {
            last_funding_rate: 1_000,
            next_funding_ts: u64::MAX,
            cumulative_funding_index: ENTRY_PRICE as i128 * 1_000,
        },
    )
    .unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());

    storage::begin_perp_call(&mut ctx);
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: 7_500,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    storage::flush_account_snapshots(&mut ctx).unwrap();

    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_account_update_groups(&logs);
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        QTY,
        "fixture: nothing may close — empty book, insolvent residual, no ADL counterparty"
    );
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        0,
        "a liquidation that consumed no margin charges no clearance fee, however many times \
         it is retried — this is what stops a never-ADL-able position from having its wallet \
         drained one fee per oracle tick forever"
    );
    let reasons = account_update_reasons(&logs);
    assert_eq!(
        reasons
            .iter()
            .filter(|(u, _)| *u == ALICE)
            .map(|(_, r)| *r)
            .collect::<Vec<_>>(),
        // EXACTLY ONE group: the funding settle, published inline. No clearance fee (nothing
        // closed), and the post-funding position write's mark is cleared because the drain
        // would only repeat that same payload with no position rows.
        vec![R::FundingFee],
        "got {reasons:?}"
    );
}
