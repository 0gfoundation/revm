use super::*;
use crate::events::stream_test_support::stream_shape;
use crate::interface::IPerpDex::{
    Adl, FundingSettled, InsuranceFundChanged, InsuranceFundDepleted, Liquidation,
    OrderCancelled, PositionChanged, PositionMarginAdjusted, Trade,
};
use crate::math::{calc_bankruptcy_price, calc_funding_payment, maintenance_margin};

const SELL: u8 = Side::Sell as u8;
const BUY: u8 = Side::Buy as u8;
/// $130.00 — the mark at which a $100 long is held up by $300 of unrealized profit.
const MARK_130: u64 = 13_000;
/// Notional of `QTY` at `MARK_130`.
const N_130: i64 = 1_300_000_000;
/// `⌊N_130 / 6⌋`.
const MM_130: i64 = 216_666_666;
/// `⌊ENTRY_VALUE / 6⌋`.
const MM_100: i64 = 166_666_666;
/// Funding index giving a 250M charge on the `QTY` long (`2.5e9 / 10`).
const INDEX_250M: i128 = 2_500_000_000;

/// ALICE's funding settled AND PERSISTED — exactly the write `liquidate_position` makes
/// (`compute` → `apply` → `save_position(FundingFee)`) — so a test can start from a STORED
/// funding debt and measure the next operation on its own. Its funding events are discarded.
fn persist_alice_funding(ctx: &mut TestCtx) -> PerpPosition {
    let (pos, _) = settle_alice_funding(ctx);
    storage::save_position(ctx, ALICE, MARKET_ID, &pos, AccountUpdateReason::FundingFee)
        .unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());
    pos
}

/// `seed_position_account` with an explicit funding anchor, for counterparties that must (or
/// must not) carry pending funding at the current index.
fn seed_anchored(
    ctx: &mut TestCtx,
    user: Address,
    amount: i64,
    v_quote: i64,
    margin: i64,
    wallet: i64,
    last_funding_index: i128,
) {
    storage::save_account(
        ctx,
        user,
        UserAccount {
            perp_wallet_balance: wallet,
            ..UserAccount::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    storage::save_position(
        ctx,
        user,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance: v_quote,
            margin,
            leverage: 5,
            last_funding_index,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

fn set_taker_ppm(ctx: &mut TestCtx, user: Address, ppm: u64) {
    storage::save_user_fee_rates(
        ctx,
        user,
        UserFeeRates {
            maker_fee_ppm: 0,
            taker_fee_ppm: ppm,
        },
    )
    .unwrap();
}

fn set_wallet(ctx: &mut TestCtx, user: Address, wallet: i64) {
    let mut a = storage::load_account(ctx, user).unwrap();
    a.perp_wallet_balance = wallet;
    storage::save_account(ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
}

fn signed_wallet(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_account(ctx, user).unwrap().perp_wallet_balance
}

/// The stored `Σ pos.margin` scalar agrees with the one position the user holds — the Σ the
/// `AccountBalanceChanged` header and `totalWalletBalance` are built from. Returns it.
fn assert_sigma_margin_consistent(ctx: &mut TestCtx, user: Address) -> i64 {
    let sigma = storage::load_account(ctx, user).unwrap().total_position_margin;
    assert_eq!(sigma, position(ctx, user).margin, "{user}: stored Σ pos.margin drifted");
    sigma
}

fn sell_as_alice(ctx: &mut TestCtx, price: u64, qty: u64) -> Vec<primitives::Log> {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    try_place_order(ctx, ALICE, SELL, price, qty).unwrap();
    JournalTr::take_logs(ctx.journal_mut())
}

// ── C1: the sweep finds a profit-supported long at exactly the closed-form epoch ─────────────

/// **A long held up by unrealized profit is liquidated by funding alone, at exactly the epoch the
/// closed form predicts.**
///
/// Long 10 @ $100 with a silo `M = 199_416_666`, marked at $130: `U = 300M ≥ MM = 216_666_666`.
/// Under the clamp this replaced, equity was `max(M − F, 0) + U ≥ U > MM` for EVERY cumulative
/// charge `F` — the position could never be liquidated at this mark, and once the silo hit 0 the
/// insurance fund paid its funding every epoch. Now equity is `M + U − F`, so it is liquidatable
/// iff `F > M + U − MM = 282_750_000`.
///
/// Each epoch charges the CAPPED rate, `mark × MAX_FUNDING_RATE = 13_000 × 7_500 = 97_500_000`
/// of index, i.e. 9_750_000 (0.75% of the notional) on this long, so
/// `k* = ⌊282_750_000 / 9_750_000⌋ + 1 = 30`. `M` is chosen so the threshold is EXACTLY 29
/// epochs' charge: at epoch 29 (`F = 282_750_000`) equity is `216_666_666 == MM` — healthy only
/// because the predicate is `>=` — and at epoch 30 (`F = 292_500_000`) it is
/// `206_916_666 < MM`. So the test pins the boundary itself, not just the epoch: a strict `>`
/// would liquidate one epoch early. At `k* − 1` a manual `liquidate` is also refused ("above
/// maintenance margin") — a positive witness that the predicate ran and said healthy, not
/// merely that nothing happened.
///
/// Driven through REAL oracle ticks at an unchanged $130 index: the sweep is what must find
/// it, with nobody touching the position. Every tick before `k*` must leave the position
/// exactly as stored (a healthy candidate is write-free, so funding is never persisted and
/// each tick prices the WHOLE accrued delta with a single floor). The liquidation at `k*` is
/// SOLVENT (equity +206_916_666), so the empty-book residual ADLs at MARK against KEEPER — a
/// short that has been RECEIVING the same funding, unsettled, and is paid it at the fill (C7).
#[test]
fn c1_a_profit_supported_long_paying_funding_is_swept_at_exactly_the_closed_form_epoch() {
    const STEP: i128 = 97_500_000;
    const CHARGE: i64 = 9_750_000;
    // `MM_130 − U + 29 · CHARGE`: puts equity at `k* − 1` exactly on maintenance.
    const M: i64 = 199_416_666;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, M, 5, USER_WALLET as i64);
    // A well-collateralised short: equity at $130 is 700M, so it stays healthy at every tick.
    seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, 1_000_000_000, 5, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();

    // ── the closed form ──
    assert_eq!(STEP, MARK_130 as i128 * crate::math::MAX_FUNDING_RATE as i128);
    let u = N_130 - ENTRY_VALUE;
    assert_eq!(maintenance_margin(&market.tiers, N_130).unwrap(), MM_130);
    assert!(u >= MM_130, "profit-supported: the old clamp kept equity ≥ U > MM forever");
    let threshold = M + u - MM_130;
    assert_eq!(threshold, 282_750_000);
    assert_eq!(threshold, 29 * CHARGE, "the threshold sits exactly on an epoch");
    let k_star = threshold / CHARGE + 1;
    assert_eq!(k_star, 30);
    for k in [k_star - 1, k_star] {
        // One floor over the whole accrued delta is exactly k epochs' charge here.
        assert_eq!(
            calc_funding_payment(QTY, STEP * k as i128, 0, PRICE_DECIMALS).unwrap(),
            -CHARGE * k
        );
    }
    assert_eq!(M + u - CHARGE * (k_star - 1), MM_130, "k* − 1: equity EXACTLY MM");
    assert_eq!(M + u - CHARGE * k_star, 206_916_666, "k*: below MM, still solvent");

    let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
    assert_eq!(value_before, 1_249_416_666);

    for k in 1..k_star {
        set_funding_index(&mut ctx, STEP * k as i128);
        oracle_tick(&mut ctx, MARK_130, 15 * (k as u64 + 1) + 1);
        assert_eq!(storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(), MARK_130);
        let p = position(&mut ctx, ALICE);
        assert_eq!(
            (p.amount, p.v_quote_balance, p.margin, p.last_funding_index),
            (QTY, -ENTRY_VALUE, M, 0),
            "epoch {k}: F = {} ≤ {threshold} — above maintenance, and the healthy sweep \
             persisted nothing",
            CHARGE * k
        );
    }
    // The positive witness at `k* − 1`: equity == MM is HEALTHY, so a manual liquidation is
    // refused by the predicate itself (and, like the sweep, persists nothing).
    let err = liquidate(&mut ctx, ALICE).unwrap_err();
    assert!(err.to_string().contains("above maintenance margin"), "{err}");
    let p = position(&mut ctx, ALICE);
    assert_eq!((p.amount, p.margin, p.last_funding_index), (QTY, M, 0));

    set_funding_index(&mut ctx, STEP * k_star as i128);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    oracle_tick(&mut ctx, MARK_130, 15 * (k_star as u64 + 1) + 1);
    let logs = JournalTr::take_logs(ctx.journal_mut());

    let alice = position(&mut ctx, ALICE);
    assert_eq!(
        (alice.amount, alice.v_quote_balance, alice.margin),
        (0, 0, 0),
        "epoch k* = {k_star}: funding alone liquidated it, and the close released the debt"
    );
    // Close at the $130 mark: −93_083_334 of margin (the debt `M − 292.5M`) + 300M realized
    // = +206_916_666.
    assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64 + 206_916_666);
    assert_eq!(signed_wallet(&mut ctx, ALICE), 256_916_666);
    let alice_funding: Vec<i64> = decode_events::<FundingSettled>(&logs)
        .into_iter()
        .filter(|f| f.user == ALICE)
        .map(|f| f.amount)
        .collect();
    assert_eq!(alice_funding, vec![-CHARGE * k_star], "30 epochs settled ONCE, one floor");
    let adl = decode_events::<Adl>(&logs);
    assert_eq!(adl.len(), 1);
    assert_eq!(
        (adl[0].adlUser, adl[0].qty, adl[0].price),
        (KEEPER, QTY as u64, MARK_130),
        "solvent residual → ADL at MARK"
    );
    // KEEPER: margin 1e9 + 292.5M received funding, closed at mark: −300M realized.
    let keeper = position(&mut ctx, KEEPER);
    assert_eq!((keeper.amount, keeper.margin), (0, 0));
    assert_eq!(keeper.last_funding_index, STEP * k_star as i128);
    assert_eq!(signed_wallet(&mut ctx, KEEPER), 992_500_000);
    assert_eq!(
        conservation_sum(&mut ctx, &[ALICE, KEEPER]),
        value_before,
        "funding was zero-sum between the two and every leg conserved"
    );
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0, "no fund involvement");
}

// ── C2 / C3: closes net the debt against the PnL ───────────────────────────────────────────

/// **C2 — a full voluntary close of a debt-laden long, in profit, through a real fill.**
///
/// Long 10 @ $100, silo 200M, a 250M funding charge PERSISTED first → margin −50M (`D`), the
/// stored `Σ pos.margin` −50M with it. Then ALICE sells all 10 @ $130 to MAKER at 10 bps
/// (fee 1_300_000). `apply_position_fill` releases the margin SIGNED: credit
/// `= −50M + 300M = 250M`, so `wallet += realized − D − fee = 300M − 50M − 1.3M = 248.7M`, margin
/// ends EXACTLY 0 (the flat ⇒ margin == 0 invariant `save_position` asserts), and the fund does
/// not move. Under the clamp this replaced, the close credited the full 300M and left −50M on a
/// flat position.
#[test]
fn c2_a_full_close_in_profit_nets_the_funding_debt_against_the_realized_pnl() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    set_funding_index(&mut ctx, INDEX_250M);
    let settled = persist_alice_funding(&mut ctx);
    assert_eq!(settled.margin, -50_000_000);
    assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), -50_000_000);

    set_taker_ppm(&mut ctx, ALICE, 1_000);
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
    place_maker_order(&mut ctx, BUY, MARK_130, QTY as u64);
    let users = [ALICE, MAKER, ADMIN];
    let value_before = conservation_sum(&mut ctx, &users);
    let wallet_before = signed_wallet(&mut ctx, ALICE);

    let logs = sell_as_alice(&mut ctx, MARK_130, QTY as u64);

    let trade = decode_events::<Trade>(&logs);
    assert_eq!(trade.len(), 1);
    assert_eq!(
        (trade[0].taker, trade[0].takerFee, trade[0].takerRealizedPnl),
        (ALICE, 1_300_000, 300_000_000)
    );
    assert_eq!(
        signed_wallet(&mut ctx, ALICE) - wallet_before,
        300_000_000 - 50_000_000 - 1_300_000,
        "wallet += realized − D − fee, exactly"
    );
    let alice = position(&mut ctx, ALICE);
    assert_eq!((alice.amount, alice.v_quote_balance, alice.margin), (0, 0, 0));
    assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), 0, "the debt left Σ too");
    assert_eq!(assert_sigma_margin_consistent(&mut ctx, MAKER), N_130, "MAKER opened at 1×");
    let row = decode_events::<PositionChanged>(&logs)
        .into_iter()
        .find(|r| r.user == ALICE)
        .unwrap();
    assert_eq!(
        (row.margin, row.realizedPnl, row.closedQuantity),
        (0, 300_000_000, QTY as u64),
        "realizedPnl stays GROSS — the debt is a margin release, not PnL"
    );
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 80_000_000, "fund untouched");
    assert!(decode_events::<InsuranceFundChanged>(&logs).is_empty());
    assert_eq!(
        conservation_sum(&mut ctx, &users),
        value_before,
        "Σ(wallet + margin + vQuote) + fund + fee recipient conserved across the close"
    );
}

