//! A CROSSED liquidation's ADL fill is held at `maxPrice` too (owner decision 2026-11-19, the
//! cross half of what D2 did for an ISOLATED one — `risk/tests/adl_max_price.rs`):
//! `risk::adl::run_adl` fills a cross loser at `min(its episode's price, maxPrice)`, so no ADL fill
//! of either margin type executes above `maxPrice`.
//!
//! The episode's price is the stored MARK while the account's `E ≥ 0` (or a leg is unpriced), its
//! MM-share bankruptcy price clamped into the band otherwise — and the band's upper edge is
//! `maxPrice` already. So the cap bites in the WINDOW between an `updateMarket` that lowers
//! `maxPrice` under the stored mark and the next oracle update, where `liquidate()` and
//! `drainDeferredWork` run on that stored mark.
//!
//! What each side gets, at `maxPrice` instead of the mark:
//! * a LONG loser sells lower: its cross wallet takes the difference, and what that leaves the
//!   account short takes the route the band's edge already leaves a deficit on — the episode's next
//!   cross leg at its MM-share bankruptcy price, and once the account holds no position the
//!   insurance fund (Q8);
//! * a SHORT loser buys back lower: it keeps the difference;
//! * SHORT winners buy lower (better for them); LONG winners sell lower: one that cannot absorb
//!   that (an entry above `maxPrice` on a thin silo) is fallback tier, filled after every clean
//!   holder, its shortfall the fund's.
//!
//! ALICE is the CROSSED account (3x), every other holder ISOLATED. Fees are 0 and the band is off,
//! so every number is exact, and every step is checked against the custody identity.

use super::*;
use crate::interface::IPerpDex::updateMarketCall;

/// `updateMarket(m)` by ADMIN: the fixture's market with `maxPrice` = `max_price`. A lowering is
/// always accepted, and the stored mark stays where it is until the next oracle update.
fn set_max_price(ctx: &mut TestCtx, m: u64, max_price: u64) {
    let mark = storage::load_mark_price(ctx, m).unwrap();
    let mk = market(m);
    ok(
        ctx,
        ADMIN,
        &updateMarketCall {
            marketId: m,
            tickSize: mk.tick_size,
            stepSize: mk.step_size,
            minQuantity: mk.min_quantity,
            maxQuantity: mk.max_quantity,
            maxPrice: max_price,
            priceUpdateInterval: mk.price_update_interval,
            active: true,
            fundingInterval: mk.funding_interval,
            interestRate: mk.interest_rate,
            liquidationFeeRateBps: mk.liquidation_fee_rate_bps,
            priceBandBps: mk.price_band_bps,
        }
        .abi_encode(),
    );
    assert_eq!(storage::load_mark_price(ctx, m).unwrap(), mark, "the stored mark is untouched");
}

/// ALICE CROSSED `side` 10 on M1 @ $100 against BOB (ISOLATED, the other side, at 3x: a
/// $333.333333 silo), her wallet set to $100 after it: at the $100 mark `E = $100` against
/// `Σ MM = $166.666666` — under maintenance, still solvent. A fund of $1,000.
fn alice_10_on_m1(ctx: &mut TestCtx, side: u8) -> u64 {
    set_leverage(ctx, BOB, M1, 3);
    trade(ctx, M1, ALICE, BOB, side, px(100), 10);
    set_wallet(ctx, ALICE, usd(100));
    let f = fold(ctx, ALICE);
    assert_eq!((f.e, f.cross_mm), (100_000_000, 166_666_666));
    assert_eq!(pos(ctx, BOB, M1).margin, BOB_SILO);
    insurance_fund_seeded(ctx, usd(1_000))
}

/// BOB's silo on 10 @ $100 at 3x.
const BOB_SILO: i64 = 333_333_333;

/// `liquidate(ALICE, M1)` by KEEPER, under the custody identity (the fund's socialised part, if
/// any, opens it by exactly that much): the call's logs.
fn liquidate_alice(ctx: &mut TestCtx, ledger: &mut Ledger) -> Vec<Log> {
    let logs = ok(ctx, KEEPER, &liquidate(ALICE, M1));
    ledger.total +=
        evs::<InsuranceFundDepleted>(&logs).iter().map(|d| d.badDebt as i128).sum::<i128>();
    ledger.check(ctx, "after liquidate()");
    assert_eq!(evs::<CrossLiquidationStarted>(&logs).len(), 1);
    assert_eq!(evs::<CrossLiquidationEnded>(&logs).len(), 1, "no cross position left");
    assert!(!flagged(ctx, ALICE));
    assert_eq!((pos(ctx, ALICE, M1).amount, pos(ctx, BOB, M1).amount), (0, 0));
    logs
}

