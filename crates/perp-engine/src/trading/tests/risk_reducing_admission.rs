use super::*;

/// $110 — one QTY lot is worth 1_100_000 here (calc_value(110e9, 1e6, 8, 9)).
const P_HIGH: u64 = 110 * TICK;
/// $120 — one QTY lot is worth 1_200_000.
const P_HIGHER: u64 = 120 * TICK;
/// $105 — one QTY lot is worth 1_050_000.
const P_MID: u64 = 105 * TICK;
/// $90 — one QTY lot is worth 900_000.
const P_LOW: u64 = 90 * TICK;

/// Raw SIGNED wallet balance. The `wallet()` helper reports
/// `visible_perp_wallet_balance()`, which clamps negatives to 0 — useless here.
fn raw_wallet(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_account(ctx, user).unwrap().perp_wallet_balance
}

/// Overwrite the signed perp wallet directly (same pattern as the existing
/// `alice.perp_wallet_balance = …` tests). A negative value is REACHABLE in
/// production — a close-path fee or funding charge can drive it below zero.
fn set_raw_wallet(ctx: &mut TestCtx, user: Address, v: i64) {
    let mut a = storage::load_account(ctx, user).unwrap();
    a.perp_wallet_balance = v;
    storage::save_account(ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
}

/// Seed a flat long of `lots` QTY-sized lots opened at PRICE (margin at leverage 1).
/// `setup()` plus a live mark at $100 — see the module header for why every test needs it.
fn setup_marked(ctx: &mut TestCtx) {
    setup(ctx);
    set_mark(ctx, PRICE);
}

fn seed_long(ctx: &mut TestCtx, user: Address, lots: i64) {
    let notional = lots * FILL_VALUE as i64;
    storage::save_position(
        ctx,
        user,
        MARKET_ID,
        &PerpPosition {
            amount: lots * QTY as i64,
            v_quote_balance: -notional,
            margin: notional,
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

fn try_place(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
) -> Result<Bytes, PerpError> {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
                flags: 0,
    }
    .abi_encode();
    run_place_order(&input, caller, ctx)
}

fn cancel(ctx: &mut TestCtx, caller: Address, id: [u8; 32]) -> Result<Bytes, PerpError> {
    let input = cancelOrderCall {
        orderId: id.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    run_cancel_order(&input, caller, ctx)
}

// ── B1 ─────────────────────────────────────────────────────────────────

/// REGRESSION (B1). Pinned bug: `has_available_perp(0)` evaluated `-5 >= 0`
/// == `false`, so a NEGATIVE perp wallet refused a debit of ZERO. This
/// placement reserves nothing (pure reduce ⇒ delta 0) yet was rejected with
/// "insufficient perp wallet for margin" — locking a negative-balance user
/// out of exactly the order that would REDUCE their risk.
#[test]
fn b1_negative_wallet_admits_a_zero_delta_reduce_only_rest() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx);
    seed_long(&mut ctx, ALICE, 2);
    set_raw_wallet(&mut ctx, ALICE, -5);

    let ret = try_place(&mut ctx, ALICE, 1, P_HIGH, QTY, 0, 0)
        .expect("a zero-cost risk-reducing order must be admitted on a negative wallet");
    let id: [u8; 32] = ret[..32].try_into().unwrap();

    assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open);
    // N = +2e6 (2 lots at mark $100), Ask = 1.1e6 ⇒
    // IM = max(|2e6 + 0|, |2e6 − 1.1e6|) = 2e6 = PIM ⇒ ooIM = 0. Same answer the escrow gave.
    assert_eq!(
        oo_im(&mut ctx, ALICE),
        0,
        "a sell fully covered by the long adds no exposure"
    );
    assert_eq!(
        raw_wallet(&mut ctx, ALICE),
        -5,
        "a zero debit must leave the wallet untouched"
    );
}

/// REGRESSION (B1), the sharper half: the CLOSE path, which is gated by a
/// `has_available_perp` site the audit's list of six does NOT contain —
/// `finalize_compute` in `trading/settlement.rs`.
///
/// A pure close with a zero taker fee has `total_required == 0`
/// (`opening_margin + fee_from_wallet + mr_extra`, all zero). The gate is
/// evaluated on the POST-fill working copy, so a shallow deficit is masked:
/// the closing cashflow lifts the wallet positive before the check and the
/// close goes through even pre-fix. The bug only bites when the deficit is
/// DEEPER than the margin the close releases — which is exactly the user who
/// most needs to de-risk. Here: wallet −2_000_000, a 1-lot long releasing
/// 1_000_000, so the post-fill wallet is still −1_000_000 and
/// `has_available_perp(0)` returned false. Pre-fix that dropped into the
/// wallet-cover branch, found no same-side order to cancel, and rejected the
/// close with "insufficient perp wallet for margin" — the deepest-underwater
/// user was the one locked out of closing.
#[test]
fn b1_deeply_negative_wallet_user_can_still_close_a_position() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx);
    seed_long(&mut ctx, ALICE, 1);
    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0); // BOB rests a bid at $100
                                               // Deficit deeper than the 1_000_000 the close will release.
    set_raw_wallet(&mut ctx, ALICE, -2_000_000);

    try_place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 1)
        .expect("closing a position must never be blocked by a negative wallet");

    assert_eq!(pos(&mut ctx, ALICE).amount, 0, "position closed");
    assert_eq!(
        raw_wallet(&mut ctx, ALICE),
        -2_000_000 + INIT_MARGIN as i64,
        "the released position margin reduces the deficit; still negative, still allowed"
    );
}