/// **C3 — a 50% close, then the rest, and separately a flip, each on a stored funding debt.**
///
/// Margin −50_000_001 (a 250_000_001 charge on the 200M silo) — ODD, so the 50% slice is not an
/// exact half. `remaining = trunc0(−50_000_001 · 5 / 10) = −25_000_000`, so the closing slice
/// carries `−25_000_001`: truncation toward zero puts the extra unit of debt on the slice being
/// closed, never on the remainder. (A floor would leave −25_000_001 behind.)
///
/// * sell 5 @ $130, 10 bps (fee 650_000): realized `−500M + 650M = 150M`, credit
///   `−25_000_001 + 150M = 124_999_999`, wallet `50M + 124_999_999 − 650_000 = 174_349_999`; the
///   remainder is exactly `(5, −500M, −25_000_000)`.
/// * sell the other 5: credit `−25_000_000 + 150M`, wallet `298_699_999`, margin 0 — the whole
///   50_000_001 debt collected across the two closes.
/// * the flip (fresh fixture, leverage 2, wallet 1e9): sell 20 @ $130, fee 2_600_000 on the full
///   2.6e9. The closing leg is the whole old long: credit `−50_000_001 + 300M = 249_999_999`. The
///   opening short is funded `1.3e9 / 2 = 650M` with the fee out of it: margin `647_400_000`, NO
///   debt inherited; wallet `1e9 + 249_999_999 − 650M = 599_999_999`.
///
/// Each operation conserves `Σ(wallet + margin + vQuote) + fund + fee recipient` exactly (the
/// debt was persisted beforehand, so no funding is settled inside them).
#[test]
fn c3_a_partial_close_and_a_flip_carry_the_slices_trunc0_share_of_the_debt() {
    const INDEX: i128 = 2_500_000_010; // charge 250_000_001
    let users = [ALICE, MAKER, ADMIN];

    // ── 50% close, then the rest ──
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    set_funding_index(&mut ctx, INDEX);
    assert_eq!(persist_alice_funding(&mut ctx).margin, -50_000_001);
    set_taker_ppm(&mut ctx, ALICE, 1_000);
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
    place_maker_order(&mut ctx, BUY, MARK_130, QTY as u64);
    let value_before = conservation_sum(&mut ctx, &users);

    let logs = sell_as_alice(&mut ctx, MARK_130, 5);
    let trade = decode_events::<Trade>(&logs);
    assert_eq!((trade[0].takerFee, trade[0].takerRealizedPnl), (650_000, 150_000_000));
    let alice = position(&mut ctx, ALICE);
    assert_eq!(
        (alice.amount, alice.v_quote_balance, alice.margin),
        (5, -500_000_000, -25_000_000),
        "the remainder keeps trunc0 of its share; the slice took −25_000_001"
    );
    assert_eq!(signed_wallet(&mut ctx, ALICE), 174_349_999);
    assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), -25_000_000);
    assert_eq!(conservation_sum(&mut ctx, &users), value_before, "partial close conserves");

    let logs = sell_as_alice(&mut ctx, MARK_130, 5);
    let trade = decode_events::<Trade>(&logs);
    assert_eq!((trade[0].takerFee, trade[0].takerRealizedPnl), (650_000, 150_000_000));
    let alice = position(&mut ctx, ALICE);
    assert_eq!((alice.amount, alice.v_quote_balance, alice.margin), (0, 0, 0));
    assert_eq!(signed_wallet(&mut ctx, ALICE), 298_699_999);
    assert_eq!(
        signed_wallet(&mut ctx, ALICE),
        USER_WALLET as i64 + 2 * 150_000_000 - 50_000_001 - 2 * 650_000,
        "over both closes: the whole debt, once"
    );
    assert_eq!(conservation_sum(&mut ctx, &users), value_before, "and the rest conserves");

    // ── the flip ──
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_wallet(&mut ctx, ALICE, 1_000_000_000);
    set_wallet(&mut ctx, MAKER, 10_000_000_000);
    save_position_with_leverage(&mut ctx, QTY, -ENTRY_VALUE, 2);
    set_funding_index(&mut ctx, INDEX);
    assert_eq!(persist_alice_funding(&mut ctx).margin, -50_000_001);
    set_taker_ppm(&mut ctx, ALICE, 1_000);
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
    place_maker_order(&mut ctx, BUY, MARK_130, 2 * QTY as u64);
    let value_before = conservation_sum(&mut ctx, &users);

    let logs = sell_as_alice(&mut ctx, MARK_130, 2 * QTY as u64);
    let trade = decode_events::<Trade>(&logs);
    assert_eq!(trade.len(), 1);
    assert_eq!((trade[0].takerFee, trade[0].takerRealizedPnl), (2_600_000, 300_000_000));
    let alice = position(&mut ctx, ALICE);
    assert_eq!(
        (alice.amount, alice.v_quote_balance, alice.margin),
        (-QTY, N_130, 650_000_000 - 2_600_000),
        "the new short's silo is exactly its own IM net of the fee — the debt stayed with the \
         closed long"
    );
    assert_eq!(signed_wallet(&mut ctx, ALICE), 599_999_999);
    assert_eq!(
        signed_wallet(&mut ctx, ALICE),
        1_000_000_000 + (300_000_000 - 50_000_001) - 650_000_000
    );
    assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), 647_400_000);
    assert_eq!(conservation_sum(&mut ctx, &users), value_before, "the flip conserves");
}

