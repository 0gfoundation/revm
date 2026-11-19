use super::*;
use crate::margin::view::{position_open_order_margin, total_open_order_initial_margin};
use crate::types::MarginTier;

const P_LOW: u64 = 90 * TICK;
const P_HIGH: u64 = 110 * TICK;

fn market_with(mark: u64, tiers: MarginTiers) -> Market {
    Market {
        market_id: MARKET_ID,
        base_decimals: 8,
        price_decimals: 9,
        tick_size: TICK,
        step_size: QTY,
        min_quantity: QTY,
        max_quantity: QTY * 1_000,
        max_price: PRICE * 1_000,
        price_update_interval: 15,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 0,
        price_band_bps: 0,
        mark_price: mark,
        tiers,
        basis_mode: 0,
        deferred_work: 0,
        cumulative_funding_index: 0,
    }
}

/// A market with a live mark price. `setup()` leaves `mark_price` at 0, which makes the
/// derived basis blind to every position (`N = trunc(|amt| × 0) = 0`) — a test artefact, not
/// production behaviour (`addMarket` rejects a zero mark). One test below exists precisely to
/// separate that artefact from the real numbers.
fn setup_marked(ctx: &mut TestCtx, mark: u64) {
    storage::save_admin(ctx, ADMIN).unwrap();
    storage::save_market(ctx, &market_with(mark, MarginTiers::default())).unwrap();
    fund(ctx, ALICE, WALLET * 100);
    fund(ctx, BOB, WALLET * 100);
}

fn place(ctx: &mut TestCtx, who: Address, side: u8, price: u64, lots: u64) -> [u8; 32] {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: QTY * lots,
        orderType: 0,
        tif: 3, // PostOnly: rest without matching, so the book state is exactly as written
        clientOrderId: FixedBytes::default(),
                flags: 0,
    }
    .abi_encode();
    let ret = run_place_order(&input, who, ctx).expect("placement");
    ret[..32].try_into().unwrap()
}

fn seed_position(ctx: &mut TestCtx, who: Address, lots: i64, leverage: u64) {
    let notional = lots * FILL_VALUE as i64;
    storage::save_position(
        ctx,
        who,
        MARKET_ID,
        &PerpPosition {
            amount: lots * QTY as i64,
            v_quote_balance: -notional,
            margin: notional.abs(),
            leverage,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

/// ALICE's derived open-order requirement in the test market, right now.
fn oo_im_alice(ctx: &mut TestCtx) -> u64 {
    let m = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    let p = storage::load_position(ctx, ALICE, MARKET_ID).unwrap();
    position_open_order_margin(&m, &p).unwrap()
}

// ── 1. Cross-side orders are charged the MAX of the two terminal exposures ───────────────

/// **The headline shape.** With orders resting on BOTH sides of the same market the
/// requirement is `max(|N + Bid|, |N − Ask|)`: the larger of the two exposures the book could
/// leave behind, not the sum of anything.
///
/// Flat position, mark $100, leverage 1, resting BUY 2 lots @ $90 and SELL 1 lot @ $110:
///
/// | quantity | value     | how |
/// |----------|-----------|-----|
/// | `Bid`    | 1_800_000 | 2 × 900_000 |
/// | `Ask`    | 1_100_000 | 1 × 1_100_000 |
/// | ooIM     | 1_800_000 | `max(\|0 + 1_800_000\|, \|0 − 1_100_000\|) − 0` |
///
/// The retired escrow charged 2_000_000 here — `max(S + B', B + S')`, where `B' = 900_000` is
/// the buy leg left over after one lot of it is consumed covering the short the sells would
/// open. It modelled the two sides filling in SEQUENCE; Binance's formula does not, and the
/// owner accepted that loosening. **The migration cut this book's requirement by 200_000
/// (−10%).**
#[test]
fn a_cross_side_book_is_charged_the_larger_terminal_exposure() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx, PRICE);
    place(&mut ctx, ALICE, 0, P_LOW, 2);
    place(&mut ctx, ALICE, 1, P_HIGH, 1);

    let p = storage::load_position(&mut ctx, ALICE, MARKET_ID).unwrap();
    assert_eq!(
        (p.amount, p.total_buy_notional, p.total_sell_notional),
        (0, 1_800_000, 1_100_000)
    );

    assert_eq!(
        oo_im_alice(&mut ctx),
        1_800_000,
        "ooIM = max(|N+Bid|, |N-Ask|) - |N|/L"
    );
}

/// The same shape one lot smaller: BUY 1 lot @ $90, SELL 1 lot @ $110, flat ⇒
/// `max(900_000, 1_100_000) = 1_100_000`. The escrow happened to agree here
/// (`max(1_100_000 + 0, 900_000 + 0)`), so the boundary of the old divergence set is recorded
/// and not just its interior.
#[test]
fn cross_side_one_for_one_is_the_dearer_side() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx, PRICE);
    place(&mut ctx, ALICE, 0, P_LOW, 1);
    place(&mut ctx, ALICE, 1, P_HIGH, 1);
    assert_eq!(oo_im_alice(&mut ctx), 1_100_000);
}