/// REGRESSION (B1) on the audit's missed site, and the ugliest shape of it.
/// `finalize_compute`'s cover branch runs a SIMULATED LIFO cancel loop,
/// `while !sim_account.has_available_perp(core.total_required)`, which a
/// negative wallet entered even at `total_required == 0`. Here the resting
/// sell is fully covered by the long, so it reserves NOTHING: cancelling it
/// in-sim credits 0, the wallet stays negative, the list is exhausted and the
/// close was REJECTED — the user was told to liquidate orders that could not
/// possibly help. (Had the order reserved something the sim could instead
/// have "succeeded" while the apply half, guarded by
/// `ensure_taker_wallet_can_cover_margin`'s own `required_margin == 0` early
/// return, performed no real cancel — the two halves disagreeing.)
/// Post-fix both halves take the zero fast path: the close is admitted and
/// the resting order is untouched. (Under the derived basis the cover loop's
/// guard is `derived_can_afford(available, 0)`, which is unconditionally true —
/// the same fast path, restated.)
#[test]
fn b1_zero_cost_close_does_not_disturb_resting_same_side_orders() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx);
    seed_long(&mut ctx, ALICE, 2);
    // A resting sell fully covered by the long: costs nothing, and is the
    // LIFO victim the sim cover loop would have reached for.
    let resting = place(&mut ctx, ALICE, 1, P_HIGHER, QTY, 0, 0);
    assert_eq!(oo_im(&mut ctx, ALICE), 0);
    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0); // BOB rests a bid at $100
    set_raw_wallet(&mut ctx, ALICE, -5_000_000); // deeper than anything released

    try_place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 1).expect("zero-cost close admitted");

    assert_eq!(
        pos(&mut ctx, ALICE).amount,
        QTY as i64,
        "1 of 2 lots closed"
    );
    assert_eq!(
        get_order(&mut ctx, resting).status,
        OrderStatus::Open,
        "the resting same-side order must survive a zero-cost close"
    );
}

/// Companion to the above: the SHALLOW-deficit close was already fine pre-fix
/// (the closing cashflow lifts the working copy positive before the gate).
/// Pinned so the two cases are not conflated.
#[test]
fn b1_shallow_negative_wallet_close_was_already_admitted() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx);
    seed_long(&mut ctx, ALICE, 1);
    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0);
    set_raw_wallet(&mut ctx, ALICE, -5);

    try_place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 1).expect("close admitted");

    assert_eq!(pos(&mut ctx, ALICE).amount, 0);
    assert_eq!(raw_wallet(&mut ctx, ALICE), INIT_MARGIN as i64 - 5);
}