// ── C4: a genuine shortfall at close is the only way a debt reaches the fund ─────────────────

/// **C4 — an insolvent close of a debt-laden long: the fund pays the SHORTFALL, via bad debt.**
///
/// Long 10 @ $100, silo 200M, a 250M charge pending; ALICE sells all 10 @ $102 (realized +20M).
/// The fill settles the funding first (margin −50M), then the close: credit
/// `−50M + 20M = −30M` → nothing left to draw → `bad_debt = 30M`, routed to the fund. The wallet
/// does not move (isolated margin).
///
/// Two rows, the fund the only difference:
/// * fund 80M: it covers the 30M — `InsuranceFundChanged(−30M, 50M)`, no depletion;
/// * fund 10M: it covers only 10M — `InsuranceFundChanged(−10M, 0)` and then
///   `InsuranceFundDepleted(MARKET_ID, 20M)`, the 20M socialised. This is the depletion path
///   the retired funding test used to pin; funding can no longer reach it, a close can.
///
/// The contrast is the point: under the clamp this replaced, FUNDING drew 50M from the fund
/// on the spot and the close then paid the user its 20M profit. Now the profit nets against the
/// debt and the fund pays only the 30M nobody can — and the event comes from the close.
///
/// Value over `[ALICE, MAKER, ADMIN]` + fund moves by exactly −250M — ALICE's funding, which has
/// no receiver here (the fund's payment of the shortfall is internal) — plus, in the depletion
/// row, the 20M write-off: socialised debt is value nobody paid, so the measured sum RISES by it.
#[test]
fn c4_an_insolvent_close_routes_only_the_shortfall_to_the_fund_through_bad_debt() {
    for (fund, absorbed, depleted) in
        [(80_000_000u64, 30_000_000i64, 0u64), (10_000_000, 10_000_000, 20_000_000)]
    {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_insurance_fund(&mut ctx, fund).unwrap();
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        set_funding_index(&mut ctx, INDEX_250M);
        storage::save_mark_price(&mut ctx, MARKET_ID, 10_200).unwrap();
        place_maker_order(&mut ctx, BUY, 10_200, QTY as u64);
        let users = [ALICE, MAKER, ADMIN];
        let value_before = conservation_sum(&mut ctx, &users);

        let logs = sell_as_alice(&mut ctx, 10_200, QTY as u64);

        let funding = decode_events::<FundingSettled>(&logs);
        assert_eq!(funding.len(), 1);
        assert_eq!((funding[0].user, funding[0].amount), (ALICE, -250_000_000));
        let trade = decode_events::<Trade>(&logs);
        assert_eq!(trade[0].takerRealizedPnl, 20_000_000);
        let new_fund = fund - absorbed as u64;
        let changed = decode_events::<InsuranceFundChanged>(&logs);
        assert_eq!(changed.len(), 1, "fund {fund}: one fund movement — the close's");
        assert_eq!((changed[0].delta, changed[0].newBalance), (-absorbed, new_fund));
        let depletion: Vec<(u64, u64)> = decode_events::<InsuranceFundDepleted>(&logs)
            .into_iter()
            .map(|d| (d.marketId, d.badDebt))
            .collect();
        if depleted == 0 {
            assert!(depletion.is_empty(), "fund {fund}: covered in full");
        } else {
            assert_eq!(
                depletion,
                vec![(MARKET_ID, depleted)],
                "fund {fund}: the uncovered rest, socialised"
            );
        }
        // The stream: the taker's funding group (its row carrying the −50M debt) is published at
        // the taker's settlement, AFTER the maker's fill group, and the fund rows follow it — the
        // fund moves for the CLOSE, ahead of the close's own group (margin 0).
        let mut expected = vec![
            ("OrderPlaced", Some(ALICE)),
            ("Trade", Some(ALICE)),
            ("AccountBalanceChanged", Some(MAKER)),
            ("PositionChanged", Some(MAKER)),
            ("FundingSettled", Some(ALICE)),
            ("AccountBalanceChanged", Some(ALICE)),
            ("PositionChanged", Some(ALICE)),
            ("InsuranceFundChanged", None),
        ];
        if depleted > 0 {
            expected.push(("InsuranceFundDepleted", None));
        }
        expected.extend([
            ("AccountBalanceChanged", Some(ALICE)),
            ("PositionChanged", Some(ALICE)),
        ]);
        assert_eq!(stream_shape(&logs), expected, "fund {fund}");
        let rows: Vec<i64> = decode_events::<PositionChanged>(&logs)
            .into_iter()
            .filter(|r| r.user == ALICE)
            .map(|r| r.margin)
            .collect();
        assert_eq!(rows, vec![-50_000_000, 0], "the debt, then the flat close");

        assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), new_fund);
        assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64, "wallet untouched");
        let alice = position(&mut ctx, ALICE);
        assert_eq!((alice.amount, alice.margin), (0, 0));
        assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), 0);
        assert_eq!(
            conservation_sum(&mut ctx, &users) - value_before,
            -250_000_000 + depleted as i128,
            "fund {fund}: ALICE's unreceived funding, plus any socialised write-off"
        );
    }
}

/// **C4, partial — an insolvent PARTIAL close of a debt-laden long draws nothing from a
/// remainder that is itself in debt; the slice's whole deficit is bad debt for the fund.**
///
/// Long 10 @ $100, silo 200M, a 250M charge PERSISTED → margin −50M; fund 80M. ALICE sells 5 @
/// $95 into a 5 @ $95 bid: the slice releases `−50M − trunc0(−50M · 5/10) = −25M`, realizes
/// `−500M + 475M = −25M`, so its credit is −50M. The remainder keeps margin −25M — a debt, so
/// the insolvent draw's `.max(0)` finds nothing to draw and the fund pays the 50M deficit
/// (80M → 30M). The wallet never moves.
///
/// Every other debt close in this module is a FULL close, where the remainder's margin is 0 and
/// the clamp is invisible. Without it a debt remainder would be "drawn" from: cast to `u64` it
/// wraps to ~1.8e19 and swallows the whole deficit (margin −75M, no bad debt, fund untouched —
/// a hidden debt piled onto an insolvent position); taken signed it RAISES the margin to 0 and
/// over-bills the fund by 25M.
#[test]
fn c4_a_partial_insolvent_close_draws_nothing_from_a_remainder_that_is_itself_in_debt() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    set_funding_index(&mut ctx, INDEX_250M);
    assert_eq!(persist_alice_funding(&mut ctx).margin, -50_000_000);
    storage::save_mark_price(&mut ctx, MARKET_ID, 9_500).unwrap();
    place_maker_order(&mut ctx, BUY, 9_500, 5);
    let users = [ALICE, MAKER, ADMIN];
    let value_before = conservation_sum(&mut ctx, &users);
    let wallet_before = signed_wallet(&mut ctx, ALICE);

    let logs = sell_as_alice(&mut ctx, 9_500, 5);

    let trade = decode_events::<Trade>(&logs);
    assert_eq!(trade.len(), 1);
    assert_eq!(trade[0].takerRealizedPnl, -25_000_000);
    let alice = position(&mut ctx, ALICE);
    assert_eq!(
        (alice.amount, alice.v_quote_balance, alice.margin),
        (5, -500_000_000, -25_000_000),
        "the remainder keeps exactly its own share of the debt"
    );
    let changed = decode_events::<InsuranceFundChanged>(&logs);
    assert_eq!(changed.len(), 1);
    assert_eq!((changed[0].delta, changed[0].newBalance), (-50_000_000, 30_000_000));
    assert!(decode_events::<InsuranceFundDepleted>(&logs).is_empty());
    assert_eq!(signed_wallet(&mut ctx, ALICE), wallet_before, "isolated: wallet untouched");
    assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), -25_000_000);
    assert_eq!(conservation_sum(&mut ctx, &users), value_before);
}