// ── 2. The requirement RE-VALUES on every mark move ──────────────────────────────────────

/// **The property the escrow structurally could not have.** `ooIM` values the position leg at
/// the CURRENT mark on every read, so the requirement moves when the mark moves — with no
/// order, no fill, and no action by the user. The escrow was decided once at placement and
/// never revisited (it was computed in QUANTITY space at the orders' LIMIT prices; the mark
/// was not an input to it at all), so it sat frozen at 2_200_000 across this whole walk.
///
/// Long 1 lot, resting SELL 3 lots @ $110, leverage 1. Nothing has traded in this market, so the
/// Assuming-Price floor at PLACEMENT was `T = max(ROUND_UP(0 × 1.0015), $100) = $100`, below the
/// $110 limit — so each sell froze at its own $110 and `Ask = 3_300_000` for the whole walk.
/// **Only `N` moves:**
///
/// | mark | `N`       | `Ask` (frozen) | `max(\|N+Bid\|, \|N−Ask\|)` | PIM       | ooIM      | (old escrow) |
/// |------|-----------|----------------|---------------------------|-----------|-----------|--------------|
/// | $100 | 1_000_000 |      3_300_000 | 2_300_000 (ask branch)    | 1_000_000 | 1_300_000 |    2_200_000 |
/// | $150 | 1_500_000 |      3_300_000 | 1_800_000 (ask branch)    | 1_500_000 |   300_000 |    2_200_000 |
/// | $200 | 2_000_000 |      3_300_000 | 2_000_000 (BID branch)    | 2_000_000 |         0 |    2_200_000 |
///
/// ⚠️ **RE-DERIVED for R12 (was `1_300_000 → 1_500_000 → 2_000_000`).** The old numbers came
/// from re-resolving `T` on every read, so as the mark rose past $110 the sells were repriced AT
/// THE MARK and the ask branch grew with it forever —「a sell 3× the position never becomes
/// free」. R12 measured that a resting order is NOT repriced (90 frames, 1939 quanta), so the
/// growing long now does swallow the frozen sells until they are pure risk reduction and free.
/// The walk that this file previously called "wrong" is the measured one.
///
/// That staleness is the exposure the docs name and accept: 「托管会变陈旧」 — rest a sell, let
/// the market run, and its term still reflects the old print. Note the shape's own limit: a sell
/// resting at $110 with the mark at $200 is deep inside the book and would not survive as a
/// resting order in a live market; this fixture reaches it only because `save_mark_price` moves
/// the mark without running the matcher.
///
/// **The property under test is unchanged and is the point of the test: ooIM moves on the mark
/// alone.** It just moves DOWN here rather than up, and the row-to-row differences are what the
/// assertion pins.
#[test]
fn a_mark_move_reprices_the_requirement_with_no_order_and_no_fill() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx, PRICE);
    seed_position(&mut ctx, ALICE, 1, 1);
    place(&mut ctx, ALICE, 1, P_HIGH, 3);

    // The frozen basis, stated once: nothing below re-derives it.
    assert_eq!(
        storage::load_position(&mut ctx, ALICE, MARKET_ID)
            .unwrap()
            .total_sell_notional,
        3_300_000,
        "3 lots frozen at their own $110 limit (T was $100 at placement)"
    );

    let mut seen = Vec::new();
    for mark in [PRICE, 150 * TICK, 200 * TICK] {
        storage::save_mark_price(&mut ctx, MARKET_ID, mark).unwrap();
        seen.push(oo_im_alice(&mut ctx));
    }
    assert_eq!(
        seen,
        vec![1_300_000, 300_000, 0],
        "ooIM 1_300_000 -> 300_000 -> 0 on the mark alone: `N` is live even though `Ask` is not"
    );
    assert!(
        seen[0] != seen[1] && seen[1] != seen[2],
        "the requirement must genuinely MOVE with the mark — a frozen `Ask` must not have \
         frozen `ooIM` (R10)"
    );
    assert_eq!(
        storage::load_position(&mut ctx, ALICE, MARKET_ID)
            .unwrap()
            .total_sell_notional,
        3_300_000,
        "and `Ask` is byte-identical after the whole walk"
    );
}