/// B1 companion: the cancel path has NO balance gate by design. Pinning that
/// so nobody "helpfully" adds one.
///
/// CHANGED BY THE ESCROW REMOVAL: a cancel no longer CREDITS the wallet (it used to return
/// the 900_000 reservation, so the wallet went `-5` → `899_995`). It moves no money at all;
/// what it returns is HEADROOM, by dropping this market's `Bid` to 0 and with it the
/// requirement. Both halves are asserted.
#[test]
fn b1_negative_wallet_user_can_still_cancel() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx);
    let id = place(&mut ctx, ALICE, 0, P_LOW, QTY, 0, 0);
    assert_eq!(oo_im(&mut ctx, ALICE), 900_000);
    set_raw_wallet(&mut ctx, ALICE, -5);
    assert_eq!(super::available(&mut ctx, ALICE), -900_005);

    cancel(&mut ctx, ALICE, id).expect("cancel is ungated and must stay ungated");

    assert_eq!(oo_im(&mut ctx, ALICE), 0);
    assert_eq!(
        raw_wallet(&mut ctx, ALICE),
        -5,
        "the wallet does not move — there was never anything escrowed to give back"
    );
    assert_eq!(
        super::available(&mut ctx, ALICE),
        -5,
        "but the 900_000 of headroom the order was holding is released"
    );
}

/// B1 must NOT open a funding hole: a genuinely margin-requiring order is
/// still refused on a negative wallet, and the ≥ boundary is unchanged.
#[test]
fn b1_negative_wallet_still_refuses_a_nonzero_debit() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx);
    set_raw_wallet(&mut ctx, ALICE, -5);

    let err = try_place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "{err}"
    );
    assert_eq!(oo_im(&mut ctx, ALICE), 0, "reject wrote nothing");

    // Boundary is untouched: one unit short still fails, exactly enough passes.
    // (Flat position ⇒ N = 0 ⇒ the requirement is the full 1e6 notional at leverage 1.)
    set_raw_wallet(&mut ctx, ALICE, INIT_MARGIN as i64 - 1);
    assert!(try_place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0).is_err());
    set_raw_wallet(&mut ctx, ALICE, INIT_MARGIN as i64);
    try_place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0).expect("exactly enough must pass");
    // CHANGED: the wallet is no longer debited, so it is the AVAILABLE that lands on 0.
    assert_eq!(raw_wallet(&mut ctx, ALICE), INIT_MARGIN as i64);
    assert_eq!(super::available(&mut ctx, ALICE), 0);
}

// ── B3 ① pure reduce ───────────────────────────────────────────────────

/// B3 ①: a sell against a long adds no EXPOSURE, so it adds no requirement. At mark $100 with
/// a 2-lot long, `N = 2e6`; one lot sold at $110 gives `Ask = 1.1e6` and
/// `IM = max(|2e6|, |2e6 − 1.1e6|) = 2e6 = PIM ⇒ ooIM = 0`; the second lot at $120 takes
/// `Ask` to 2.3e6 and `IM = max(2e6, 3e5)` is still 2e6 ⇒ ooIM still 0.
/// Admitted at a POSITIVE, ZERO and NEGATIVE wallet alike.
///
/// VERDICT UNCHANGED by the migration — the escrow reached 0 here too (by a different route:
/// its per-side cover scan found every entry covered). ① was already fine.
#[test]
fn b3_case1_pure_reduce_is_zero_delta_at_positive_zero_and_negative_balance() {
    for balance in [WALLET as i64, 0i64, -5i64] {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        seed_long(&mut ctx, ALICE, 2);
        set_raw_wallet(&mut ctx, ALICE, balance);

        // Partial reduce: 1 lot against a 2-lot long.
        try_place(&mut ctx, ALICE, 1, P_HIGH, QTY, 0, 0)
            .unwrap_or_else(|e| panic!("partial reduce refused at balance {balance}: {e}"));
        assert_eq!(oo_im(&mut ctx, ALICE), 0);
        assert_eq!(raw_wallet(&mut ctx, ALICE), balance, "delta == 0");

        // Full reduce: the second lot, still exactly covered by the long.
        try_place(&mut ctx, ALICE, 1, P_HIGHER, QTY, 0, 0)
            .unwrap_or_else(|e| panic!("full reduce refused at balance {balance}: {e}"));
        let p = pos(&mut ctx, ALICE);
        assert_eq!(p.total_sell_qty, QTY * 2);
        assert_eq!(
            oo_im(&mut ctx, ALICE),
            0,
            "aggregate sells still net inside the long"
        );
        assert_eq!(raw_wallet(&mut ctx, ALICE), balance, "delta == 0");
    }
}

// ── B3 ② flip ──────────────────────────────────────────────────────────