// ── C5: liquidation through the book ─────────────────────────────────────────────────────

/// **C5 — liquidating a debt-laden long through the book: the full fill releases the debt
/// (margin EXACTLY 0) and the clearance fee is the notional one.**
///
/// C1's state at `k*`: long 10 @ $100, silo 200M, 292.5M of funding pending → −92.5M once
/// settled, at a $130 mark (equity 207.5M < MM). MAKER bids all 10 at $130; the clearance rate
/// is 5%. The close: taker fee waived, realized +300M, credit `−92.5M + 300M = 207.5M`.
///
/// * The flat position holds margin 0, so `execute_liquidation_market_order`'s
///   `debug_assert_eq!(pos.margin, 0)` does not fire — and its write-off branch, which would
///   have ZEROED the debt, is not taken.
/// * The clearance fee is 5% of the $1,300 NOTIONAL closed = 65M (cross P3, Q3), under its J5
///   cap (the 207.5M close credit). It used to be 0 here: on the old base — the silo margin the
///   close consumed, `margin.max(0) = 0` for a debt-laden silo — every funding-driven
///   liquidation of a profitable long was fee-free (a long with `U ≥ MM` can only BE liquidated
///   once its margin is negative), a quirk of the base rather than a rule.
#[test]
fn c5_a_book_liquidation_releases_the_debt_and_charges_the_notional_clearance_fee() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    m.liquidation_fee_rate_bps = 500;
    storage::save_market(&mut ctx, &m).unwrap();
    storage::save_insurance_fund(&mut ctx, 10_000_000).unwrap();
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    set_funding_index(&mut ctx, 2_925_000_000); // 292.5M
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
    place_maker_order(&mut ctx, BUY, MARK_130, QTY as u64);
    let users = [ALICE, MAKER, ADMIN];
    let value_before = conservation_sum(&mut ctx, &users);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    liquidate(&mut ctx, ALICE).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());

    let funding = decode_events::<FundingSettled>(&logs);
    assert_eq!(funding.len(), 1);
    assert_eq!(funding[0].amount, -292_500_000);
    let trade = decode_events::<Trade>(&logs);
    assert_eq!(trade.len(), 1);
    assert_eq!(
        (trade[0].price, trade[0].quantity, trade[0].takerFee, trade[0].takerRealizedPnl),
        (MARK_130, QTY as u64, 0, 300_000_000)
    );
    let alice = position(&mut ctx, ALICE);
    assert_eq!(
        (alice.amount, alice.v_quote_balance, alice.margin),
        (0, 0, 0),
        "the full fill released the debt itself — nothing for the write-off to forgive"
    );
    assert_eq!(
        signed_wallet(&mut ctx, ALICE),
        USER_WALLET as i64 + 207_500_000 - 65_000_000
    );
    let liq = decode_events::<Liquidation>(&logs);
    assert_eq!(liq.len(), 1);
    assert_eq!(liq[0].reward, 65_000_000, "5% of the $1,300 notional closed");
    assert_eq!(decode_events::<InsuranceFundChanged>(&logs).len(), 1);
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 10_000_000 + 65_000_000);
    assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), 0);
    assert_eq!(
        conservation_sum(&mut ctx, &users) - value_before,
        -292_500_000,
        "the only change in system value is the funding ALICE paid (no receiver here)"
    );
}

// ── C6: a short whose debt outgrew its entry proceeds, in the oracle sweep ───────────────────

/// Short 10 @ $100 (vQuote 1e9, silo 200M) that has PAID 1.3e9 of funding (index −1.3e10: a
/// falling index charges shorts) → margin −1.1e9, credit `vQuote + margin = −100M`: underwater
/// at every price, `calc_bankruptcy_price` 0. KEEPER is the long on the other side; the book is
/// empty, so the whole position is an ADL residual. Mark $100, fund 1e9.
fn debt_short_fixture(keeper_anchor: i128) -> TestCtx {
    const INDEX: i128 = -13_000_000_000;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 1_000_000_000).unwrap();
    seed_anchored(&mut ctx, ALICE, -QTY, ENTRY_VALUE, MARGIN, USER_WALLET as i64, 0);
    seed_anchored(&mut ctx, KEEPER, QTY, -ENTRY_VALUE, MARGIN, 0, keeper_anchor);
    set_funding_index(&mut ctx, INDEX);
    assert_eq!(
        calc_funding_payment(-QTY, INDEX, 0, PRICE_DECIMALS).unwrap(),
        -1_300_000_000,
        "the short pays 1.3e9"
    );
    assert_eq!(
        calc_bankruptcy_price(-QTY, ENTRY_VALUE, MARGIN - 1_300_000_000, 0, PRICE_DECIMALS)
            .unwrap(),
        0,
        "no representable bankruptcy price (this was an `Err` → a sweep PANIC)"
    );
    ctx
}

/// **C6 — the sweep on a short with `vQuote + margin < 0` does not panic; ADL fills at ONE price
/// unit under `LoserOnly`; Σ amount is conserved; the fund absorbs the loser's shortfall.**
///
/// KEEPER (long 10 @ $100, silo 200M) is anchored at 0, so it is OWED the same 1.3e9 — its
/// effective margin is 1.5e9, which is what puts it in the CLEAN tier at price 1:
/// `eq_fill = calc_value(1, 10) − 1e9 + 1.5e9 = 100_000 + 500M ≥ 0`. (On its stored 200M it would
/// be −799_900_000 and fall to the fallback tier — the next test.)
///
/// WHALE makes that tiering OBSERVABLE: a second long 10 @ $100 with a 300M silo and nothing
/// pending (anchored at the current index). Its equity at the mark is 300M, above KEEPER's
/// STORED 200M, but at price 1 it is `100_000 − 1e9 + 300M < 0` → fallback tier. On effective
/// margin KEEPER is clean and filled first, and WHALE is never reached. Judged on STORED margin
/// both would be fallback, ranked healthiest-at-mark first — and WHALE would be the one closed.
///
/// Fill 10 @ price 1 (value 100_000):
/// * ALICE: release −1.1e9, realized `1e9 − 100_000` → credit `−100_100_000` → `bad_debt`
///   100_100_000 (the only debt `LoserOnly` permits); wallet untouched, margin 0.
/// * KEEPER: funding settled first (+1.3e9 → 1.5e9), release 1.5e9, realized `−999_900_000` →
///   wallet `+500_100_000`, no bad debt.
/// * fund `1e9 − 100_100_000 = 899_900_000`.
#[test]
fn c6_the_sweep_on_a_short_whose_debt_exceeds_its_entry_proceeds_fills_at_one_unit() {
    let mut ctx = debt_short_fixture(0);
    seed_anchored(&mut ctx, WHALE, QTY, -ENTRY_VALUE, 300_000_000, 0, -13_000_000_000);
    let whale_before = position(&mut ctx, WHALE);
    let users = [ALICE, KEEPER, WHALE];
    let value_before = conservation_sum(&mut ctx, &users);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    oracle_tick(&mut ctx, ENTRY_PRICE, 31); // must not panic
    let logs = JournalTr::take_logs(ctx.journal_mut());

    assert_eq!(storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(), ENTRY_PRICE);
    let adl = decode_events::<Adl>(&logs);
    assert_eq!(adl.len(), 1);
    assert_eq!(
        (adl[0].liquidatedUser, adl[0].adlUser, adl[0].qty, adl[0].price),
        (ALICE, KEEPER, QTY as u64, 1),
        "no representable bankruptcy price → one price unit, against the CLEAN-tier KEEPER"
    );
    assert_eq!(
        position(&mut ctx, WHALE),
        whale_before,
        "the fallback-tier WHALE is never reached"
    );
    let (alice, keeper) = (position(&mut ctx, ALICE), position(&mut ctx, KEEPER));
    assert_eq!(alice.amount + keeper.amount, 0, "Σ amount conserved");
    assert_eq!((alice.amount, alice.v_quote_balance, alice.margin), (0, 0, 0));
    assert_eq!((keeper.amount, keeper.v_quote_balance, keeper.margin), (0, 0, 0));
    assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64, "loser wallet untouched");
    assert_eq!(signed_wallet(&mut ctx, KEEPER), 500_100_000);
    assert_eq!(keeper.last_funding_index, -13_000_000_000, "winner's funding settled");

    let changed = decode_events::<InsuranceFundChanged>(&logs);
    assert_eq!(changed.len(), 1);
    assert_eq!((changed[0].delta, changed[0].newBalance), (-100_100_000, 899_900_000));
    assert!(decode_events::<InsuranceFundDepleted>(&logs).is_empty());
    let funding: Vec<(Address, i64)> = decode_events::<FundingSettled>(&logs)
        .into_iter()
        .map(|f| (f.user, f.amount))
        .collect();
    assert_eq!(funding, vec![(ALICE, -1_300_000_000), (KEEPER, 1_300_000_000)]);
    assert_eq!(
        conservation_sum(&mut ctx, &users),
        value_before,
        "funding zero-sum, the shortfall paid by the fund"
    );
}

