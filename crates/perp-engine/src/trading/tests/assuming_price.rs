use super::*;
use crate::interface::IPerpDex::{liquidateCall, setLeverageCall};

/// The probe market: `base_decimals 5`, `price_decimals 2`, tick 10, step 1 — so
/// `calc_value(price, qty) == price × qty / 10` and every number below is exact.
fn setup_probe(ctx: &mut TestCtx) {
    storage::save_admin(ctx, ADMIN).unwrap();
    storage::save_market(
        ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 5,
            price_decimals: 2,
            tick_size: 10,
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
            mark_price: 100_000,
            tiers: MarginTiers::default(),
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
        },
    )
    .unwrap();
}

/// ALICE long 6 @ $1000 at leverage 3 (margin 20_000, wallet 0), mark then dropped to 85_000
/// with the position still comfortably above maintenance. Returns nothing — every caller
/// continues from this state.
fn alice_long_at_85k(ctx: &mut TestCtx) {
    setup_probe(ctx);
    fund(ctx, BOB, 1_000_000);
    fund(ctx, ALICE, 20_000);
    crate::risk::run_set_leverage(
        &setLeverageCall {
            marketId: MARKET_ID,
            leverage: 3,
        }
        .abi_encode(),
        ALICE,
        ctx,
    )
    .unwrap();

    // BOB rests the ask, ALICE lifts it. The fill PRINTS at 100_000, which is what makes
    // `lastTraded` 100_000 for the rest of the test.
    place(ctx, BOB, 1, 100_000, 6, 0, 0);
    place(ctx, ALICE, 0, 100_000, 6, 0, 0);
    let p = pos(ctx, ALICE);
    assert_eq!(
        (p.amount, p.margin, p.v_quote_balance),
        (6, 20_000, -60_000)
    );
    assert_eq!(wallet(ctx, ALICE), 0, "the whole wallet funded the margin");
    assert_eq!(
        storage::load_last_traded_price(ctx, MARKET_ID).unwrap(),
        100_000
    );

    storage::save_mark_price(ctx, MARKET_ID, 85_000).unwrap();
    // Still ABOVE maintenance: N = 51_000, equity = 51_000 − 60_000 + 20_000 = 11_000 against
    // a requirement of 51_000/6 = 8_500. The liquidation attempt below is the proof — the whole
    // scenario would be vacuous if the position were liquidatable.
    assert!(
        crate::risk::run_liquidate(
            &liquidateCall {
                user: ALICE,
                marketId: MARKET_ID,
            }
            .abi_encode(),
            BOB,
            ctx,
        )
        .is_err(),
        "the position must still be above maintenance"
    );
}

