//! An ISOLATED ADL fill is held at `maxPrice` (owner decision 2026-11-19, D2 — the business half of
//! the second-round review's band-iso-adl-4): `risk::adl::isolated_adl_price` fills at
//! `min(its price, maxPrice)`, where its price is the mark for a residual solvent there and the
//! bankruptcy price `p_b` for an insolvent one. Before, an insolvent long whose `p_b` sat above a
//! lowered `maxPrice` made its winner pay `p_b`; now the winner pays `maxPrice` and the gap is the
//! loser's shortfall, which the insurance fund absorbs like any isolated bankruptcy deficit.
//!
//! Every position here is opened, and every state change made, by public calls: ALICE (2x) and
//! KEEPER (2x) trade 10 @ $100 against each other, so each one's silo is $500 and KEEPER is ALICE's
//! only ADL counterparty; the fund is deposited by ADMIN. The trading fee and the clearance fee are 0
//! in this market.

use super::*;
use crate::interface::IPerpDex::{
    depositInsuranceFundCall, updateMarketCall, Adl, InsuranceFundChanged, InsuranceFundDepleted,
    Liquidation,
};
use crate::types::TimeInForce;

/// $1,000 in quote units: each trader's wallet before it opens.
const OPENING_WALLET: i64 = 1_000_000_000;
/// The insurance fund ADMIN deposits.
const FUND: u64 = 1_000_000_000;

fn update_market(ctx: &mut TestCtx, max_price: u64) {
    crate::risk::run_update_market(
        &updateMarketCall {
            marketId: MARKET_ID,
            tickSize: 1,
            stepSize: 1,
            minQuantity: 1,
            maxQuantity: 1_000_000,
            maxPrice: max_price,
            priceUpdateInterval: 15,
            active: true,
            fundingInterval: 0,
            interestRate: 0,
            liquidationFeeRateBps: 0,
            priceBandBps: 0,
        }
        .abi_encode(),
        ADMIN,
        ctx,
    )
    .expect("a maxPrice lowering is always accepted");
}

fn place(ctx: &mut TestCtx, user: Address, side: Side, qty: u64, tif: TimeInForce) {
    run_place_order(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: side as u8,
            price: ENTRY_PRICE,
            quantity: qty,
            orderType: 0,
            tif: tif as u8,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        }
        .abi_encode(),
        user,
        ctx,
    )
    .unwrap();
}

/// ALICE ISOLATED `alice_side` 10 @ $100 at 2x against KEEPER (2x, the other side), a fund of `fund`
/// deposited by ADMIN. Each silo is $500 (`MARGIN`-free arithmetic: `calc_value($100, 10) / 2`),
/// each wallet $500 after it, and the book is empty again.
fn opened(alice_side: Side, fund: u64) -> TestCtx {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    for (user, wallet) in [(ALICE, OPENING_WALLET), (KEEPER, OPENING_WALLET), (ADMIN, fund as i64)] {
        storage::save_account(
            &mut ctx,
            user,
            UserAccount { perp_wallet_balance: wallet, ..UserAccount::default() },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
    }
    run_deposit_insurance_fund(&depositInsuranceFundCall { amount: fund }.abi_encode(), ADMIN, &mut ctx)
        .unwrap();
    for user in [ALICE, KEEPER] {
        crate::risk::run_set_leverage(
            &setLeverageCall { marketId: MARKET_ID, leverage: 2 }.abi_encode(),
            user,
            &mut ctx,
        )
        .unwrap();
    }
    let keeper_side = if alice_side == Side::Buy { Side::Sell } else { Side::Buy };
    place(&mut ctx, KEEPER, keeper_side, QTY as u64, TimeInForce::Gtc);
    place(&mut ctx, ALICE, alice_side, QTY as u64, TimeInForce::Ioc);
    let sign = if alice_side == Side::Buy { 1 } else { -1 };
    for (user, s) in [(ALICE, sign), (KEEPER, -sign)] {
        let p = position(&mut ctx, user);
        assert_eq!((p.amount, p.v_quote_balance, p.margin), (s * QTY, -s * ENTRY_VALUE, 500_000_000));
        assert_eq!(wallet(&mut ctx, user), 500_000_000);
    }
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), fund);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    ctx
}

/// The oracle tick the sweep runs on: `index` at `ts = 31` (the mark follows it, held at
/// `maxPrice`).
fn tick(ctx: &mut TestCtx, index: u64) {
    advance_block_to(ctx, 31);
    run_update_index_price(
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: 31 }.abi_encode(),
        ADMIN,
        ctx,
    )
    .unwrap();
}