/// **C6, fallback tier — what the one-price-unit fill costs a LEVERAGED solvent winner.**
/// Pins CURRENT behaviour; read it before changing the price rule.
///
/// Same short, but KEEPER is anchored at the current index (no funding owed to it), so its
/// margin is the stored 200M. At the $100 mark it is SOLVENT and above maintenance (equity 200M
/// ≥ 166_666_666), but at price 1 its equity is `100_000 − 1e9 + 200M = −799_900_000`, so it
/// lands in the FALLBACK tier and fills under `BothSides`:
/// * ALICE's shortfall 100_100_000 and KEEPER's 799_900_000 both go to the fund: 900M;
/// * KEEPER is closed at $0.01 and loses its WHOLE 200M silo — equity it had at the mark;
/// * system value moves by exactly ALICE's 1.3e9 of funding, which here has no receiver.
///
/// A fill at the MARK instead would leave KEEPER neutral and bill the fund the loser's whole
/// `−1.1e9` equity at mark — 200M more for the fund, 200M less taken from KEEPER. Which of those
/// is right is a design call (`run_adl`'s fallback-tier note); this test only records the
/// numbers the current rule produces.
#[test]
fn c6_a_leveraged_solvent_winner_drafted_at_one_unit_loses_its_silo_to_the_fallback_tier() {
    let mut ctx = debt_short_fixture(-13_000_000_000);
    let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    oracle_tick(&mut ctx, ENTRY_PRICE, 31);
    let logs = JournalTr::take_logs(ctx.journal_mut());

    let adl = decode_events::<Adl>(&logs);
    assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (KEEPER, QTY as u64, 1));
    let (alice, keeper) = (position(&mut ctx, ALICE), position(&mut ctx, KEEPER));
    assert_eq!(alice.amount + keeper.amount, 0, "Σ amount conserved");
    assert_eq!((keeper.amount, keeper.margin), (0, 0));
    assert_eq!(signed_wallet(&mut ctx, KEEPER), 0, "the 200M silo is gone");
    let changed = decode_events::<InsuranceFundChanged>(&logs);
    assert_eq!(changed.len(), 1);
    assert_eq!(
        (changed[0].delta, changed[0].newBalance),
        (-900_000_000, 100_000_000),
        "loser 100_100_000 + winner 799_900_000"
    );
    assert_eq!(
        conservation_sum(&mut ctx, &[ALICE, KEEPER]) - value_before,
        -1_300_000_000,
        "value moves by exactly the unreceived funding"
    );
}

/// **C6, the long mirror — an insolvent debt-laden LONG ADLs at a bankruptcy price ABOVE its
/// entry, and the side that RECEIVED the unpaid funding is clawed back; the fund is not
/// touched.**
///
/// ALICE long 10 @ $100 and KEEPER short 10 @ $100, 200M silos, both anchored at 0; index
/// +2.5e9, so ALICE owes 250M and KEEPER is owed 250M. At an unchanged $100 mark ALICE's settled
/// margin is −50M, equity −50M: insolvent with no price move at all. `P_b =
/// ⌈1.05e9 / 1e5⌉ = 10_500` — $105, ABOVE her entry. KEEPER's effective margin is 450M, equity
/// at `P_b` `−1.05e9 + 1e9 + 450M = 400M` → clean tier, `Forbid`.
///
/// Fill 10 @ $105: ALICE credit `−50M + 50M = 0` (wallet unchanged, no bad debt); KEEPER is
/// settled +250M then realizes −50M: wallet `450M − 50M = 400M`. It keeps 200M of the 250M it was
/// owed — the 50M ALICE could not pay comes back out of the side that was paid it. Value is
/// conserved (450M) and the fund never moves.
#[test]
fn c6_an_insolvent_debt_laden_long_adls_above_entry_and_the_receiver_is_clawed_back() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 5, 0);
    set_funding_index(&mut ctx, INDEX_250M);
    assert_eq!(
        calc_bankruptcy_price(QTY, -ENTRY_VALUE, -50_000_000, 0, PRICE_DECIMALS).unwrap(),
        10_500
    );
    let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
    assert_eq!(value_before, 450_000_000);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    oracle_tick(&mut ctx, ENTRY_PRICE, 31); // the mark does not move — funding alone
    let logs = JournalTr::take_logs(ctx.journal_mut());

    let adl = decode_events::<Adl>(&logs);
    assert_eq!(adl.len(), 1);
    assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (KEEPER, QTY as u64, 10_500));
    let paid: Vec<(Address, i64)> = decode_events::<FundingSettled>(&logs)
        .into_iter()
        .map(|f| (f.user, f.amount))
        .collect();
    assert_eq!(paid, vec![(ALICE, -250_000_000), (KEEPER, 250_000_000)]);
    let (alice, keeper) = (position(&mut ctx, ALICE), position(&mut ctx, KEEPER));
    assert_eq!((alice.amount, alice.margin, keeper.amount, keeper.margin), (0, 0, 0, 0));
    assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64, "credit exactly 0");
    assert_eq!(signed_wallet(&mut ctx, KEEPER), 400_000_000);
    assert!(decode_events::<InsuranceFundChanged>(&logs).is_empty(), "scheme X: no fund");
    assert_eq!(conservation_sum(&mut ctx, &[ALICE, KEEPER]), value_before);
}

/// **C6 — a debt short's residual goes to the CLEAN winner under `LoserOnly`, not to a
/// fallback-tier holder ahead of it.**
///
/// `debt_short_fixture(0)` (ALICE short, credit −100M, no representable `P_b`; KEEPER a long
/// OWED 1.3e9 → clean at price 1) plus BOB2, a long 10 @ $110 with an empty silo and nothing
/// pending: equity at the $100 mark −100M → fallback tier. `p_b == 0` selects `LoserOnly`, so
/// KEEPER's trial accepts the loser's shortfall and BOB2 is never reached; the fund pays only
/// ALICE's 100_100_000.
///
/// This is the ENGINE-level witness for the `p_b == 0` → `LoserOnly` branch (the fixture's own
/// `calc_bankruptcy_price == 0` pin is a constant). If the debt short got any price ≥ 1
/// instead, the policy would stay `Forbid`: KEEPER's trial would be rejected for the LOSER's
/// bad debt and re-queued behind BOB2, who would be closed under `BothSides` and bill the fund
/// BOB2's own ~1.1e9 on top. Manual `liquidate`, not the sweep, so BOB2 is not itself swept.
#[test]
fn c6_a_debt_short_adls_against_the_clean_winner_under_loser_only() {
    const BOB2: Address = address!("b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2");
    let mut ctx = debt_short_fixture(0);
    seed_anchored(&mut ctx, BOB2, QTY, -1_100_000_000, 0, 0, -13_000_000_000);
    let bob2_before = position(&mut ctx, BOB2);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    liquidate(&mut ctx, ALICE).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());

    let adl = decode_events::<Adl>(&logs);
    assert_eq!(adl.len(), 1);
    assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (KEEPER, QTY as u64, 1));
    assert_eq!(position(&mut ctx, BOB2), bob2_before, "the fallback holder is never reached");
    let changed = decode_events::<InsuranceFundChanged>(&logs);
    assert_eq!(changed.len(), 1);
    assert_eq!((changed[0].delta, changed[0].newBalance), (-100_100_000, 899_900_000));
}

// ── C7: ADL counterparties carrying UNSETTLED funding ──────────────────────────────────────