/// **THE HOLE THIS CLOSES.** Selling exactly 2× the position mirrors the exposure, so at the
/// LIMIT price `|N − Ask| == |N|` exactly and `IM == PIM ⇒ ooIM == 0`: a wallet of ZERO could
/// rest an order that flips the position, for free.
///
/// That band exists on Binance too — R7 got flipping sells accepted at `ooIM = 0E-8` — but
/// there it is NOT SIMULTANEOUSLY FILLABLE: a sell has to sit near the touch to fill, and near
/// the touch `Last × 1.0015` takes over the pricing and the markup bites. 「两者不可兼得」
/// (`binance-flip-and-admission.md` §1.6c). With limit-price aggregates and no markup, ours was
/// free AND fillable — a state Binance closes and we had opened. This is that state.
///
/// ```text
/// N = 51_000 (6 @ mark 85_000), leverage 3, PIM = ROUND_UP(51_000/3) = 17_000
/// T = max(ROUND_UP(100_000 × 1.0015), 85_000) = max(100_150, 85_000) = 100_150
///
/// at the LIMIT price      Ask = 12 × 85_000 /10 = 102_000 = 2|N| ⇒ IM = 17_000 ⇒ ooIM = 0
/// at the ASSUMING price   Ask = 12 × 100_150/10 = 120_180        ⇒ IM = ROUND_UP(69_180/3)
///                                                                    = 23_060 ⇒ ooIM = 6_060
/// ```
#[test]
fn a_sell_of_twice_the_position_is_no_longer_free() {
    // ── (a) at wallet 0 the order is now REFUSED ──
    let mut ctx = make_ctx();
    alice_long_at_85k(&mut ctx);
    assert_eq!(available(&mut ctx, ALICE), 0);
    let err = place_or_err(&mut ctx, ALICE, 1, 85_000, 12).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "{err}"
    );
    assert_eq!(oo_im(&mut ctx, ALICE), 0, "nothing rested");

    // ── (b) the requirement is EXACTLY 6_060: refused at 6_059, accepted at 6_060 ──
    let mut ctx = make_ctx();
    alice_long_at_85k(&mut ctx);
    fund(&mut ctx, ALICE, 6_059);
    assert!(place_or_err(&mut ctx, ALICE, 1, 85_000, 12).is_err());
    fund(&mut ctx, ALICE, 1);
    place(&mut ctx, ALICE, 1, 85_000, 12, 0, 0);

    // ── (c) and once it rests, the reported numbers are the Assuming-Price ones ──
    assert_eq!(oo_im(&mut ctx, ALICE), 6_060);
    assert_eq!(available(&mut ctx, ALICE), 0, "6_060 of 6_060 committed");
    let p = pos(&mut ctx, ALICE);
    assert_eq!(
        p.total_sell_notional, 120_180,
        "the maintained aggregate IS `Ask`: the markup is frozen INTO the entry at placement, \
         so there is no second (limit-price) basis to keep"
    );
    let i = margin_info(&mut ctx, ALICE);
    assert_eq!(
        i.askNotional, p.total_sell_notional,
        "and the reported `Ask` is that aggregate read back, not a re-fold"
    );
    assert_eq!(i.askNotional, 120_180);
    assert_eq!(
        storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID).unwrap()[0].price,
        85_000,
        "while the entry's LIMIT price — what matching, sorting and the fee key on — is \
         untouched at 85_000"
    );
    assert_eq!((i.notional, i.positionInitialMargin), (51_000, 17_000));
    assert_eq!(i.initialMargin, 23_060);
    assert_eq!(i.openOrderInitialMargin, 6_060);
}

/// The BOUNDARY, so the interior above is not the only thing recorded: a sell resting ABOVE
/// `T` freezes at its own limit price, the markup contributes nothing, and `Ask` equals the
/// limit-price fold — the one case where the two bases coincide.
///
/// Same position, sell 12 @ 150_000 (well above `T = 100_150`):
/// `Ask = 180_000`, `IM = ROUND_UP(|51_000 − 180_000| / 3) = 43_000`, `ooIM = 26_000`.
#[test]
fn a_sell_resting_above_the_assuming_floor_pays_exactly_its_own_notional() {
    let mut ctx = make_ctx();
    alice_long_at_85k(&mut ctx);
    fund(&mut ctx, ALICE, 26_000);
    place(&mut ctx, ALICE, 1, 150_000, 12, 0, 0);

    let p = pos(&mut ctx, ALICE);
    let i = margin_info(&mut ctx, ALICE);
    assert_eq!(p.total_sell_notional, 180_000);
    assert_eq!(
        i.askNotional, p.total_sell_notional,
        "above T the frozen price IS the limit price — no markup term at all"
    );
    assert_eq!(
        storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID).unwrap()[0].assuming_price,
        150_000,
        "and the frozen field records exactly that"
    );
    assert_eq!(i.initialMargin, 43_000);
    assert_eq!(oo_im(&mut ctx, ALICE), 26_000);
    assert_eq!(available(&mut ctx, ALICE), 0);
}

/// A resting BUY is never marked up (`Assuming Price = Order's Price` for a long order), so the
/// bid aggregate and `bidNotional` agree to the unit even with the buy resting BELOW `T` —
/// which is where a sell would be repriced. This is the asymmetry, pinned.
#[test]
fn the_buy_side_carries_no_markup() {
    let mut ctx = make_ctx();
    alice_long_at_85k(&mut ctx);
    // 6 @ 80_000 = 48_000 of notional; at leverage 3 the bid branch |N + Bid| = 99_000 gives
    // IM = ROUND_UP(99_000/3) = 33_000, so ooIM = 16_000. Priced at `T = 100_150` the same
    // order would have cost 60_090 of `Bid` and 3_030 more of ooIM.
    fund(&mut ctx, ALICE, 16_000);
    place(&mut ctx, ALICE, 0, 80_000, 6, 0, 0);

    let p = pos(&mut ctx, ALICE);
    let i = margin_info(&mut ctx, ALICE);
    assert_eq!(p.total_buy_notional, 48_000);
    assert_eq!(i.bidNotional, 48_000, "no markup on the buy side");
    assert_eq!(i.initialMargin, 33_000);
    assert_eq!(oo_im(&mut ctx, ALICE), 16_000);
    assert_eq!(available(&mut ctx, ALICE), 0);
}

