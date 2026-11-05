use super::*;

const FOK_REJECT: &str = "FOK order cannot be fully filled";
const MARGIN_REJECT: &str = "insufficient perp wallet for margin";

fn try_place(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8,
    price: u64,
    qty: u64,
    tif: u8,
) -> Result<Bytes, PerpError> {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: 0, // Limit
        tif,
        clientOrderId: FixedBytes::default(),
                flags: 0,
    }
    .abi_encode();
    run_place_order(&input, caller, ctx)
}

/// An unfillable FOK for `qty` at `PRICE`, on `side`.
fn unfillable_fok(ctx: &mut TestCtx, caller: Address, side: u8, qty: u64) -> PerpError {
    try_place(ctx, caller, side, PRICE, qty, 2).expect_err("an unfillable FOK must reject")
}

/// **⚠️ THE TRAP.** A position-CLOSING order must never be early-rejected, however broke the
/// account is.
///
/// A reducing order's true requirement is zero or NEGATIVE — closing frees `pos.margin` and
/// realises PnL into the wallet — and `pos.margin` is unbounded above (`addPositionMargin`), so
/// no fixed credit could make a positive bound safe. A false reject here is much worse than a
/// wasted walk: it locks a user out of de-risking at the moment they most need to.
///
/// Both directions, and both resolutions (rest and cross), because the guard is per-side.
#[test]
fn a_closing_order_is_never_early_rejected_however_broke_the_account_is() {
    for open_side in [0u8, 1u8] {
        let close_side = 1 - open_side;
        for crossing in [false, true] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            set_mark(&mut ctx, PRICE);
            // BOB takes a position by crossing ALICE.
            place(&mut ctx, ALICE, close_side, PRICE, QTY, 0, 0);
            place(&mut ctx, BOB, open_side, PRICE, QTY, 0, 1); // IOC → never rests
            let signed = if open_side == 0 { 1i64 } else { -1 };
            assert_eq!(
                pos(&mut ctx, BOB).amount,
                signed * QTY as i64,
                "precondition: BOB must hold the position the close will reduce"
            );
            // As broke as the fixture can make it — far deeper than anything the close releases.
            set_available(&mut ctx, BOB, -50 * FILL_VALUE as i64);
            if crossing {
                // Someone to close against, so the close FILLS instead of resting.
                place(&mut ctx, ALICE, open_side, PRICE, QTY, 0, 0);
            }

            try_place(&mut ctx, BOB, close_side, PRICE, QTY, 0).unwrap_or_else(|e| {
                panic!(
                    "open_side={open_side} crossing={crossing}: a closing order must never be \
                     refused for margin — the pre-walk bound must not apply to it: {e}"
                )
            });
            if crossing {
                assert_eq!(
                    pos(&mut ctx, BOB).amount,
                    0,
                    "open_side={open_side}: the close really closed"
                );
            }
        }
    }
}

/// A hedged order whose true requirement is ZERO must not be early-rejected either.
///
/// `ooIM = max(|N + Bid|, |N − Ask|)/L − |N|/L`, so a bid that fits under an existing ask costs
/// nothing. The bound is not hand-rolled arithmetic precisely so it inherits that `max()` — this
/// is the test that would fail if it were ever replaced by "notional / leverage".
#[test]
fn a_hedged_order_with_a_zero_requirement_is_not_early_rejected() {
    for (resting_side, probe_side) in [(1u8, 0u8), (0u8, 1u8)] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        set_mark(&mut ctx, PRICE);
        // TWICE the probe's size, so the probe's notional stays strictly under it even after
        // the one-tick offset below — that is what makes the `max()` unmoved and the
        // requirement exactly 0.
        place(&mut ctx, BOB, resting_side, PRICE, QTY * 2, 0, 0);
        // Nothing spendable left at all.
        set_available(&mut ctx, BOB, 0);
        // One tick AWAY on the probe's own side, so it cannot self-match BOB's resting order.
        let probe_price = if probe_side == 0 {
            PRICE - TICK
        } else {
            PRICE + TICK
        };
        try_place_limit(&mut ctx, BOB, probe_side, probe_price).unwrap_or_else(|e| {
            panic!(
                "resting_side={resting_side}: a free hedged order must be admitted at zero \
                 available: {e}"
            )
        });
    }
}