/// **C7 — an ADL winner's pending funding is settled at its fill, both directions.**
///
/// The audit probe: ALICE long 10 @ $100 and KEEPER short 10 @ $100, both 200M silos, both
/// anchored at 0; the index moves ±75M (a 7.5M transfer between them); the index crashes to $75.
/// ALICE is insolvent and her whole position ADLs against KEEPER at her bankruptcy price.
///
/// | index | ALICE margin | `P_b` | KEEPER effective margin | KEEPER wallet |
/// |-------|--------------|-------|-------------------------|---------------|
/// | +75M  | 192.5M       | 8_075 | 207.5M (owed +7.5M)     | 400M          |
/// | −75M  | 207.5M       | 7_925 | 192.5M (owes −7.5M)     | 400M          |
///
/// KEEPER's wallet is `effective margin + (1e9 − value(P_b, 10))` = 400M either way. Before the
/// fix KEEPER's funding was never settled — the full close re-anchored a flat position with no
/// payment — so it got 392.5M (7.5M destroyed) or 407.5M (7.5M minted). Now its index is
/// re-anchored, its `FundingSettled` is published ahead of the `Adl`, and total value is
/// conserved exactly (450M).
#[test]
fn c7_an_adl_winners_pending_funding_is_settled_at_the_fill_in_both_directions() {
    for (index, alice_margin, p_b, keeper_payment) in [
        (75_000_000i128, 192_500_000i64, 8_075u64, 7_500_000i64),
        (-75_000_000, 207_500_000, 7_925, -7_500_000),
    ] {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
        seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 5, 0);
        set_funding_index(&mut ctx, index);
        assert_eq!(
            calc_bankruptcy_price(QTY, -ENTRY_VALUE, alice_margin, 0, PRICE_DECIMALS).unwrap(),
            p_b
        );
        let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
        assert_eq!(value_before, 450_000_000);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        oracle_tick(&mut ctx, 7_500, 31);
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let adl = decode_events::<Adl>(&logs);
        assert_eq!(adl.len(), 1, "index {index}");
        assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (KEEPER, QTY as u64, p_b));
        let keeper = position(&mut ctx, KEEPER);
        assert_eq!((keeper.amount, keeper.margin), (0, 0), "index {index}");
        assert_eq!(keeper.last_funding_index, index, "index {index}: winner re-anchored");
        assert_eq!(signed_wallet(&mut ctx, KEEPER), 400_000_000, "index {index}");
        assert_eq!(position(&mut ctx, ALICE).amount, 0);
        assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64, "index {index}");

        let shape = stream_shape(&logs);
        let keeper_funding = shape
            .iter()
            .position(|e| *e == ("FundingSettled", Some(KEEPER)))
            .expect("the winner's funding is published");
        let adl_at = shape.iter().position(|(n, _)| *n == "Adl").unwrap();
        assert!(keeper_funding < adl_at, "index {index}: funding → ADL, in that order");
        let paid: Vec<(Address, i64)> = decode_events::<FundingSettled>(&logs)
            .into_iter()
            .map(|f| (f.user, f.amount))
            .collect();
        assert_eq!(paid, vec![(ALICE, -keeper_payment), (KEEPER, keeper_payment)]);
        assert_eq!(
            conservation_sum(&mut ctx, &[ALICE, KEEPER]),
            value_before,
            "index {index}: no funding destroyed or minted by the ADL"
        );
        assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0);
    }
}

/// **C7 — a counterparty whose STORED equity is positive but whose EFFECTIVE equity (pending
/// debt included) is ≤ 0 is FALLBACK tier, not clean tier.**
///
/// ALICE long 10 @ $100 (200M) at a $75 mark: equity −50M, `P_b = 8_000`. No clean
/// counterparty exists; two shorts, both fallback:
/// * WHALE @ $100, silo 100M, anchored at 0 under index −3.6e9, so it OWES `3.6e9 / 10 = 360M`:
///   effective margin −260M, effective equity at mark `−750M + 1e9 − 260M = −10M`. On its STORED
///   margin it reads +350M at mark and +300M at `P_b` — clean.
/// * BOB2 @ $50 (vQuote 500M), silo 100M, nothing pending: equity at mark −150M.
///
/// The fallback tier fills HEALTHIEST first, so on effective equity WHALE (−10M) goes before
/// BOB2 (−150M): it is closed at $80 under `BothSides`, its funding settled first (margin −260M),
/// credit `−260M + 1e9 − 800M = −60M` → 60M of winner bad debt to the fund; BOB2 is never
/// reached. Judged on STORED margin WHALE would instead be tried as CLEAN, fail the bad-debt-free
/// trial, and be re-queued BEHIND BOB2 — who would then be the one closed (and bill the fund
/// 200M). System value moves by exactly WHALE's 360M of unreceived funding.
#[test]
fn c7_a_counterparty_solvent_only_on_stored_margin_is_fallback_not_clean_tier() {
    const INDEX: i128 = -3_600_000_000;
    const BOB2: Address = address!("b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2");
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 1_000_000_000).unwrap();
    seed_anchored(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, USER_WALLET as i64, INDEX);
    seed_anchored(&mut ctx, WHALE, -QTY, ENTRY_VALUE, 100_000_000, 0, 0);
    seed_anchored(&mut ctx, BOB2, -QTY, 500_000_000, 100_000_000, 0, INDEX);
    set_funding_index(&mut ctx, INDEX);
    storage::save_mark_price(&mut ctx, MARKET_ID, 7_500).unwrap();
    let users = [ALICE, WHALE, BOB2];
    let value_before = conservation_sum(&mut ctx, &users);
    let bob2_before = position(&mut ctx, BOB2);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    liquidate(&mut ctx, ALICE).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());

    let adl = decode_events::<Adl>(&logs);
    assert_eq!(adl.len(), 1);
    assert_eq!(
        (adl[0].adlUser, adl[0].qty, adl[0].price),
        (WHALE, QTY as u64, 8_000),
        "WHALE is fallback tier on its EFFECTIVE equity, and the healthiest of it"
    );
    assert_eq!(position(&mut ctx, BOB2), bob2_before, "BOB2 never reached");
    let whale = position(&mut ctx, WHALE);
    assert_eq!((whale.amount, whale.margin, whale.last_funding_index), (0, 0, INDEX));
    assert_eq!(signed_wallet(&mut ctx, WHALE), 0);
    assert_eq!((position(&mut ctx, ALICE).amount, signed_wallet(&mut ctx, ALICE)), (0, 50_000_000));
    let changed = decode_events::<InsuranceFundChanged>(&logs);
    assert_eq!(changed.len(), 1);
    assert_eq!((changed[0].delta, changed[0].newBalance), (-60_000_000, 940_000_000));
    assert_eq!(conservation_sum(&mut ctx, &users) - value_before, -360_000_000);
}

/// **C7 — clean-tier counterparties are RANKED on effective equity.**
///
/// ALICE as above (`P_b = 8_000` at a $75 mark), index +1.5e9 (rising: shorts are OWED). Two
/// clean shorts @ $100, uPnL 250M each:
/// * KEEPER, silo 200M, nothing pending: equity 450M, ROE `250 / 450`;
/// * WHALE, silo 100M, anchored at 0 → owed 150M: effective equity 500M, ROE `250 / 500` — but
///   on STORED margin 350M, ROE `250 / 350`, the highest.
///
/// Ranked on effective equity, KEEPER goes first and absorbs all 10 (wallet `200M + 200M`);
/// WHALE is untouched, anchor and all. The control (WHALE anchored too, nothing pending, so
/// stored == effective) proves the fixture discriminates: WHALE goes first, wallet
/// `100M + 200M`, and KEEPER is untouched.
#[test]
fn c7_clean_tier_counterparties_are_ranked_on_effective_equity() {
    const INDEX: i128 = 1_500_000_000;
    for (whale_anchor, first, wallet) in
        [(0i128, KEEPER, 400_000_000i64), (INDEX, WHALE, 300_000_000)]
    {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        seed_anchored(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, USER_WALLET as i64, INDEX);
        seed_anchored(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 0, INDEX);
        seed_anchored(&mut ctx, WHALE, -QTY, ENTRY_VALUE, 100_000_000, 0, whale_anchor);
        set_funding_index(&mut ctx, INDEX);
        storage::save_mark_price(&mut ctx, MARKET_ID, 7_500).unwrap();
        let other = if first == KEEPER { WHALE } else { KEEPER };
        let other_before = position(&mut ctx, other);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        liquidate(&mut ctx, ALICE).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let adl = decode_events::<Adl>(&logs);
        assert_eq!(adl.len(), 1, "WHALE anchor {whale_anchor}: one counterparty takes all");
        assert_eq!((adl[0].adlUser, adl[0].price), (first, 8_000), "anchor {whale_anchor}");
        assert_eq!(position(&mut ctx, first).amount, 0);
        assert_eq!(signed_wallet(&mut ctx, first), wallet, "anchor {whale_anchor}");
        assert_eq!(position(&mut ctx, other), other_before, "anchor {whale_anchor}");
        assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0, "Forbid: no fund");
    }
}