/// **(a) The window, a LONG loser: the ADL at the stored $100 mark is held at the lowered
/// `maxPrice`.** ALICE (long 10, `E = $100` at the mark) is `liquidate()`d with an empty book, so
/// all 10 go to BOB by ADL — at the mark, $100, before the cap; now at `maxPrice`:
///
/// * `maxPrice` $95: she sells at $95 — realized −$50, wallet $50, still solvent; BOB buys back at
///   $95, +$50 (his wallet + silo + $50). The fund is not touched.
/// * `maxPrice` $85: realized −$150 takes her wallet to −$50 with nothing left open, and the end of
///   the call clears it against the fund (Q8: `InsuranceFundChanged(−50_000_000)`, an
///   `INSURANCE_CLEAR` row at 0) — the route the band's edge leaves a cross deficit on; no bad debt
///   is reported by the ADL itself. BOB +$150.
/// * the same with a $20 fund: it pays $20 and `InsuranceFundDepleted(M1, 30_000_000)` socialises
///   the rest; ALICE still ends at 0, BOB is still paid at $85.
///
/// At the mark each fill was neutral: ALICE kept $100, BOB realized 0.
#[test]
fn in_the_window_a_cross_long_is_adld_at_max_price_and_its_gap_takes_the_q8_route() {
    for (max_price, fund, alice_realized, depleted) in [
        (px(95), usd(1_000), usd(-50), 0),
        (px(85), usd(1_000), usd(-150), 0),
        (px(85), usd(20), usd(-150), usd(30)),
    ] {
        let mut ctx = make_ctx();
        alice_10_on_m1(&mut ctx, BUY);
        storage::save_insurance_fund(&mut ctx, fund as u64).unwrap();
        set_max_price(&mut ctx, M1, max_price);
        let mut ledger = Ledger::open(&mut ctx);
        let bob_w0 = wallet(&mut ctx, BOB);

        let logs = liquidate_alice(&mut ctx, &mut ledger);
        assert!(evs::<Trade>(&logs).is_empty(), "nothing in the book");
        assert_eq!(adls(&logs), [(ALICE, BOB, M1, 10, max_price)], "maxPrice {max_price}");
        let bob_gain = wallet(&mut ctx, BOB) - bob_w0;
        assert_eq!(bob_gain, BOB_SILO - alice_realized, "maxPrice {max_price}");
        assert_eq!(evs::<Liquidation>(&logs)[0].reward, 0);
        let left = usd(100) + alice_realized;
        if left >= 0 {
            assert_eq!(wallet(&mut ctx, ALICE), left);
            assert!(evs::<InsuranceFundChanged>(&logs).is_empty());
            assert_eq!(insurance_fund(&mut ctx), fund as u64);
        } else if depleted == 0 {
            assert_cleared(&mut ctx, &logs, ALICE, -left, fund as u64);
            assert_eq!(insurance_fund(&mut ctx), usd(950) as u64);
        } else {
            assert_eq!(wallet(&mut ctx, ALICE), 0);
            assert_eq!(insurance_fund(&mut ctx), 0);
            let changed = evs::<InsuranceFundChanged>(&logs);
            assert_eq!(changed.iter().map(|c| c.delta).collect::<Vec<_>>(), [-fund]);
            let d = evs::<InsuranceFundDepleted>(&logs);
            let d: Vec<(u64, u64)> = d.iter().map(|d| (d.marketId, d.badDebt)).collect();
            assert_eq!(d, [(M1, depleted as u64)]);
            assert_eq!(rows(&logs, ALICE).last().unwrap().reason, INSURANCE_CLEAR);
        }
    }
}