/// What one liquidation left behind, read off its logs and the store.
#[derive(Debug, PartialEq)]
struct Outcome {
    /// `(liquidatedUser, adlUser, qty, price)` of every `Adl`.
    adls: Vec<(Address, Address, u64, u64)>,
    /// `delta` of every `InsuranceFundChanged`, and `badDebt` of every `InsuranceFundDepleted`.
    fund_moves: Vec<i64>,
    depleted: Vec<u64>,
    /// The two traders' wallets and positions, and the fund.
    alice_wallet: u64,
    keeper_wallet: u64,
    alice_amount: i64,
    keeper_amount: i64,
    fund: u64,
    /// `realizedPnl` of the two ADL legs' `PositionChanged` rows, loser first.
    realized: Vec<(Address, i64)>,
}

fn outcome(ctx: &mut TestCtx) -> Outcome {
    let logs = JournalTr::take_logs(ctx.journal_mut());
    let liquidations = decode_events::<Liquidation>(&logs);
    assert_eq!(liquidations.len(), 1, "one liquidation pass");
    assert_eq!((liquidations[0].user, liquidations[0].reward), (ALICE, 0));
    Outcome {
        adls: decode_events::<Adl>(&logs)
            .iter()
            .map(|a| (a.liquidatedUser, a.adlUser, a.qty, a.price))
            .collect(),
        fund_moves: decode_events::<InsuranceFundChanged>(&logs).iter().map(|e| e.delta).collect(),
        depleted: decode_events::<InsuranceFundDepleted>(&logs).iter().map(|e| e.badDebt).collect(),
        alice_wallet: wallet(ctx, ALICE),
        keeper_wallet: wallet(ctx, KEEPER),
        alice_amount: position(ctx, ALICE).amount,
        keeper_amount: position(ctx, KEEPER).amount,
        fund: storage::load_insurance_fund(ctx).unwrap(),
        realized: decode_events::<crate::interface::IPerpDex::PositionChanged>(&logs)
            .iter()
            .filter(|p| p.closedQuantity > 0)
            .map(|p| (p.user, p.realizedPnl))
            .collect(),
    }
}

/// **(a) An insolvent long whose bankruptcy price sits above a lowered `maxPrice` is ADL'd AT
/// `maxPrice`, and the fund takes the gap.** ALICE long 10 @ $100 on a $500 silo: `p_b` = $50.
/// `updateMarket` lowers `maxPrice` to $40, and the tick to $35 (inside it) liquidates her: equity
/// −$150, empty book, so all 10 go to ADL against KEEPER.
///
/// * The fill is at $40 = `maxPrice`, not $50: KEEPER buys back at $40 and realizes +$600 (at $50 it
///   was +$500), its wallet $500 + $500 silo + $600 = $1,600.
/// * ALICE sells at $40: −$600 realized against a $500 silo — a $100 shortfall,
///   `(p_b − maxPrice) × 10`. Her wallet stays at $500 (an isolated loss stops at the silo).
/// * The fund absorbs exactly that $100 (`InsuranceFundChanged(−100_000_000)`) and nothing is
///   socialized; `Σ(wallet + margin + vQuote) + fund` over the two traders is unchanged.
///
/// With a $30 fund the same $100 takes the fund to 0 and the $70 it cannot cover is
/// `InsuranceFundDepleted` — the path every isolated bankruptcy deficit takes: KEEPER is still paid
/// in full at $40, so the system holds $70 of claims nothing backs.
#[test]
fn an_insolvent_long_past_a_lowered_max_price_is_adld_there_and_the_fund_takes_the_gap() {
    let mut ctx = opened(Side::Buy, FUND);
    update_market(&mut ctx, 4_000);
    let before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
    tick(&mut ctx, 3_500);
    assert_eq!(storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(), 3_500);
    assert_eq!(
        outcome(&mut ctx),
        Outcome {
            adls: vec![(ALICE, KEEPER, 10, 4_000)],
            fund_moves: vec![-100_000_000],
            depleted: vec![],
            alice_wallet: 500_000_000,
            keeper_wallet: 1_600_000_000,
            alice_amount: 0,
            keeper_amount: 0,
            fund: FUND - 100_000_000,
            realized: vec![(ALICE, -600_000_000), (KEEPER, 600_000_000)],
        }
    );
    assert_eq!(conservation_sum(&mut ctx, &[ALICE, KEEPER]), before, "value conserved");

    // A fund too shallow for the gap: it covers $30, and the $70 left is socialized.
    let mut ctx = opened(Side::Buy, 30_000_000);
    update_market(&mut ctx, 4_000);
    let before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
    tick(&mut ctx, 3_500);
    let o = outcome(&mut ctx);
    assert_eq!(o.adls, vec![(ALICE, KEEPER, 10, 4_000)]);
    assert_eq!((o.fund_moves, o.depleted, o.fund), (vec![-30_000_000], vec![70_000_000], 0));
    assert_eq!((o.alice_wallet, o.keeper_wallet), (500_000_000, 1_600_000_000));
    assert_eq!(
        conservation_sum(&mut ctx, &[ALICE, KEEPER]),
        before + 70_000_000,
        "the socialized $70 is paid to KEEPER with nothing behind it — the only imbalance"
    );
}