/// The re-valuation runs BOTH ways: a mark move AGAINST the position inflates the
/// requirement. Same position and book, mark crashed to $10: `N = 100_000`, ask branch
/// `|100_000 − 3_300_000| = 3_200_000`, PIM `100_000` ⇒ ooIM `3_100_000` — 900_000 MORE than
/// the escrow's frozen 2_200_000. The migration is not a uniform loosening; it is a different
/// function, stricter in some states and looser in others.
#[test]
fn an_adverse_mark_raises_the_requirement_above_what_the_escrow_held() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx, PRICE);
    seed_position(&mut ctx, ALICE, 1, 1);
    place(&mut ctx, ALICE, 1, P_HIGH, 3);
    storage::save_mark_price(&mut ctx, MARKET_ID, 10 * TICK).unwrap();
    assert_eq!(oo_im_alice(&mut ctx), 3_100_000);
}

// ── 3. mark == 0 is a TEST artefact, and it is worth knowing which is which ──────────────

/// A zero mark makes `N = 0`, so the derived basis cannot see the position at all and a
/// risk-reducing sell reads as a naked one. Same state, two marks. Long 2 lots, resting SELL
/// 1 lot @ $110:
/// * mark 0    ⇒ `N = 0`         ⇒ ooIM `1_100_000` — the artefact.
/// * mark $100 ⇒ `N = 2_000_000` ⇒ `max(2_000_000, 900_000) = 2_000_000 = PIM` ⇒ ooIM `0`.
///
/// Production cannot reach the first row (`addMarket` rejects a zero mark and every mark
/// component is floored away from zero), so "the derived basis punishes risk-reducing orders"
/// is NOT a real finding. Recording it here stops it being rediscovered as one — and is why
/// several tests elsewhere in this file had to start setting a mark.
#[test]
fn the_pure_reduce_charge_is_a_zero_mark_artefact_only() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx, 0);
    seed_position(&mut ctx, ALICE, 2, 1);
    place(&mut ctx, ALICE, 1, P_HIGH, 1);
    assert_eq!(
        oo_im_alice(&mut ctx),
        1_100_000,
        "mark 0: the basis is blind to the long"
    );

    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    assert_eq!(oo_im_alice(&mut ctx), 0, "mark $100: a pure reduce is free");
}

// ── 4. A position that flips sign under a two-sided book ────────────────────────────────

/// `ooIM` as a function of the position, with the book held fixed. Book: BUY 2 lots @ $90
/// (`Bid = 1_800_000`) and SELL 2 lots @ $110 (`Ask = 2_200_000`), mark $100, leverage 1,
/// position walked from +2 lots to −2 lots:
///
/// | position | `N`        | ooIM      | (old escrow) |
/// |----------|------------|-----------|--------------|
/// | +2 lots  |  2_000_000 | 1_800_000 |    1_800_000 |
/// | +1 lot   |  1_000_000 | 1_800_000 |    2_000_000 |
/// |  flat    |          0 | 2_200_000 |    2_200_000 |
/// | −1 lot   | −1_000_000 | 2_200_000 |    2_200_000 |
/// | −2 lots  | −2_000_000 | 2_200_000 |    2_200_000 |
///
/// It is a STEP function: `Bid` while the position is long enough for the bid branch to win,
/// `Ask` once the ask branch takes over. The escrow tracked the same two plateaus but bulged
/// 200_000 above them at +1 lot — the cross-side residual of test 1, appearing wherever the
/// position only PARTIALLY covers one side of a two-sided book. The flip itself was never the
/// divergence source; that one mechanism was.
#[test]
fn the_requirement_is_a_step_function_of_the_position_under_a_two_sided_book() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx, PRICE);
    place(&mut ctx, ALICE, 0, P_LOW, 2);
    place(&mut ctx, ALICE, 1, P_HIGH, 2);

    let m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    let mut walk = Vec::new();
    for lots in [2i64, 1, 0, -1, -2] {
        // Move ONLY `amount`; the book and its aggregates stay exactly as placed, so the
        // position sign is the single independent variable.
        let mut p = storage::load_position(&mut ctx, ALICE, MARKET_ID).unwrap();
        p.amount = lots * QTY as i64;
        walk.push(position_open_order_margin(&m, &p).unwrap());
    }
    assert_eq!(
        walk,
        vec![1_800_000, 1_800_000, 2_200_000, 2_200_000, 2_200_000],
        "Bid while the long dominates, then Ask — one step, no bulge"
    );
}

// ── 5. The tier table does NOT enter the requirement ────────────────────────────────────