/// **(a) The window, a SHORT loser: held at `maxPrice` in its favour.** ALICE short 10 @ $100
/// (`E = $100` at the mark), `maxPrice` lowered to $95: she buys back at $95 — +$50, wallet $150 —
/// and BOB, the long winner, sells at $95: −$50 out of his $333.333333 silo (his wallet +$283.33),
/// clean: his equity there is $283.33, so the fill leaves no shortfall anywhere and the fund is not
/// touched. At the mark both were neutral.
#[test]
fn in_the_window_a_cross_short_is_adld_at_max_price_in_its_favour() {
    let mut ctx = make_ctx();
    let fund = alice_10_on_m1(&mut ctx, SELL);
    set_max_price(&mut ctx, M1, px(95));
    let mut ledger = Ledger::open(&mut ctx);
    let bob_w0 = wallet(&mut ctx, BOB);

    let logs = liquidate_alice(&mut ctx, &mut ledger);
    assert_eq!(adls(&logs), [(ALICE, BOB, M1, 10, px(95))]);
    assert_eq!(wallet(&mut ctx, ALICE), usd(150));
    assert_eq!(wallet(&mut ctx, BOB) - bob_w0, BOB_SILO - usd(50));
    assert!(evs::<InsuranceFundChanged>(&logs).is_empty());
    assert_eq!(insurance_fund(&mut ctx), fund);
}

/// **(b) Control: `maxPrice` at or above the stored mark changes nothing.** Either side, `maxPrice`
/// lowered to exactly the $100 mark, to $150, or left at $1,000,000: ADL at the $100 mark, ALICE
/// keeps her $100, BOB realizes 0 (his silo back), the fund untouched.
#[test]
fn a_max_price_at_or_above_the_mark_leaves_the_cross_adl_at_the_mark() {
    for side in [BUY, SELL] {
        for max_price in [px(100), px(150), market(M1).max_price] {
            let mut ctx = make_ctx();
            let fund = alice_10_on_m1(&mut ctx, side);
            if max_price != market(M1).max_price {
                set_max_price(&mut ctx, M1, max_price);
            }
            let mut ledger = Ledger::open(&mut ctx);
            let bob_w0 = wallet(&mut ctx, BOB);

            let logs = liquidate_alice(&mut ctx, &mut ledger);
            assert_eq!(adls(&logs), [(ALICE, BOB, M1, 10, px(100))], "{side} at {max_price}");
            assert_eq!(wallet(&mut ctx, ALICE), usd(100));
            assert_eq!(wallet(&mut ctx, BOB) - bob_w0, BOB_SILO);
            assert!(evs::<InsuranceFundChanged>(&logs).is_empty());
            assert_eq!(insurance_fund(&mut ctx), fund);
        }
    }
}

/// **(c) A LONG winner whose entry is above `maxPrice`: the winner policy as it stands.** ALICE
/// short 10 in the window, `maxPrice` lowered to $60 — under the $100 at which her winners went
/// long. Each long winner sells at $60, a $40-a-lot loss:
///
/// * BOB alone (3x, silo $333.333333): his equity at $60 is −$66.666667, so he is not a clean
///   candidate — fallback tier. As the only holder he is filled there (`BothSides`): his whole silo
///   goes, nothing comes back (wallet unchanged), and the $66.666667 his silo cannot cover is his
///   bad debt, absorbed by the fund (`InsuranceFundChanged(−66_666_667)`). ALICE buys back at $60:
///   +$400, wallet $500. (At $95 he was clean — the short-loser test above.)
/// * BOB (3x) and CAROL (1x) long 5 each: CAROL's $500 silo absorbs her $200 loss (equity $300 at
///   $60) — clean — so she is filled FIRST, though at the mark the two rank equal and BOB, the
///   lower address, would go first (as he does at $95); BOB, fallback, takes the other 5 last, his
///   $33.333334 shortfall the fund's. ALICE again ends at $500.
#[test]
fn a_long_winner_that_cannot_sell_at_max_price_is_fallback_tier_its_shortfall_the_funds() {
    // BOB alone.
    let mut ctx = make_ctx();
    let fund = alice_10_on_m1(&mut ctx, SELL);
    set_max_price(&mut ctx, M1, px(60));
    let mut ledger = Ledger::open(&mut ctx);
    let bob_w0 = wallet(&mut ctx, BOB);
    let logs = liquidate_alice(&mut ctx, &mut ledger);
    assert_eq!(adls(&logs), [(ALICE, BOB, M1, 10, px(60))]);
    assert_eq!(wallet(&mut ctx, ALICE), usd(500));
    assert_eq!(wallet(&mut ctx, BOB), bob_w0, "his silo is gone, nothing back");
    let shortfall = usd(400) - BOB_SILO;
    assert_eq!(shortfall, 66_666_667);
    let changed = evs::<InsuranceFundChanged>(&logs);
    assert_eq!(changed.iter().map(|c| c.delta).collect::<Vec<_>>(), [-shortfall]);
    assert_eq!(insurance_fund(&mut ctx), fund - shortfall as u64);
    assert!(evs::<InsuranceFundDepleted>(&logs).is_empty());

    // BOB (3x) and CAROL (1x), 5 each: the clean one first at $60, by address at $95.
    for (max_price, winners, bob_shortfall) in
        [(px(60), [CAROL, BOB], 33_333_334), (px(95), [BOB, CAROL], 0)]
    {
        let mut ctx = make_ctx();
        set_leverage(&mut ctx, BOB, M1, 3);
        trade(&mut ctx, M1, ALICE, BOB, SELL, px(100), 5);
        trade(&mut ctx, M1, ALICE, CAROL, SELL, px(100), 5);
        set_wallet(&mut ctx, ALICE, usd(100));
        let fund = insurance_fund_seeded(&mut ctx, usd(1_000));
        let bob_silo = pos(&mut ctx, BOB, M1).margin;
        let carol_silo = pos(&mut ctx, CAROL, M1).margin;
        assert_eq!((bob_silo, carol_silo), (166_666_666, 500_000_000));
        set_max_price(&mut ctx, M1, max_price);
        let ledger = Ledger::open(&mut ctx);
        let (bob_w0, carol_w0) = (wallet(&mut ctx, BOB), wallet(&mut ctx, CAROL));

        let logs = ok(&mut ctx, KEEPER, &liquidate(ALICE, M1));
        ledger.check(&mut ctx, "after liquidate()");
        assert_eq!(
            adls(&logs),
            [(ALICE, winners[0], M1, 5, max_price), (ALICE, winners[1], M1, 5, max_price)],
            "maxPrice {max_price}"
        );
        let loss = usd(5 * (100 - max_price as i64 / 100));
        assert_eq!(wallet(&mut ctx, ALICE), usd(100) + 2 * loss);
        assert_eq!(wallet(&mut ctx, CAROL) - carol_w0, carol_silo - loss);
        assert_eq!(wallet(&mut ctx, BOB) - bob_w0, (bob_silo - loss).max(0));
        assert_eq!(bob_shortfall, (loss - bob_silo).max(0));
        assert_eq!(insurance_fund(&mut ctx), fund - bob_shortfall as u64, "maxPrice {max_price}");
        assert!(!flagged(&mut ctx, ALICE));
    }
}