/// **It fires, and it fires BEFORE the walk.** The FOK message is the witness: reaching
/// `check_fok_feasibility` would have produced it, and it did not.
#[test]
fn a_provably_unaffordable_fok_is_refused_without_walking_the_book() {
    for side in [0u8, 1u8] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        set_mark(&mut ctx, PRICE);
        set_available(&mut ctx, BOB, 0);
        // 11 lots against an EMPTY book: unfillable AND unaffordable at once.
        let err = unfillable_fok(&mut ctx, BOB, side, QTY * 11);
        assert!(
            err.to_string().contains(MARGIN_REJECT),
            "side={side}: expected the pre-walk margin reject, got {err}"
        );
        assert!(
            !err.to_string().contains(FOK_REJECT),
            "side={side}: the FOK feasibility WALK ran — the early-out did not precede it: {err}"
        );
    }
}

/// Guard 1 is really a guard: with an offsetting position the filter stands down, so the
/// walk-based check runs and reports the FOK message.
///
/// The mirror of the previous test on the same fixture, which is what makes it a differential on
/// the guard rather than an assertion about a message.
#[test]
fn an_offsetting_position_stands_the_filter_down() {
    for open_side in [0u8, 1u8] {
        let close_side = 1 - open_side;
        let mut ctx = make_ctx();
        setup(&mut ctx);
        set_mark(&mut ctx, PRICE);
        place(&mut ctx, ALICE, close_side, PRICE, QTY, 0, 0);
        place(&mut ctx, BOB, open_side, PRICE, QTY, 0, 1);
        set_available(&mut ctx, BOB, 0);
        // A REDUCING FOK, unfillable (the book is empty again).
        let err = unfillable_fok(&mut ctx, BOB, close_side, QTY * 11);
        assert!(
            err.to_string().contains(FOK_REJECT),
            "open_side={open_side}: the filter must stand down for a reducing order, so the \
             walk-based FOK check should be what rejects: {err}"
        );
    }
}

/// Guard 2, the same way: a resting order on the order's OWN side stands the filter down (it is
/// what the cover loop would cancel, and a cancel can shrink the baseline the bound assumes).
#[test]
fn an_own_side_resting_order_stands_the_filter_down() {
    for side in [0u8, 1u8] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        set_mark(&mut ctx, PRICE);
        // A same-side resting order, placed while BOB can still afford it…
        let away = if side == 0 {
            PRICE - 10 * TICK
        } else {
            PRICE + 10 * TICK
        };
        place(&mut ctx, BOB, side, away, QTY, 0, 0);
        // …then drain the account.
        set_available(&mut ctx, BOB, 0);
        let err = unfillable_fok(&mut ctx, BOB, side, QTY * 11);
        assert!(
            err.to_string().contains(FOK_REJECT),
            "side={side}: with an own-side resting order the filter must stand down: {err}"
        );
    }
}

/// **The bound never cuts into the accepted region.** At EXACTLY the requirement the order is
/// still admitted; one quantum below, it is refused. That the boundary is unmoved is the
/// operative half — a filter that shaved even one quantum off it would fail here.
///
/// (The crossing case is pinned by
/// `the_match_and_rest_path_is_mirror_symmetric_between_buy_and_sell`, whose acceptance leg sits
/// exactly on the boundary and whose orders satisfy both guards.)
#[test]
fn the_bound_does_not_move_the_acceptance_boundary_of_a_pure_rest() {
    for side in [0u8, 1u8] {
        for (target, expect_ok) in [(FILL_VALUE as i64, true), (FILL_VALUE as i64 - 1, false)] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            set_mark(&mut ctx, PRICE);
            set_available(&mut ctx, BOB, target);
            let result = try_place_limit(&mut ctx, BOB, side, PRICE);
            assert_eq!(
                result.is_ok(),
                expect_ok,
                "side={side} available={target}: expected ok={expect_ok}, got {result:?}"
            );
        }
    }
}