/// B3 ②: a sell LARGER than the long genuinely opens a short, so it costs something —
/// but far less than the escrow charged.
///
/// CHANGED BY THE DERIVED-ooIM SWITCH: requirement **1_100_000 → 200_000**. Long 1 lot at
/// mark $100 (`N = 1e6`), sell 2 lots at $110 (`Ask = 2.2e6`):
///   `IM  = ROUND_UP(max(|1e6 + 0|, |1e6 − 2.2e6|) / 1) = 1.2e6`
///   `PIM = 1e6`  ⇒  `ooIM = 200_000`
/// The escrow charged the excess lot's whole opening notional (1 lot @ $110 = 1_100_000) and
/// netted nothing, because it priced the two sides separately in QUANTITY space. Binance's
/// joint `max()` measures the exposure that would REMAIN after the flip — a 1.2e6 short
/// against a 1e6 long already margined — so it charges only the 200_000 of extra exposure.
/// The old comment called crediting the released position margin "A3, a separate product
/// decision"; adopting Binance's formula settles it.
///
/// The SHAPE is unchanged: fully deployed ⇒ refused; one unit short ⇒ refused; exactly the
/// requirement ⇒ admitted. Only the threshold moved.
#[test]
fn b3_case2_flip_is_charged_only_the_residual_exposure_not_the_whole_opening_leg() {
    // Fully deployed ⇒ refused.
    let mut ctx = make_ctx();
    setup_marked(&mut ctx);
    seed_long(&mut ctx, ALICE, 1);
    set_raw_wallet(&mut ctx, ALICE, 0);
    let err = try_place(&mut ctx, ALICE, 1, P_HIGH, QTY * 2, 0, 0).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "{err}"
    );
    assert_eq!(oo_im(&mut ctx, ALICE), 0, "reject wrote nothing");

    // One unit short of the requirement: still refused.
    set_raw_wallet(&mut ctx, ALICE, 200_000 - 1);
    assert!(try_place(&mut ctx, ALICE, 1, P_HIGH, QTY * 2, 0, 0).is_err());

    // Exactly the residual exposure is admitted.
    set_raw_wallet(&mut ctx, ALICE, 200_000);
    try_place(&mut ctx, ALICE, 1, P_HIGH, QTY * 2, 0, 0)
        .expect("admitted at exactly the derived requirement");
    let p = pos(&mut ctx, ALICE);
    assert_eq!(p.total_sell_notional, 2_200_000, "Ask");
    assert_eq!(oo_im(&mut ctx, ALICE), 200_000);
    assert_eq!(raw_wallet(&mut ctx, ALICE), 200_000, "nothing was debited");
    assert_eq!(super::available(&mut ctx, ALICE), 0);
    assert_eq!(
        p.margin, 1_000_000,
        "the long's own margin is untouched — it is NETTED in the requirement, not moved"
    );
}

// ── B3 ③ reduce with other resting orders ──────────────────────────────

/// B3 ③a: an OPPOSITE-side resting order does NOT change the answer.
///
/// Long 2 lots (`N = 2e6`) with a buy of 1 lot @ $90 resting (`Bid = 900_000`):
/// `IM = max(|2e6 + 9e5|, |2e6|) = 2.9e6`, `PIM = 2e6` ⇒ `ooIM = 900_000`, the same number
/// the escrow held. Adding a reducing sell of 1 lot @ $110 (`Ask = 1.1e6`) leaves the BID
/// branch winning at 2.9e6, so `ooIM` is unchanged ⇒ Δ = 0 and it is free.
///
/// VERDICT UNCHANGED by the migration. "Fully deployed" is now expressed as AVAILABLE = 0.
#[test]
fn b3_case3a_opposite_side_resting_order_does_not_block_a_reduce() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx);
    seed_long(&mut ctx, ALICE, 2);

    // Every buy OPENS on top of a long, so this costs its full notional.
    place(&mut ctx, ALICE, 0, P_LOW, QTY, 0, 0);
    assert_eq!(oo_im(&mut ctx, ALICE), 900_000);

    set_available(&mut ctx, ALICE, 0); // fully deployed

    try_place(&mut ctx, ALICE, 1, P_HIGH, QTY, 0, 0)
        .expect("a reduce must be admitted despite a resting opposite-side order");
    assert_eq!(
        oo_im(&mut ctx, ALICE),
        900_000,
        "UNCHANGED — the bid branch still wins"
    );
    assert_eq!(super::available(&mut ctx, ALICE), 0, "delta == 0");
}