/// **The gap of a capped cross long takes the band clamp's route — the episode's NEXT leg.** ALICE
/// long 10 on M1 (against BOB) and on M2 (against CAROL), wallet $200: `E = $200` against
/// `Σ MM = $333.33`. `maxPrice` of M1 lowered to $70 under its $100 mark; `liquidate(ALICE, M1)`:
///
/// * M1 first: the IOC's limit is its MM-share bankruptcy price ($90) clamped into the band, whose
///   upper edge is now $70; the book is empty, and the rest goes by ADL at the $100 mark (`E ≥ 0`)
///   held at $70: −$300, `E = −$100`, still under M2's maintenance;
/// * M2: `E < 0` and every leg priced, so its rest goes at the MM-share bankruptcy price — the
///   whole deficit is M2's share: `$1,000 + $100` of notional, $110, inside M2's band — and CAROL,
///   its winner, absorbs the $100 (realized −$100). ALICE ends flat at exactly 0; the fund is
///   untouched.
///
/// Before the cap the M1 ADL at the mark left `E = $200 ≥ MM_2`: a partial liquidation, M2 kept.
#[test]
fn a_capped_cross_longs_gap_is_priced_into_the_episodes_next_leg_like_the_band_edges() {
    let mut ctx = make_ctx();
    longs_on_m1_and_m2(&mut ctx, usd(200));
    let fund = insurance_fund_seeded(&mut ctx, usd(1_000));
    let f = fold(&mut ctx, ALICE);
    assert_eq!((f.e, f.cross_mm), (200_000_000, 333_333_332));
    set_max_price(&mut ctx, M1, px(70));
    let ledger = Ledger::open(&mut ctx);
    let (bob_w0, carol_w0) = (wallet(&mut ctx, BOB), wallet(&mut ctx, CAROL));
    let (bob_silo, carol_silo) = (pos(&mut ctx, BOB, M1).margin, pos(&mut ctx, CAROL, M2).margin);

    let logs = ok(&mut ctx, KEEPER, &liquidate(ALICE, M1));
    ledger.check(&mut ctx, "after liquidate()");
    let closes: Vec<(u64, u64)> = evs::<OrderPlaced>(&logs)
        .iter()
        .filter(|o| o.user == ALICE)
        .map(|o| (o.marketId, o.price))
        .collect();
    assert_eq!(closes, [(M1, px(70)), (M2, px(110))], "the IOC limits, each clamped into its band");
    assert_eq!(adls(&logs), [(ALICE, BOB, M1, 10, px(70)), (ALICE, CAROL, M2, 10, px(110))]);
    assert_eq!(wallet(&mut ctx, ALICE), 0);
    assert_eq!(wallet(&mut ctx, BOB) - bob_w0, bob_silo + usd(300));
    assert_eq!(wallet(&mut ctx, CAROL) - carol_w0, carol_silo - usd(100));
    assert_eq!(insurance_fund(&mut ctx), fund, "no Q8: the next leg took the gap");
    assert!(evs::<InsuranceFundChanged>(&logs).is_empty());
    let liq: Vec<u64> = evs::<Liquidation>(&logs).iter().map(|l| l.marketId).collect();
    assert_eq!(liq, [M1, M2]);
    assert!(!flagged(&mut ctx, ALICE));
}