/// **C7 — a PARTIALLY filled clean winner carrying a funding DEBT: the remainder keeps its
/// trunc0 share of (stored + pending) and is re-anchored, so nothing is settled twice.**
///
/// ALICE long 10 @ $100 (200M, anchored) at a $75 mark: equity −50M, `P_b = 8_000`. Index
/// −1.5e9 (falling: shorts PAY). WHALE short 20 @ $100 (vQuote 2e9), silo 100M, anchored at 0,
/// so it owes `20 · 1.5e9 / 100 = 300M`: effective margin −200M — a DEBT — yet effective equity
/// at mark `−1.5e9 + 2e9 − 200M = 300M` and at `P_b` `−1.6e9 + 2e9 − 200M = 200M ≥ 0` → CLEAN
/// tier (ROE `500M / 300M > 1`). It takes 10 of its 20 at $80 (value 800M):
/// * WHALE: funding settled (margin −200M), then the half close releases
///   `−200M − trunc0(−200M · 10/20) = −100M`, realizes `1e9 − 800M = 200M` → credit +100M to the
///   wallet; the remainder is `(−10, 1e9, −100M)` anchored at the index — half the debt stays
///   with half the position, and the signed release ran on the WINNER leg;
/// * ALICE: realized `800M − 1e9 = −200M`, release 200M → credit 0, wallet unchanged, no bad
///   debt, the fund untouched (`Forbid`).
///
/// Afterwards WHALE has nothing pending (`compute_funding_settlement` → `None`), so a later
/// settle cannot charge the 300M again. System value moves by exactly WHALE's unreceived 300M.
/// WHALE, not KEEPER: `liquidate()` calls as KEEPER.
#[test]
fn c7_a_partially_filled_clean_winner_in_funding_debt_keeps_its_share_and_is_re_anchored() {
    const INDEX: i128 = -1_500_000_000;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_anchored(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, USER_WALLET as i64, INDEX);
    seed_anchored(&mut ctx, WHALE, -2 * QTY, 2 * ENTRY_VALUE, 100_000_000, 0, 0);
    set_funding_index(&mut ctx, INDEX);
    storage::save_mark_price(&mut ctx, MARKET_ID, 7_500).unwrap();
    assert_eq!(
        calc_funding_payment(-2 * QTY, INDEX, 0, PRICE_DECIMALS).unwrap(),
        -300_000_000
    );
    assert_eq!(
        calc_bankruptcy_price(QTY, -ENTRY_VALUE, MARGIN, 0, PRICE_DECIMALS).unwrap(),
        8_000
    );
    let users = [ALICE, WHALE, KEEPER];
    let value_before = conservation_sum(&mut ctx, &users);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    liquidate(&mut ctx, ALICE).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());

    let adl = decode_events::<Adl>(&logs);
    assert_eq!(adl.len(), 1);
    assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (WHALE, QTY as u64, 8_000));
    let paid: Vec<(Address, i64)> = decode_events::<FundingSettled>(&logs)
        .into_iter()
        .map(|f| (f.user, f.amount))
        .collect();
    assert_eq!(paid, vec![(WHALE, -300_000_000)], "the winner's whole pending debt, once");
    let whale = position(&mut ctx, WHALE);
    assert_eq!(
        (whale.amount, whale.v_quote_balance, whale.margin, whale.last_funding_index),
        (-QTY, ENTRY_VALUE, -100_000_000, INDEX),
        "the remainder carries half of (stored + pending), re-anchored"
    );
    assert_eq!(signed_wallet(&mut ctx, WHALE), 100_000_000);
    assert_eq!(assert_sigma_margin_consistent(&mut ctx, WHALE), -100_000_000);
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64);
    assert!(decode_events::<InsuranceFundChanged>(&logs).is_empty(), "Forbid: no fund");
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0);
    assert_eq!(conservation_sum(&mut ctx, &users) - value_before, -300_000_000);

    // Nothing left to settle: the 300M is not charged a second time.
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    let mut pos = whale.clone();
    let pending = crate::risk::funding::compute_funding_settlement(
        &mut ctx, WHALE, &market, &mut pos, None,
    )
    .unwrap();
    assert!(pending.is_none());
    assert_eq!(pos, whale);
}

/// Short ALICE 1 @ $100 (vQuote 1e8, silo 2e7, anchored — nothing pending): `P_b = $120`, and at
/// a $130 mark she is insolvent. KEEPER is a long 3 whose EFFECTIVE margin is −1 (stored 2, owes
/// 3 at index 100) and whose vQuote is `−(3·value($120,1) − 2)`: equity at `P_b` is +1, at mark
/// +30_000_001 → CLEAN tier. But a 1-of-3 slice under trunc0 carries `−ceil(1/3) = −1` of margin
/// and `−ceil(359_999_998/3) = −120M` of vQuote against +120M of proceeds: credit −1 with nothing
/// left to draw → winner bad debt 1 → the `Forbid` trial is REJECTED and KEEPER re-queued as
/// fallback. (That rejection is a trunc0 artifact — see `run_adl`'s re-queue note.)
fn rejected_trial_fixture(ctx: &mut TestCtx) {
    const INDEX: i128 = 100;
    setup_market(ctx);
    storage::save_insurance_fund(ctx, 1_000_000_000).unwrap();
    seed_anchored(ctx, ALICE, -1, 100_000_000, 20_000_000, USER_WALLET as i64, INDEX);
    seed_anchored(ctx, KEEPER, 3, -359_999_998, 2, 0, 0);
    set_funding_index(ctx, INDEX);
    storage::save_mark_price(ctx, MARKET_ID, 13_000).unwrap();
    assert_eq!(calc_funding_payment(3, INDEX, 0, PRICE_DECIMALS).unwrap(), -3);
    assert_eq!(
        calc_bankruptcy_price(-1, 100_000_000, 20_000_000, 0, PRICE_DECIMALS).unwrap(),
        12_000
    );
}

/// **C7 — a REJECTED ADL trial publishes no funding for its winner; the fallback retry
/// settles it ONCE.**
///
/// KEEPER's clean-tier trial is rejected (`rejected_trial_fixture`), it is re-queued as fallback
/// and filled under `BothSides` (winner bad debt 1). Its funding must be published once, by the
/// accepted fill. Publishing it at the rejected trial would emit `FundingSettled(KEEPER, −3)`
/// twice — the retry reloads the stored, un-anchored position — and an indexer summing
/// `FundingSettled.amount` would charge KEEPER 6 for a 3 settlement.
#[test]
fn c7_a_rejected_trial_publishes_no_winner_funding_and_the_retry_settles_it_once() {
    let mut ctx = make_ctx();
    rejected_trial_fixture(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    liquidate(&mut ctx, ALICE).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());

    let adl = decode_events::<Adl>(&logs);
    assert_eq!(adl.len(), 1);
    assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (KEEPER, 1, 12_000));
    let keeper_funding: Vec<i64> = decode_events::<FundingSettled>(&logs)
        .into_iter()
        .filter(|f| f.user == KEEPER)
        .map(|f| f.amount)
        .collect();
    assert_eq!(keeper_funding, vec![-3], "settled ONCE — the rejected trial published nothing");
    let keeper = position(&mut ctx, KEEPER);
    assert_eq!(
        (keeper.amount, keeper.v_quote_balance, keeper.margin, keeper.last_funding_index),
        (2, -239_999_998, 0, 100)
    );
    let changed = decode_events::<InsuranceFundChanged>(&logs);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].delta, -1, "the winner's 1-unit trunc0 dust, under BothSides");
}

/// **C7 — a REJECTED ADL trial that is never retried publishes no ghost funding.**
///
/// `rejected_trial_fixture` plus BOB2, a long 1 @ $140 (silo 0, anchored) — bankrupt at the
/// mark, so FALLBACK tier, queued ahead of the re-queued KEEPER. BOB2 absorbs the whole residual
/// under `BothSides`; KEEPER is never retried and its stored position is untouched. The stream
/// must carry no `FundingSettled` and no `PositionChanged` for KEEPER: publishing at the trial
/// would show a settlement (and a row with margin −1) for a position whose stored margin is
/// still 2 and whose anchor never moved.
#[test]
fn c7_a_rejected_trial_never_retried_publishes_no_ghost_funding() {
    const BOB2: Address = address!("b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2b0b2");
    let mut ctx = make_ctx();
    rejected_trial_fixture(&mut ctx);
    seed_anchored(&mut ctx, BOB2, 1, -140_000_000, 0, 0, 100);
    let keeper_before = position(&mut ctx, KEEPER);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    liquidate(&mut ctx, ALICE).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());

    let adl = decode_events::<Adl>(&logs);
    assert_eq!(adl.len(), 1);
    assert_eq!((adl[0].adlUser, adl[0].qty, adl[0].price), (BOB2, 1, 12_000));
    assert_eq!(position(&mut ctx, KEEPER), keeper_before, "KEEPER never filled");
    assert!(
        decode_events::<FundingSettled>(&logs).iter().all(|f| f.user != KEEPER),
        "no funding event for a winner whose trial was rejected and never retried"
    );
    assert!(
        decode_events::<PositionChanged>(&logs).iter().all(|r| r.user != KEEPER),
        "and no position row for it either"
    );
}