/// **`bidNotional == Σ qty_i × P_i` EXACTLY** — the regression test
/// `misc/binance-flip-and-admission.md` §3.8 names for its own correction
/// (「`bidNotional` 应精确等于 `q_B × P_b`(与卖单侧对照,是 §3.7 那条更正的回归测试)」).
///
/// The single-order case above cannot distinguish "no markup" from "a markup that happens to
/// miss", so this walks MORE THAN ONE order and straddles `T` in both directions. The easy way
/// to break the asymmetry is to apply the sell side's `max(T, price)` to the buy fold as well;
/// that would silently lift the two below-`T` orders and leave the two others alone, which is
/// exactly what the per-order identity below catches.
///
/// `T = max(ROUND_UP(100_000 × 1.0015), 85_000) = 100_150`, and
/// `calc_value(price, qty) == price × qty / 10` on the probe market (`tick_size` is 10, so
/// every price below is a tick multiple):
///
/// ```text
///  price     qty   vs T       own notional    at max(T, price)   at min(T, price)
///  80_000      6   BELOW            48_000          60_090             48_000
/// 100_140      3   BELOW (1 tick)   30_042          30_045             30_042
/// 100_150      4   == T             40_060          40_060             40_060
/// 110_000      5   ABOVE            55_000          55_000             50_075
///                                 ────────        ────────           ────────
///                        Bid    =  173_102         185_195            168_177
///                                    ↑ the only right answer
/// ```
///
/// `N = 51_000`, `L = 3` ⇒ bid branch `|51_000 + 173_102| = 224_102`,
/// `IM = ROUND_UP(224_102/3) = 74_701`, `ooIM = 74_701 − 17_000 = 57_701`.
#[test]
fn the_bid_notional_is_exactly_quantity_times_each_orders_own_price() {
    let mut ctx = make_ctx();
    alice_long_at_85k(&mut ctx);
    fund(&mut ctx, ALICE, 1_000_000);

    // The three high bids are above the upper band edge at the $85_000 mark, so they can only
    // be REACHED by mark drift: a bid above the upper edge is a too-good quote and — bids
    // ranking better-first — necessarily the best bid, which placement now refuses. So place
    // them at a mark that contains them and then move the mark back.
    //
    // 100_000 is chosen, not arbitrary: it is the LOWEST mark whose upper edge (110_000)
    // contains the top bid, and it is still <= T, so T = max(ROUND_UP(lastTraded x 1.0015),
    // mark) = 100_150 is the SAME pivot the table above is written against. `save_mark_price`
    // rather than an oracle update, because an oracle update would run the band-expiry sweep.
    storage::save_mark_price(&mut ctx, MARKET_ID, 100_000).unwrap();
    let (upper_at_placement, _) = crate::math::mark_band_bounds(100_000, 0);
    assert_eq!(
        upper_at_placement, 110_000,
        "precondition: the placement band contains every bid below, top one inclusive"
    );
    assert_eq!(
        storage::load_last_traded_price(&mut ctx, MARKET_ID).unwrap(),
        100_000,
        "precondition: T's other input is unchanged, so T is still 100_150"
    );

    // Placed out of price order on purpose; the buy list is maintained price-DESCENDING.
    let orders = [(80_000u64, 6u64), (110_000, 5), (100_150, 4), (100_140, 3)];
    for (price, qty) in orders {
        place(&mut ctx, ALICE, 0, price, qty, 0, 0);
    }

    // DRIFT BACK: every number asserted below (N = 51_000 and everything derived from it) is on
    // the $85_000 basis this scenario is built around.
    storage::save_mark_price(&mut ctx, MARKET_ID, 85_000).unwrap();
    let (upper, lower) = crate::math::mark_band_bounds(85_000, 0);
    for high in [100_140u128, 100_150, 110_000] {
        assert!(
            high > upper,
            "the bid at {high} is now OUTSIDE the band [{lower}, {upper}] — this fixture's \
             book is unreachable by placement and only the drift above gets it there"
        );
    }

    let buys = storage::load_buy_orders(&mut ctx, ALICE, MARKET_ID).unwrap();
    assert_eq!(
        buys.iter().map(|e| e.price).collect::<Vec<_>>(),
        vec![110_000, 100_150, 100_140, 80_000],
        "all four rested, the buy list is price-descending, and it straddles T = 100_150 both \
         ways"
    );
    assert_eq!(
        buys.iter().map(|e| e.assuming_price).collect::<Vec<_>>(),
        vec![110_000, 100_150, 100_140, 80_000],
        "precondition: every buy entry froze its OWN price as its Assuming Price — the two \
         below T prove the straddle is real and that T did not collapse onto the prices"
    );

    // The identity, recomputed here from the order terms alone — deliberately NOT by calling an
    // engine helper, so this is a cross-check rather than a tautology.
    let want: u64 = orders.iter().map(|&(p, q)| p * q / 10).sum();
    assert_eq!(want, 48_000 + 55_000 + 40_060 + 30_042);
    assert_eq!(want, 173_102);

    let p = pos(&mut ctx, ALICE);
    let i = margin_info(&mut ctx, ALICE);
    assert_eq!(
        i.bidNotional, want,
        "bidNotional must be Σ qty × the order's OWN price, with no uplift anywhere"
    );
    assert_eq!(
        p.total_buy_notional, i.bidNotional,
        "for the buy side the maintained aggregate IS `Bid` — there is no second basis"
    );

    // Both plausible wrong rules are named, so a regression cannot pass by coincidence. The two
    // orders BELOW `T` are what the sell side's `max(T, price)` would lift; the one ABOVE `T` is
    // what a `min(T, price)` cap would lower. Neither may touch the buy fold.
    let floored_at_t: u64 = orders.iter().map(|&(p, q)| p.max(100_150) * q / 10).sum();
    let capped_at_t: u64 = orders.iter().map(|&(p, q)| p.min(100_150) * q / 10).sum();
    assert_eq!(floored_at_t, 185_195);
    assert_eq!(capped_at_t, 168_177);
    assert_ne!(
        i.bidNotional, floored_at_t,
        "the sell side's max(T, price) must NOT be applied to buys"
    );
    assert_ne!(i.bidNotional, capped_at_t, "and neither must a cap at T");
    assert_eq!(
        floored_at_t - i.bidNotional,
        12_093,
        "the exact amount the sell-side rule would have over-charged the buy side"
    );

    // ...and the requirement that follows from it.
    assert_eq!((i.notional, i.positionInitialMargin), (51_000, 17_000));
    assert_eq!(i.initialMargin, 74_701);
    assert_eq!(oo_im(&mut ctx, ALICE), 57_701);
}