/// B3 ③b: **FLIPS FROM REJECT TO ACCEPT.** This is the audit claim ("Binance ADMITS, we
/// REJECT: a partly-closing sell on a long") resolved rather than characterised.
///
/// Long 2 lots (`N = 2e6`). A first sell of 2 lots @ $110 exactly covers it; a second sell of
/// 1 lot @ $120 takes the AGGREGATE to 3 lots against a 2-lot long — a genuine 1-lot short in
/// QUANTITY space, which is what the escrow charged for (1_200_000, the excess lot at the
/// dearest price). In NOTIONAL space at mark it is not:
///   `Ask = 2.2e6 + 1.2e6 = 3.4e6`, `IM = max(|2e6|, |2e6 − 3.4e6|) = max(2e6, 1.4e6) = 2e6`
///   `= PIM` ⇒ `ooIM = 0`.
/// If every sell filled, the account would hold 1.4e6 of SHORT exposure — strictly less than
/// the 2e6 of LONG exposure it is already margined for. No additional initial margin is
/// required, and Binance charges none.
///
/// EXTRA SCRUTINY (this admits an order we used to refuse): the loosening is bounded and
/// guarded. `pos.margin` still fully backs the existing 2-lot long; the sells' own opening
/// margin is charged at FILL time out of the wallet (which is allowed to go negative if it
/// cannot cover it — Binance never sweeps an under-covered lien); and K9 still refuses any fill
/// that would leave the resulting position below maintenance. What is no longer charged is a
/// worst-case exposure SMALLER than the one already margined — which was never a risk.
#[test]
fn b3_case3b_same_side_resting_orders_that_stay_inside_the_long_are_free() {
    let mut ctx = make_ctx();
    setup_marked(&mut ctx);
    seed_long(&mut ctx, ALICE, 2);

    // First sell exactly covers the long ⇒ costs nothing.
    place(&mut ctx, ALICE, 1, P_HIGH, QTY * 2, 0, 0);
    assert_eq!(oo_im(&mut ctx, ALICE), 0);

    set_available(&mut ctx, ALICE, 0); // fully deployed

    // The escrow REFUSED this (it wanted 1_200_000). The derived basis admits it for free.
    try_place(&mut ctx, ALICE, 1, P_HIGHER, QTY, 0, 0)
        .expect("aggregate short exposure 1.4e6 < the 2e6 long already margined");
    let p = pos(&mut ctx, ALICE);
    assert_eq!(p.total_sell_qty, QTY * 3);
    assert_eq!(p.total_sell_notional, 3_400_000, "Ask");
    assert_eq!(oo_im(&mut ctx, ALICE), 0);
    assert_eq!(super::available(&mut ctx, ALICE), 0, "nothing was charged");

    // The boundary is real, not vacuous: one more lot at $120 takes Ask to 4.6e6, the ask
    // branch overtakes (|2e6 − 4.6e6| = 2.6e6 > 2e6) and the excess IS charged.
    let err = try_place(&mut ctx, ALICE, 1, P_HIGHER, QTY, 0, 0).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "{err}"
    );
    let w = raw_wallet(&mut ctx, ALICE);
    set_raw_wallet(&mut ctx, ALICE, w + 600_000);
    try_place(&mut ctx, ALICE, 1, P_HIGHER, QTY, 0, 0)
        .expect("2.6e6 − 2e6 = 600_000 is exactly the marginal requirement");
    assert_eq!(oo_im(&mut ctx, ALICE), 600_000);
    assert_eq!(super::available(&mut ctx, ALICE), 0);
}