/// Phase 1 briefly enforced a TIER-CAPPED ooIM, which re-priced the position leg whenever the
/// COMBINED notional crossed a tier boundary. It was rejected: open orders are priced at the
/// position's own leverage, uncapped, as Binance does (tiers still govern maintenance margin,
/// which is continuous). This test pins the consequence on the exact fixture that used to
/// show a 10× discontinuity.
///
/// Tiers `[(0, 10x), (2_000_000, 2x)]`, position long 1 lot at leverage 10, mark $100
/// (`N = 1_000_000`):
///
/// | resting BUY  | `Bid`     | combined  | ooIM    | (tier-capped variant) |
/// |--------------|-----------|-----------|---------|-----------------------|
/// | 1 lot @ $90  |   900_000 | 1_900_000 |  90_000 |                90_000 |
/// | 2 lots @ $90 | 1_800_000 | 2_800_000 | 180_000 |               900_000 |
///
/// The capped variant dropped `L_eff` 10 → 2 on the second row, which re-priced the
/// PRE-EXISTING position as well as the new order and multiplied the requirement by 10 for
/// one extra lot. The surviving definition is linear in `Bid`: 90_000 → 180_000.
#[test]
fn the_tier_table_does_not_enter_the_open_order_requirement() {
    let tiers = MarginTiers::from_tiers(&[
        MarginTier {
            lower_bound_notional: 0,
            max_leverage: 10,
            maint_margin_rate_ppm: 0,
        },
        MarginTier {
            lower_bound_notional: 2_000_000,
            max_leverage: 2,
            maint_margin_rate_ppm: 0,
        },
    ])
    .unwrap();
    for (lots, want) in [(1u64, 90_000u64), (2, 180_000)] {
        let mut ctx = make_ctx();
        storage::save_admin(&mut ctx, ADMIN).unwrap();
        storage::save_market(&mut ctx, &market_with(PRICE, tiers)).unwrap();
        fund(&mut ctx, ALICE, WALLET * 100);
        seed_position(&mut ctx, ALICE, 1, 10);
        place(&mut ctx, ALICE, 0, P_LOW, lots);
        assert_eq!(oo_im_alice(&mut ctx), want, "{lots} lot(s) resting");
    }
}

// ── 6. The account-level fold, and the available it produces ────────────────────────────

/// The Σ walker agrees with the per-market numbers, and `available` is
/// `perp_wallet_balance − Σ ooIM` with NO gross-up term.
///
/// Phase 1 needed one (`+ Σ margin_reserved`) because the escrow had already been debited
/// from the wallet, so subtracting Σ ooIM on top would have charged the book twice. Nothing
/// is debited now: the funded balance IS the gross, and adding anything back would re-open
/// that double-count from the other side. This test is the pin on that — it is the single
/// easiest thing to get backwards in this migration.
///
/// Flat, mark $100, BUY 2 lots @ $90 + SELL 1 lot @ $110 (the cross-side case of test 1):
/// ooIM is 1_800_000, the wallet is untouched, and available is funded − 1_800_000.
#[test]
fn the_derived_available_subtracts_the_requirement_exactly_once() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx, PRICE);
    let funded = storage::load_account(&mut ctx, ALICE)
        .unwrap()
        .perp_wallet_balance;
    place(&mut ctx, ALICE, 0, P_LOW, 2);
    place(&mut ctx, ALICE, 1, P_HIGH, 1);

    let wallet = storage::load_account(&mut ctx, ALICE)
        .unwrap()
        .perp_wallet_balance;
    assert_eq!(wallet, funded, "the escrow is gone — nothing was debited");
    assert_eq!(
        total_open_order_initial_margin(&mut ctx, ALICE, None).unwrap(),
        1_800_000
    );

    let available = crate::margin::view::derived_available_balance(&mut ctx, ALICE).unwrap();
    assert_eq!(available, funded as i128 - 1_800_000);
    // The trap, stated numerically: grossing the (nonexistent) escrow back up, as Phase 1
    // correctly did, would now over-report by exactly the requirement.
    assert_ne!(available, funded as i128);
}

/// A user active in SEVERAL markets folds every one of them, and a market they have left
/// contributes nothing (it is not in the index, and its ooIM would be 0 anyway).
#[test]
fn the_sum_spans_every_market_in_the_index_and_only_those() {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    for id in 1..=3u64 {
        let mut m = market_with(PRICE, MarginTiers::default());
        m.market_id = id;
        storage::save_market(&mut ctx, &m).unwrap();
    }
    fund(&mut ctx, ALICE, WALLET * 100);

    let mut expect = 0u128;
    for id in 1..=3u64 {
        let input = placeOrderCall {
            marketId: id,
            side: 0,
            price: P_LOW,
            quantity: QTY * id,
            orderType: 0,
            tif: 3,
            clientOrderId: FixedBytes::default(),
                        flags: 0,
        }
        .abi_encode();
        run_place_order(&input, ALICE, &mut ctx).unwrap();
        expect += 900_000u128 * id as u128;
    }
    assert_eq!(
        storage::load_user_markets(&mut ctx, ALICE).unwrap(),
        vec![1, 2, 3]
    );
    assert_eq!(
        total_open_order_initial_margin(&mut ctx, ALICE, None).unwrap(),
        expect
    );
    assert_eq!(expect, 5_400_000, "900_000 × (1 + 2 + 3)");
}