/// The markup is PER ORDER — each entry freezes its own `max(T, price)`, not a single adjustment
/// to the total — and the sell list stays ASCENDING BY LIMIT PRICE, which is what the insert
/// predicate `partition_point(|e| e.price < price)` maintains and what matching, price priority
/// and `cancel_same_side_orders_until_wallet_covers`' `.back()` depend on. The markup no longer
/// walks that order (it is frozen per entry at placement), so this test also pins that the sort
/// is keyed on `price` and NOT on the marked-up `assuming_price` — under which the list below
/// would come out `[100_150, 100_150, 100_150, 150_000]` and the first three would be
/// interchangeable.
///
/// Four sells of 4 straddling `T = 100_150`:
///
/// ```text
///  limit    frozen assuming price
///  90_000 → 100_150 (lifted to T)     100_000 → 100_150 (lifted to T)
/// 100_150 → 100_150 (exactly T)       150_000 → 150_000 (above T)
///
/// Ask = 4·(100_150 + 100_150 + 100_150 + 150_000) / 10 = 180_180   ← the aggregate, frozen
/// ooIM = ROUND_UP(|51_000 − 180_180| / 3) − 17_000 = 43_060 − 17_000 = 26_060
///        (at the limit price it would have been 41_687 − 17_000 = 24_687)
/// ```
#[test]
fn the_markup_is_frozen_per_order_and_never_reorders_the_list() {
    let mut ctx = make_ctx();
    alice_long_at_85k(&mut ctx);
    fund(&mut ctx, ALICE, 1_000_000);
    // Placed out of order on purpose: the list must end up ascending regardless.
    for price in [150_000u64, 90_000, 100_150, 100_000] {
        place(&mut ctx, ALICE, 1, price, 4, 0, 0);
    }
    let sells = storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID).unwrap();
    assert_eq!(
        sells.iter().map(|e| e.price).collect::<Vec<_>>(),
        vec![90_000, 100_000, 100_150, 150_000],
        "ascending by LIMIT price — the sort matching and `.back()` rely on"
    );
    assert_eq!(
        sells.iter().map(|e| e.assuming_price).collect::<Vec<_>>(),
        vec![100_150, 100_150, 100_150, 150_000],
        "each entry froze its own max(T, limit) — three of them collide, which is exactly why \
         the sort must not key on this field"
    );

    let p = pos(&mut ctx, ALICE);
    assert_eq!(
        p.total_sell_notional, 180_180,
        "the aggregate IS `Ask`, at the frozen prices"
    );
    assert_eq!(margin_info(&mut ctx, ALICE).askNotional, 180_180);
    assert_eq!(
        oo_im(&mut ctx, ALICE),
        26_060,
        "1_373 more than the 24_687 the limit-price basis would charge"
    );
}