/// B3 ③c: **THE PHENOMENON THIS TEST PINNED NO LONGER EXISTS.**
///
/// It used to record that once the aggregate over-covers, the sell side is scanned ASC, so
/// the CHEAPEST sells absorb the position's cover and the dearest are pushed into the opening
/// bucket — meaning a reduce-only order could be charged at ANOTHER order's price, and more
/// than its own notional was worth. That was an artefact of a per-order cover scan.
///
/// `ooIM` has no such scan: it is a function of `(N, Bid, Ask, L)` alone, and `Bid`/`Ask` are
/// plain sums. So the requirement cannot depend on which order is "the new one", and cannot
/// depend on the ORDER in which two orders were placed. That is what this test now pins —
/// the property that replaced the anomaly.
#[test]
fn b3_case3c_the_requirement_is_order_independent_not_priced_at_a_particular_order() {
    // Place the $110 order first, then the $105 one ...
    let mut a = make_ctx();
    setup_marked(&mut a);
    seed_long(&mut a, ALICE, 2);
    place(&mut a, ALICE, 1, P_HIGH, QTY * 2, 0, 0); // 2 lots @ $110
    assert_eq!(oo_im(&mut a, ALICE), 0);
    try_place(&mut a, ALICE, 1, P_MID, QTY, 0, 0).expect("admitted");

    // ... and the other way round.
    let mut b = make_ctx();
    setup_marked(&mut b);
    seed_long(&mut b, ALICE, 2);
    place(&mut b, ALICE, 1, P_MID, QTY, 0, 0); // 1 lot @ $105
    try_place(&mut b, ALICE, 1, P_HIGH, QTY * 2, 0, 0).expect("admitted");

    // Same book, same requirement, whichever order was "the new one".
    assert_eq!(pos(&mut a, ALICE).total_sell_notional, 3_250_000);
    assert_eq!(
        pos(&mut b, ALICE).total_sell_notional,
        pos(&mut a, ALICE).total_sell_notional
    );
    assert_eq!(oo_im(&mut b, ALICE), oo_im(&mut a, ALICE));
    // And the value: Ask = 2.2e6 + 1.05e6 = 3.25e6, |2e6 − 3.25e6| = 1.25e6 < |N| = 2e6,
    // so the bid branch still wins and NOTHING is charged. Under the escrow this book cost
    // 1_100_000 — "charged at $110, more than the $105 order's own notional".
    assert_eq!(oo_im(&mut a, ALICE), 0);
}

// ── The admission gate's case table, all four rows explicitly ──────────

/// `(Δ ooIM, available(after))` for the hypothetical "rest `qty` at `price` on `side`" — the
/// two quantities `rest_in_book`'s gate is a function of, computed here the way the gate
/// computes them (`margin::view`, same helpers, same overrides) so the table below indexes the
/// real inputs and not a paraphrase.
fn gate_inputs(
    ctx: &mut TestCtx,
    user: Address,
    side: u8,
    price: u64,
    qty: u64,
) -> (i128, i128) {
    let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    let before = storage::load_position(ctx, user, MARKET_ID).unwrap();
    let mut after = before.clone();
    // A buy's Assuming Price IS its limit price; a sell's is `max(T, limit)`, frozen now.
    let assuming = if side == 0 {
        price
    } else {
        price.max(crate::margin::view::assuming_price_floor(ctx, MARKET_ID, &market).unwrap())
    };
    let notional =
        crate::math::calc_value(assuming, qty, market.base_decimals, market.price_decimals)
            .unwrap();
    if side == 0 {
        after.total_buy_qty += qty;
        after.total_buy_notional += notional;
    } else {
        after.total_sell_qty += qty;
        after.total_sell_notional += notional;
    }
    let delta =
        crate::margin::view::derived_requirement_delta(&market, &before, &after).unwrap();
    let available_after = crate::margin::view::derived_available_balance_with(
        ctx,
        user,
        None,
        Some((MARKET_ID, &after)),
    )
    .unwrap();
    (delta, available_after)
}