/// **(b) Control: a bankruptcy price at or below `maxPrice` fills there, as before.** The same
/// liquidation with `maxPrice` lowered to exactly $50 = `p_b`, to $60, or left at $10,000: ALICE is
/// ADL'd at $50, KEEPER realizes +$500 (wallet $1,500), ALICE's silo covers her loss to the unit and
/// the fund is not touched.
#[test]
fn a_bankruptcy_price_at_or_below_max_price_is_not_capped() {
    for max_price in [5_000, 6_000, 1_000_000] {
        let mut ctx = opened(Side::Buy, FUND);
        if max_price != 1_000_000 {
            update_market(&mut ctx, max_price);
        }
        let before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
        tick(&mut ctx, 3_500);
        assert_eq!(
            outcome(&mut ctx),
            Outcome {
                adls: vec![(ALICE, KEEPER, 10, 5_000)],
                fund_moves: vec![],
                depleted: vec![],
                alice_wallet: 500_000_000,
                keeper_wallet: 1_500_000_000,
                alice_amount: 0,
                keeper_amount: 0,
                fund: FUND,
                realized: vec![(ALICE, -500_000_000), (KEEPER, 500_000_000)],
            },
            "maxPrice {max_price}"
        );
        assert_eq!(conservation_sum(&mut ctx, &[ALICE, KEEPER]), before, "maxPrice {max_price}");
    }
}

/// The maintenance table that makes a 2x position with $500 of equity on $1,000 of notional
/// liquidatable without moving the mark: one tier, 1x, a 60% maintenance rate (MM $600).
fn tighten_maintenance(ctx: &mut TestCtx) {
    set_margin_tiers_with_rates(ctx, ADMIN, MARKET_ID, &[0], &[1], &[600_000]).unwrap();
}

/// **(c) The window after a lowering: a residual priced at the stored mark is held at `maxPrice`.**
/// `updateMarket` lowers `maxPrice` under the stored $100 mark, which stays until the next oracle
/// update; `setMarginTiers` (60% maintenance) then puts ALICE under maintenance at that mark while
/// she is still SOLVENT there (equity $500), and `liquidate()` runs on the stored mark: empty book,
/// so ADL — at the mark, $100, before the cap; now at `maxPrice`.
///
/// * LONG, `maxPrice` $90: she sells at $90 — realized −$100, wallet $500 + $400 = $900; KEEPER buys
///   at $90 — +$100, wallet $1,100. Still solvent at the cap: the fund is not touched.
/// * LONG, `maxPrice` $40: she is solvent at the $100 mark but not at $40 (equity −$100), so the cap
///   leaves her $100 short: the fund absorbs it; KEEPER realizes +$600 (wallet $1,600).
/// * SHORT, `maxPrice` $90: the cap is in her favour — she buys back at $90, +$100, wallet $1,100 —
///   and KEEPER, the long, sells at $90: −$100, wallet $900. A short is capped only in this window.
///
/// At the mark (before the cap) each fill was neutral for both: realized 0, wallets $1,000 / $1,000.
#[test]
fn in_the_window_after_a_lowering_a_residual_at_the_stored_mark_is_held_at_max_price() {
    #[allow(clippy::type_complexity)]
    let cases: [(Side, u64, (u64, u64), i64, Vec<i64>); 3] = [
        (Side::Buy, 9_000, (900_000_000, 1_100_000_000), -100_000_000, vec![]),
        (Side::Buy, 4_000, (500_000_000, 1_600_000_000), -600_000_000, vec![-100_000_000]),
        (Side::Sell, 9_000, (1_100_000_000, 900_000_000), 100_000_000, vec![]),
    ];
    for (side, max_price, (alice_wallet, keeper_wallet), alice_realized, fund_moves) in cases {
        let mut ctx = opened(side, FUND);
        update_market(&mut ctx, max_price);
        tighten_maintenance(&mut ctx);
        assert_eq!(storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(), ENTRY_PRICE, "the window");
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
        liquidate(&mut ctx, ALICE).unwrap();
        let absorbed: i64 = fund_moves.iter().sum();
        assert_eq!(
            outcome(&mut ctx),
            Outcome {
                adls: vec![(ALICE, KEEPER, 10, max_price)],
                fund_moves,
                depleted: vec![],
                alice_wallet,
                keeper_wallet,
                alice_amount: 0,
                keeper_amount: 0,
                fund: (FUND as i64 + absorbed) as u64,
                realized: vec![(ALICE, alice_realized), (KEEPER, -alice_realized)],
            },
            "{side:?} at maxPrice {max_price}"
        );
        assert_eq!(conservation_sum(&mut ctx, &[ALICE, KEEPER]), before, "{side:?} at {max_price}");
    }
}