// ── C8: margin add / remove on a debt ────────────────────────────────────────────────────

/// **C8 — `addPositionMargin` repays a funding debt and can lift the position back above
/// maintenance; the boundary is exact.**
///
/// Long 10 @ $100 at a $100 mark, silo 200M, a 250M charge pending: once settled, margin −50M,
/// equity −50M against `MM = 166_666_666`. Adding `X` gives equity `X − 50M`, so the smallest
/// add that clears maintenance is `216_666_666` (equity == MM, the predicate is `>=`). One less
/// leaves it liquidatable. The add settles the funding first and both land: margin `X − 50M`,
/// wallet `−X`, the index re-anchored, and the stream is the funding group (row margin −50M)
/// then the margin-transfer group.
#[test]
fn c8_add_position_margin_repays_the_debt_and_lifts_the_position_at_exactly_mm() {
    const X: u64 = 216_666_666;
    let market_tiers = MarginTiers::default();
    for (add, above) in [(X - 1, false), (X, true)] {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        set_wallet(&mut ctx, ALICE, 500_000_000);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        set_funding_index(&mut ctx, INDEX_250M);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        add_position_margin(&mut ctx, add).unwrap();
        let logs = JournalTr::take_logs(ctx.journal_mut());

        let pos = position(&mut ctx, ALICE);
        assert_eq!(pos.margin, add as i64 - 50_000_000, "add {add}");
        assert_eq!(pos.last_funding_index, INDEX_250M);
        assert_eq!(signed_wallet(&mut ctx, ALICE), 500_000_000 - add as i64);
        assert_eq!(assert_sigma_margin_consistent(&mut ctx, ALICE), pos.margin);
        assert_eq!(
            is_above_maintenance_margin(
                &market_tiers,
                ENTRY_PRICE,
                pos.amount,
                pos.v_quote_balance,
                pos.margin,
                0,
                PRICE_DECIMALS
            )
            .unwrap(),
            above,
            "add {add}: equity {} vs MM {MM_100}",
            pos.margin
        );
        assert_eq!(
            stream_shape(&logs)
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>(),
            vec![
                "FundingSettled",
                "AccountBalanceChanged",
                "PositionChanged",
                "PositionMarginAdjusted",
                "AccountBalanceChanged",
                "PositionChanged",
            ]
        );
        let rows: Vec<i64> =
            decode_events::<PositionChanged>(&logs).into_iter().map(|r| r.margin).collect();
        assert_eq!(rows, vec![-50_000_000, pos.margin], "the debt, then the repaid silo");
        let adj = decode_events::<PositionMarginAdjusted>(&logs);
        assert_eq!((adj[0].delta, adj[0].margin), (add as i64, pos.margin));
        if above {
            let err = liquidate(&mut ctx, ALICE).unwrap_err();
            assert!(err.to_string().contains("above maintenance margin"), "{err}");
        }
    }
}

/// **C8 — `removePositionMargin` on a STORED debt is refused by G1, and writes nothing.**
/// Margin −50M persisted, no funding pending: `pos.margin < amount` for every `amount ≥ 1`.
#[test]
fn c8_remove_position_margin_on_a_negative_margin_is_refused_by_g1() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_mark_price(&mut ctx, MARKET_ID, MARK_130).unwrap();
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    set_funding_index(&mut ctx, INDEX_250M);
    assert_eq!(persist_alice_funding(&mut ctx).margin, -50_000_000);
    // Healthy at $130 (equity 250M ≥ MM 216_666_666) — G1 refuses on the BALANCE alone.
    let before = alice_untouched_snapshot(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let err = remove_position_margin(&mut ctx, 1).unwrap_err();
    assert!(
        matches!(&err, PerpError::Reject(m)
            if m == crate::risk::REMOVE_MARGIN_G1_REJECT),
        "{err}"
    );
    assert_eq!(alice_untouched_snapshot(&mut ctx), before);
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
}

// ── C9: K9 on a debt-laden position ──────────────────────────────────────────────────────

/// **C9 — K9 refuses a TAKER open that leaves a debt-laden position under maintenance; it
/// admits one that does not, and the new leg's margin nets against the debt.**
///
/// Long 10 @ $100 at leverage 3, margin −50M stored. ALICE buys 1 more:
/// * at $100: IM `⌊1e8 / 3⌋ = 33_333_333` → margin −16_666_667, equity −16_666_667 against
///   `MM = 1.1e9 / 6 = 183_333_333` → "open would breach maintenance margin", nothing written,
///   MAKER's ask untouched;
/// * at $130 (control): IM 43_333_333 → margin −6_666_667 — STILL a debt — but equity
///   `1.43e9 − 1.13e9 − 6_666_667 = 293_333_333 ≥ 238_333_333` → admitted.
#[test]
fn c9_k9_refuses_a_taker_open_on_a_debt_laden_position_under_maintenance() {
    for (price, admitted) in [(ENTRY_PRICE, false), (MARK_130, true)] {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position_with_leverage(&mut ctx, QTY, -ENTRY_VALUE, 3);
        set_funding_index(&mut ctx, INDEX_250M);
        assert_eq!(persist_alice_funding(&mut ctx).margin, -50_000_000);
        storage::save_mark_price(&mut ctx, MARKET_ID, price).unwrap();
        place_maker_order(&mut ctx, SELL, price, 1);
        let before = alice_untouched_snapshot(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let res = try_place_order(&mut ctx, ALICE, BUY, price, 1);
        if !admitted {
            let err = res.unwrap_err();
            assert!(
                matches!(&err, PerpError::Reject(m)
                    if m == "placeOrder: open would breach maintenance margin"),
                "{err}"
            );
            assert_eq!(alice_untouched_snapshot(&mut ctx), before, "a K9 refusal writes nothing");
            assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
            assert_eq!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(), vec![price]);
        } else {
            res.unwrap();
            let pos = position(&mut ctx, ALICE);
            assert_eq!(
                (pos.amount, pos.v_quote_balance, pos.margin),
                (QTY + 1, -1_130_000_000, -50_000_000 + 43_333_333),
                "admitted, and the new IM netted against the debt"
            );
            assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64 - 43_333_333);
        }
    }
}

/// **C9 — a MAKER fill that would OPEN on a debt-laden position below maintenance is
/// `RejectedInsolvent`: the order is cancelled (`MakerInsolvent`), the fill does not land, and
/// the only state change is the maker's funding settle.**
///
/// ALICE (long 10 @ $100, leverage 3, silo 200M) rests a bid for 1 @ $100 while nothing is
/// pending. Then a 250M charge accrues and MAKER sells into the bid: the match settles ALICE's
/// funding (−50M), the trial fill would leave margin `−50M + 33_333_333` and equity −16_666_667
/// against MM 183_333_333, so K9 rejects. MAKER's sell rests instead.
#[test]
fn c9_k9_cancels_a_maker_open_on_a_debt_laden_position() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position_with_leverage(&mut ctx, QTY, -ENTRY_VALUE, 3);
    place_order(&mut ctx, ALICE, BUY, ENTRY_PRICE, 1);
    assert_eq!(position(&mut ctx, ALICE).total_buy_qty, 1, "fixture: the bid rests");
    set_funding_index(&mut ctx, INDEX_250M);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    try_place_order(&mut ctx, MAKER, SELL, ENTRY_PRICE, 1).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());

    assert!(decode_events::<Trade>(&logs).is_empty(), "the fill did not land");
    let cancelled = decode_events::<OrderCancelled>(&logs);
    assert_eq!(cancelled.len(), 1);
    assert_eq!(
        (cancelled[0].user, cancelled[0].reason),
        (ALICE, CancelReason::MakerInsolvent as u8)
    );
    let alice = position(&mut ctx, ALICE);
    assert_eq!(
        (alice.amount, alice.v_quote_balance, alice.margin, alice.last_funding_index),
        (QTY, -ENTRY_VALUE, -50_000_000, INDEX_250M),
        "only the funding settle landed"
    );
    assert_eq!(alice.total_buy_qty, 0, "the bid is gone");
    assert_eq!(signed_wallet(&mut ctx, ALICE), USER_WALLET as i64);
    let funding = decode_events::<FundingSettled>(&logs);
    assert_eq!((funding[0].user, funding[0].amount), (ALICE, -250_000_000));
    assert_eq!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(), vec![ENTRY_PRICE]);
    assert_eq!(position(&mut ctx, MAKER).amount, 0);
}