/// **The full case table `rest_in_book`'s admission gate is defined over**, one row per cell,
/// each row pinned by asserting BOTH gate inputs and then the verdict:
///
/// ```text
///   Δ ooIM │ available(after) │ verdict
///   ───────┼──────────────────┼─────────
///     ≤ 0  │       ≥ 0        │ ACCEPT
///     ≤ 0  │       < 0        │ ACCEPT   ← the B1 escape; the ONLY row that needs Δ
///     > 0  │       ≥ 0        │ ACCEPT
///     > 0  │       < 0        │ REJECT
/// ```
///
/// The gate evaluates `available(after) ≥ 0` first and consults `Δ ooIM`'s SIGN only on the
/// negative branch, so rows 1/3 must be indistinguishable to it (they are: both accept without
/// the delta) and rows 2/4 must be separated by the sign alone. Delete the `Δ > 0` guard and
/// row 2 flips to REJECT — that is the mutation this test exists to catch.
///
/// Every row rests via PostOnly, so no matching runs and the gate under test is exactly
/// `rest_in_book`'s (the taker path has its own, in `settlement.rs`).
///
/// Note `Δ ooIM < 0` is unreachable HERE — `max(|N + Bid|, |N − Ask|)` is non-decreasing in
/// each aggregate at fixed `N` — so the `≤ 0` rows are `Δ == 0`, asserted as such.
#[test]
fn admission_gate_case_table_over_delta_sign_and_post_availability() {
    const POST_ONLY: u8 = 3;

    // ── Row 1: Δ == 0, available(after) ≥ 0 ⇒ ACCEPT ──
    // A 2-lot long at mark $100 (N = 2e6) and a 1-lot sell at $110 (Ask = 1.1e6):
    // IM = max(2e6, |2e6 − 1.1e6|) = 2e6 = PIM ⇒ ooIM unchanged. Wallet is the funded 10e6.
    {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        seed_long(&mut ctx, ALICE, 2);
        assert_eq!(
            gate_inputs(&mut ctx, ALICE, 1, P_HIGH, QTY),
            (0, 10_000_000),
            "row 1 inputs: zero delta, positive post-availability"
        );
        try_place(&mut ctx, ALICE, 1, P_HIGH, QTY, 0, POST_ONLY).expect("row 1 must ACCEPT");
        assert_eq!(oo_im(&mut ctx, ALICE), 0);
    }

    // ── Row 2: Δ == 0, available(after) < 0 ⇒ ACCEPT (the B1 escape) ──
    // Same risk-reducing order, but the mark has already left the account under-covered
    // (modelled by the negative wallet, which is what a close-path fee or funding charge
    // produces). The order costs nothing yet the post-availability is still negative.
    {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        seed_long(&mut ctx, ALICE, 2);
        set_raw_wallet(&mut ctx, ALICE, -5);
        assert_eq!(
            gate_inputs(&mut ctx, ALICE, 1, P_HIGH, QTY),
            (0, -5),
            "row 2 inputs: zero delta, NEGATIVE post-availability"
        );
        try_place(&mut ctx, ALICE, 1, P_HIGH, QTY, 0, POST_ONLY).expect(
            "row 2 must ACCEPT — a risk-reducing order is never gated on the balance (B1)",
        );
        assert_eq!(oo_im(&mut ctx, ALICE), 0);
        assert_eq!(raw_wallet(&mut ctx, ALICE), -5, "and moves no money");
    }

    // ── Row 3: Δ > 0, available(after) ≥ 0 ⇒ ACCEPT ──
    // Flat position (N = 0), so a 1-lot buy at $100 costs its full notional at leverage 1.
    // 10e6 wallet − 1e6 requirement leaves 9e6.
    {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        assert_eq!(
            gate_inputs(&mut ctx, ALICE, 0, PRICE, QTY),
            (INIT_MARGIN as i128, 9_000_000),
            "row 3 inputs: positive delta, positive post-availability"
        );
        try_place(&mut ctx, ALICE, 0, PRICE, QTY, 0, POST_ONLY).expect("row 3 must ACCEPT");
        assert_eq!(oo_im(&mut ctx, ALICE), INIT_MARGIN);
        assert_eq!(super::available(&mut ctx, ALICE), 9_000_000);
    }

    // ── Row 4: Δ > 0, available(after) < 0 ⇒ REJECT ──
    // The same buy, one unit short of affordable: post-availability is exactly −1.
    {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        set_raw_wallet(&mut ctx, ALICE, INIT_MARGIN as i64 - 1);
        assert_eq!(
            gate_inputs(&mut ctx, ALICE, 0, PRICE, QTY),
            (INIT_MARGIN as i128, -1),
            "row 4 inputs: positive delta, NEGATIVE post-availability"
        );
        let err = try_place(&mut ctx, ALICE, 0, PRICE, QTY, 0, POST_ONLY)
            .expect_err("row 4 must REJECT");
        assert!(
            err.to_string()
                .contains("insufficient perp wallet for margin"),
            "{err}"
        );
        assert_eq!(oo_im(&mut ctx, ALICE), 0, "the reject wrote nothing");
    }
}