/// **(d) Outside that window an isolated SHORT is never capped:** its ADL price is the mark or a
/// bankruptcy price below it, and every oracle update holds the mark at or below `maxPrice`. ALICE
/// short 10 @ $100 on a $500 silo (`p_b` = $150), `maxPrice` lowered to $155 or $160 (or left at
/// $10,000), the tick to $155: she is insolvent (equity −$50) and ADL'd at $150 — even with the mark
/// AT `maxPrice` — KEEPER realizing +$500 and the fund untouched, exactly as before the cap. And a
/// short still solvent at a mark that IS `maxPrice` ($130: equity $200 under $216.67 maintenance) is
/// ADL'd at that mark, neutrally.
#[test]
fn outside_the_window_an_isolated_short_is_never_capped() {
    for max_price in [15_500, 16_000, 1_000_000] {
        let mut ctx = opened(Side::Sell, FUND);
        if max_price != 1_000_000 {
            update_market(&mut ctx, max_price);
        }
        tick(&mut ctx, 15_500);
        assert_eq!(
            outcome(&mut ctx),
            Outcome {
                adls: vec![(ALICE, KEEPER, 10, 15_000)],
                fund_moves: vec![],
                depleted: vec![],
                alice_wallet: 500_000_000,
                keeper_wallet: 1_500_000_000,
                alice_amount: 0,
                keeper_amount: 0,
                fund: FUND,
                realized: vec![(ALICE, -500_000_000), (KEEPER, 500_000_000)],
            },
            "maxPrice {max_price}"
        );
    }
    let mut ctx = opened(Side::Sell, FUND);
    update_market(&mut ctx, 13_000);
    tick(&mut ctx, 13_000);
    let o = outcome(&mut ctx);
    assert_eq!(o.adls, vec![(ALICE, KEEPER, 10, 13_000)]);
    assert_eq!(o.realized, vec![(ALICE, -300_000_000), (KEEPER, 300_000_000)]);
    assert_eq!((o.alice_wallet, o.keeper_wallet, o.fund), (700_000_000, 1_300_000_000, FUND));
}


/// One oracle tick at `ts` (the mark follows `index`, held at `maxPrice`).
fn tick_at(ctx: &mut TestCtx, index: u64, ts: u64) {
    advance_block_to(ctx, ts);
    run_update_index_price(
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: ts }.abi_encode(),
        ADMIN,
        ctx,
    )
    .unwrap();
}