/// `lastTraded == 0` (nothing has ever printed) collapses `T` to the mark — the `Mark` branch
/// of the vendor `max()`, so no separate fallback is needed. A sell above the mark is then
/// unmarked, and one below it is charged AT THE MARK.
#[test]
fn before_the_first_trade_the_floor_is_the_mark() {
    let mut ctx = make_ctx();
    setup_probe(&mut ctx);
    fund(&mut ctx, ALICE, 1_000_000);
    assert_eq!(
        storage::load_last_traded_price(&mut ctx, MARKET_ID).unwrap(),
        0,
        "nothing has traded"
    );

    // Above the 100_000 mark: unmarked.
    place(&mut ctx, ALICE, 1, 110_000, 3, 0, 0);
    assert_eq!(margin_info(&mut ctx, ALICE).askNotional, 33_000);

    // A second sell BELOW the mark is lifted to the mark: 3 × 100_000/10 = 30_000 instead of
    // 3 × 90_000/10 = 27_000.
    place(&mut ctx, ALICE, 1, 90_000, 3, 0, 0);
    assert_eq!(margin_info(&mut ctx, ALICE).askNotional, 33_000 + 30_000);
    assert_eq!(
        pos(&mut ctx, ALICE).total_sell_notional,
        33_000 + 30_000,
        "the aggregate carries the frozen lift, and is `Ask` itself"
    );
    assert_eq!(
        storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID)
            .unwrap()
            .iter()
            .map(|e| (e.price, e.assuming_price))
            .collect::<Vec<_>>(),
        vec![(90_000, 100_000), (110_000, 110_000)],
        "the below-mark sell froze at the mark; the above-mark one at its own limit"
    );
}