/// **(a) The window through `drainDeferredWork`.** ALICE long 135 @ $100, wallet $2,750, 135
/// single-lot bids at $80 (`capped_cross_fixture`). The tick to $80 fills 128 of them and stops on
/// the distinct-maker cap: `E = $50` against `Σ MM = $93.33`, 7 left, M1's deferred-work bit set.
/// `updateMarket` then lowers `maxPrice` to $70 under the $80 mark, and the drain runs on that
/// stored mark: the 7 bids at $80 are stranded above it and expired (`OrderCancelled` reason 4),
/// the IOC (limit clamped to $70) finds nothing, and the 7 go to BOB by ADL at the $80 mark held at
/// $70: −$210 on a $190 wallet, −$20 with nothing left open, cleared by the fund (Q8). BOB +$210.
/// (Without the lowering the drain closes the 7 on the book at $80 —
/// `a_maker_capped_cross_close_resumes_on_the_drain_instead_of_adl`.)
#[test]
fn in_the_window_the_drain_adls_a_cross_long_at_max_price() {
    let mut ctx = make_ctx();
    capped_cross_fixture(&mut ctx, 135);
    let fund = insurance_fund_seeded(&mut ctx, usd(1_000));
    let _ = tick(&mut ctx, M1, px(80));
    assert_eq!((pos(&mut ctx, ALICE, M1).amount, deferred_of(&mut ctx, M1)), (7, 1));
    assert_eq!(wallet(&mut ctx, ALICE), usd(190));
    let f = fold(&mut ctx, ALICE);
    assert_eq!((f.e, f.cross_mm), (50_000_000, 93_333_333));
    set_max_price(&mut ctx, M1, px(70));
    assert_eq!(storage::load_mark_price(&mut ctx, M1).unwrap(), px(80), "the window");
    // The 128 makers are outside the identity's users; the drain only expires their bids, so the
    // identity's value holds over the users and `Σ amount` is checked with them included.
    let ledger = Ledger::open(&mut ctx);
    let (bob_w0, bob_silo0) = (wallet(&mut ctx, BOB), pos(&mut ctx, BOB, M1).margin);

    let (more, logs) = drain_market(&mut ctx, M1);
    assert_eq!(Ledger::system_value(&mut ctx), ledger.total, "custody across the drain");
    assert_eq!(sigma_amount(&mut ctx, M1, &[ALICE, BOB], 135), 0);
    assert!(!more);
    let expired: Vec<u8> = evs::<OrderCancelled>(&logs).iter().map(|c| c.reason).collect();
    assert_eq!(expired, [4; 7], "the bids stranded above maxPrice");
    assert!(evs::<Trade>(&logs).is_empty());
    let close: Vec<u64> =
        evs::<OrderPlaced>(&logs).iter().filter(|o| o.user == ALICE).map(|o| o.price).collect();
    assert_eq!(close, [px(70)]);
    assert_eq!(adls(&logs), [(ALICE, BOB, M1, 7, px(70))]);
    let released = bob_silo0 - pos(&mut ctx, BOB, M1).margin;
    assert_eq!(wallet(&mut ctx, BOB) - bob_w0, released + usd(210), "his 7 lots' silo and +$210");
    assert_cleared(&mut ctx, &logs, ALICE, usd(20), fund);
    assert_eq!(deferred_of(&mut ctx, M1), 0);
    assert!(!flagged(&mut ctx, ALICE));
}