/// **A capped long's ADL cut short pays each filled slice's gap out of what is left of its own silo
/// first, so a `maxPrice` raise before the rest is ADL'd makes the rest's winners pay its raised
/// bankruptcy price** (third-round review D2-continuation-1 — pinned as it stands: the routing is
/// `apply_position_fill`'s rule for every isolated close, kept for this case too — accepted by the
/// owner as a known issue, 2026-11-19).
///
/// ALICE long 130 @ $100 on a $6,500 silo at 2x (`p_b` $50); 130 one-lot ISOLATED shorts @ $100, each
/// on a $50 silo at 2x, are the only opposite holders; a $5,000 fund. `updateMarket` lowers `maxPrice`
/// to $40 and the tick to $35 liquidates her: empty book, so ADL at $40 — and the pass stops at the
/// 128-fill ADL budget. Each one-lot slice realizes −$60 on its $50 pro-rata silo; its $10 gap is
/// drawn from the REST's silo first, so the fund takes $1,180 = 128 × $10 − $100 (the rest's whole
/// pro-rata silo) and ALICE is left long 2, vQuote −$200, margin 0: her rest's `p_b` is now her entry,
/// $100.
///
/// * No raise — the next tick ADLs the rest at $40 too: the fund takes 2 × $60 = $120, $1,300 in all =
///   ($50 − $40) × 130, exactly what one uninterrupted pass would have billed; every winner has $110.
/// * `updateMarket` raises `maxPrice` back to $10,000 before that tick: the rest's own price ($100) is
///   under the cap, so its two winners buy back at $100 — realized 0, wallet $50, against their 128
///   peers' $110 — and the fund takes nothing more ($1,180 in all). They paid $50 a lot above ALICE's
///   $50 bankruptcy price (what they paid before D2): together the $100 her rest's silo had put toward
///   the fund's share.
///
/// Value is conserved either way, and no winner pays above the `maxPrice` in force at its fill.
#[test]
fn a_capped_long_adl_cut_short_draws_its_rest_silo_first_so_a_raise_reprices_the_rest() {
    const LOTS: u8 = 130;
    const BIG_FUND: u64 = 5_000_000_000;
    for raise in [false, true] {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_account(
            &mut ctx,
            ADMIN,
            UserAccount { perp_wallet_balance: BIG_FUND as i64, ..UserAccount::default() },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
        run_deposit_insurance_fund(
            &depositInsuranceFundCall { amount: BIG_FUND }.abi_encode(),
            ADMIN,
            &mut ctx,
        )
        .unwrap();
        seed_position_account(&mut ctx, ALICE, LOTS as i64, -13_000_000_000, 6_500_000_000, 2, 0);
        // `0xd0…d0NN`: clear of ALICE / KEEPER / MAKER (`[i; 20]`) and of each other.
        let winners: Vec<Address> = (1..=LOTS)
            .map(|i| {
                let mut a = [0xd0u8; 20];
                a[19] = i;
                Address::from(a)
            })
            .collect();
        for &w in &winners {
            seed_position_account(&mut ctx, w, -1, 100_000_000, 50_000_000, 2, 0);
        }
        let mut everyone = winners.clone();
        everyone.push(ALICE);
        let before = conservation_sum(&mut ctx, &everyone);
        update_market(&mut ctx, 4_000);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        // Pass 1: the ADL budget's 128 fills, all at maxPrice.
        tick_at(&mut ctx, 3_500, 31);
        let logs = JournalTr::take_logs(ctx.journal_mut());
        let adls = decode_events::<Adl>(&logs);
        assert_eq!(adls.len(), 128, "raise {raise}: the ADL budget");
        assert!(adls.iter().all(|a| (a.liquidatedUser, a.qty, a.price) == (ALICE, 1, 4_000)));
        let fund_moves: Vec<i64> =
            decode_events::<InsuranceFundChanged>(&logs).iter().map(|e| e.delta).collect();
        assert_eq!(fund_moves, vec![-1_180_000_000], "128 × $10 less the rest's $100 silo");
        assert!(decode_events::<InsuranceFundDepleted>(&logs).is_empty());
        let rest = position(&mut ctx, ALICE);
        assert_eq!(
            (rest.amount, rest.v_quote_balance, rest.margin),
            (2, -200_000_000, 0),
            "the rest's silo drawn to 0: its p_b is its $100 entry"
        );

        // Pass 2: the rest, at the next tick — after a raise or not.
        if raise {
            update_market(&mut ctx, 1_000_000);
        }
        tick_at(&mut ctx, 3_500, 46);
        let logs = JournalTr::take_logs(ctx.journal_mut());
        let price = if raise { 10_000 } else { 4_000 };
        assert_eq!(
            decode_events::<Adl>(&logs)
                .iter()
                .map(|a| (a.liquidatedUser, a.adlUser, a.qty, a.price))
                .collect::<Vec<_>>(),
            vec![(ALICE, winners[128], 1, price), (ALICE, winners[129], 1, price)],
            "raise {raise}"
        );
        let fund_moves: Vec<i64> =
            decode_events::<InsuranceFundChanged>(&logs).iter().map(|e| e.delta).collect();
        let (pass2_fund, fund_total) =
            if raise { (vec![], 1_180_000_000) } else { (vec![-120_000_000], 1_300_000_000) };
        assert_eq!(fund_moves, pass2_fund, "raise {raise}");
        assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), BIG_FUND - fund_total);
        assert_eq!(position(&mut ctx, ALICE).amount, 0);
        let last_two = if raise { 50_000_000 } else { 110_000_000 };
        for (i, &w) in winners.iter().enumerate() {
            assert_eq!(position(&mut ctx, w).amount, 0);
            let want = if i < 128 { 110_000_000 } else { last_two };
            assert_eq!(wallet(&mut ctx, w), want, "raise {raise}: winner {i}");
        }
        assert_eq!(conservation_sum(&mut ctx, &everyone), before, "raise {raise}: value conserved");
    }
}