/// **R12, the freeze itself.** Once a sell rests, its `Ask` term never moves again — not when
/// the mark walks, not when a later trade prints, not when the floor crosses its own limit
/// price. `misc/binance-flip-and-admission.md` §3.13: 3 trials × 30 frames × 2 s, the reported
/// `askNotional` never moved while `Last` walked 32.80 USD, with **9 consecutive frames below
/// the `P_s / 1.0015` kink** where a recomputed-at-read value has to plateau at `q × P_s`.
/// `H_live` was refused by 1939 quanta — a SHAPE-level refutation, not a slope-level one.
///
/// This is the on-chain analogue of that walk, and it is built to be discriminating in the same
/// way: a sell rests at 85_000 with `T = 100_150`, freezing at 100_150, and then the mark is
/// walked from 85_000 all the way to 200_000 — right through and far past the frozen price. A
/// re-resolving implementation would report `Ask = 12 × mark / 10` on the later frames
/// (240_000 at mark 200_000); the frozen one reports 120_180 on every frame.
///
/// ⚠️ And the other half, which must NOT be broken by the freeze: `ooIM` still MOVES, because
/// `N = |qty| × mark` is live. Both are asserted here so neither can be "fixed" into the other.
#[test]
fn a_resting_sells_ask_term_is_frozen_while_ooim_still_moves_with_the_mark() {
    let mut ctx = make_ctx();
    alice_long_at_85k(&mut ctx);
    fund(&mut ctx, ALICE, 6_060);
    place(&mut ctx, ALICE, 1, 85_000, 12, 0, 0);
    // Frozen at T = max(ROUND_UP(100_000 × 1.0015), 85_000) = 100_150 ⇒ 12 × 100_150/10.
    assert_eq!(margin_info(&mut ctx, ALICE).askNotional, 120_180);

    // Fund the walk so a rising requirement is never the thing that reverts (nothing writes
    // here anyway — `save_mark_price` is the only mutation).
    fund(&mut ctx, ALICE, 10_000_000);

    let mut asks = Vec::new();
    let mut oo_ims = Vec::new();
    for mark in [85_000u64, 95_000, 100_150, 120_000, 200_000] {
        storage::save_mark_price(&mut ctx, MARKET_ID, mark).unwrap();
        asks.push(margin_info(&mut ctx, ALICE).askNotional);
        oo_ims.push(oo_im(&mut ctx, ALICE));
    }

    // (a) THE FREEZE: byte-identical on every frame, including the three where the mark is at or
    //     above the frozen 100_150 and a live rule would have taken over.
    assert_eq!(
        asks,
        vec![120_180; 5],
        "a resting sell's Ask term must not move with the mark (R12)"
    );
    assert_ne!(
        asks.last(),
        Some(&240_000),
        "…and specifically not to 12 × mark / 10, which is what re-resolving T would give"
    );
    // Also unmoved by a fresh PRINT that lifts the floor past the frozen price: BOB and CAROL
    // trade at 200_000, so `T` becomes 200_300 — nothing to do with ALICE's resting order.
    fund(&mut ctx, CAROL, 10_000_000);
    place(&mut ctx, BOB, 0, 200_000, 1, 0, 0);
    place(&mut ctx, CAROL, 1, 200_000, 1, 0, 0);
    assert_eq!(
        storage::load_last_traded_price(&mut ctx, MARKET_ID).unwrap(),
        200_000,
        "the print landed"
    );
    assert_eq!(
        margin_info(&mut ctx, ALICE).askNotional,
        120_180,
        "a later print does not re-freeze an already-resting order"
    );

    // (b) `N` IS STILL LIVE, so ooIM moves — the frozen aggregate did not freeze the
    //     requirement. Long 6 at leverage 3 against a frozen Ask of 120_180:
    //       mark  85_000: N = 51_000, IM = ⌈(120_180−51_000)/3⌉ = 23_060, PIM = 17_000 ⇒  6_060
    //       mark  95_000: N = 57_000, IM = ⌈(120_180−57_000)/3⌉ = 21_060, PIM = 19_000 ⇒  2_060
    //       mark 100_150: N = 60_090, IM = ⌈(120_180−60_090)/3⌉ = 20_030, PIM = 20_030 ⇒      0
    //       mark 120_000: N = 72_000, bid branch |72_000| wins ⇒ IM = PIM              ⇒      0
    //       mark 200_000: N = 120_000, same                                            ⇒      0
    assert_eq!(oo_ims, vec![6_060, 2_060, 0, 0, 0]);
    assert!(
        oo_ims[0] != oo_ims[1],
        "ooIM must still re-value on a mark move with no user action (R10) — the freeze is \
         per-order, it is NOT 'the escrow is a constant' (that holds only at N = 0)"
    );
}

// ── local helpers ────────────────────────────────────────────────────────────────────────

fn place_or_err(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8,
    price: u64,
    qty: u64,
) -> Result<Bytes, PerpError> {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
                flags: 0,
    }
    .abi_encode();
    run_place_order(&input, caller, ctx)
}

/// The reported margin row for `(user, MARKET_ID)`, in ABI form.
///
/// Goes through `AccountPositionRow::to_abi` — the ONE row encoder both `getPositionRisk` and
/// `getAccount().positions[]` use. It used to hand-fill a `getMarginInfoReturn` field by field,
/// which made it a fourth encoder of the same numbers living in a test module; when
/// `getMarginInfo` was deleted that literal was the only thing still naming it.
fn margin_info(
    ctx: &mut TestCtx,
    user: Address,
) -> crate::interface::IPerpDex::AccountPosition {
    crate::margin::view::AccountPositionRow {
        market_id: MARKET_ID,
        info: crate::margin::view::compute_margin_info(ctx, user, MARKET_ID).unwrap(),
    }
    .to_abi()
}
