//! An order that can OPEN meets the per-open guards of its fill when it RESTS (Q6/Q13 review):
//! the margin-tier guard and the market's position bound, judged with the order filled in full at
//! its own price (`trading::resting::open_guard_refusal`).
//!
//! Before, such an order was accepted, sat in the book and the BBO, and was cancelled
//! `MakerInsolvent` at its first touch — invisible to its owner until then, and, in front of a
//! liquidation's book leg, a "phantom" that spent the distinct-maker cap without filling.

use super::*;
use crate::interface::IPerpDex::{setMarginTypeCall, OrderPlaced};

const TIER_REJECT: &str = "placeOrder: leverage exceeds the margin tier for this position size";

fn place_sell(ctx: &mut TestCtx, user: Address, price: u64, qty: u64) -> Result<Bytes, PerpError> {
    try_place_order(ctx, user, Side::Sell as u8, price, qty)
}

/// Asserts `f` is refused with `reason` and leaves no trace: no perp write, no log, no order.
fn refused_write_free(
    ctx: &mut TestCtx,
    user: Address,
    reason: &str,
    f: impl FnOnce(&mut TestCtx) -> Result<Bytes, PerpError>,
) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let (writes, orders) =
        (ctx.perp_write_count(), storage::book_order_count(ctx, user, MARKET_ID).unwrap());
    let err = f(ctx).unwrap_err();
    assert_eq!(err.to_string(), reason);
    assert_eq!(ctx.perp_write_count(), writes, "a refused rest wrote");
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty(), "a refused rest logged");
    assert_eq!(storage::book_order_count(ctx, user, MARKET_ID).unwrap(), orders);
}

/// **Q13: once no tier admits the stored leverage, an opening order cannot rest either.** A flat
/// ALICE set 50x under a 50x table; lowered to 20x, `getSymbolConfig` reports `(50, 0)` — no
/// notional admits her leverage — and the ABI says every open is refused. It held for a crossing
/// order only: a non-crossing bid @ $90 used to be ACCEPTED, rest in the book, and be cancelled
/// `MakerInsolvent` at its first touch. Now it is refused at placement with the tier guard's string,
/// write-free — ISOLATED and CROSSED alike (the tier guard has no margin-type condition). After
/// `setLeverage(20)` the same bid rests.
#[test]
fn q13_an_opening_rest_is_refused_when_no_tier_admits_the_stored_leverage() {
    let _gate = crate::margin::margin_type::test_gate::enable();
    for cross in [false, true] {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0], &[50]).unwrap();
        // The suite's never-set type is ISOLATED (`make_ctx`); switch for the CROSSED run.
        if cross {
            let out = run_perp_dex_call(
                &setMarginTypeCall { marketId: MARKET_ID, marginType: 0 }.abi_encode(),
                1_000_000,
                ALICE,
                U256::ZERO,
                false,
                &mut ctx,
            )
            .unwrap();
            assert!(!out.reverted, "{:?}", out.bytes);
        }
        set_leverage(&mut ctx, 50).unwrap();
        set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0], &[20]).unwrap();
        assert_eq!(symbol_config(&mut ctx, ALICE, MARKET_ID), (50, 0), "cross {cross}");
        let want = if cross {
            crate::types::MarginType::Crossed
        } else {
            crate::types::MarginType::Isolated
        };
        assert_eq!(
            crate::margin::margin_type::effective_margin_type(&position(&mut ctx, ALICE)),
            want
        );

        refused_write_free(&mut ctx, ALICE, TIER_REJECT, |ctx| {
            try_place_order(ctx, ALICE, Side::Buy as u8, LONG_LIQ_PRICE, 1)
        });
        refused_write_free(&mut ctx, ALICE, TIER_REJECT, |ctx| {
            place_sell(ctx, ALICE, SHORT_LIQ_PRICE, 1)
        });

        set_leverage(&mut ctx, 20).unwrap();
        try_place_order(&mut ctx, ALICE, Side::Buy as u8, LONG_LIQ_PRICE, 1).unwrap();
        assert_eq!(storage::book_order_count(&mut ctx, ALICE, MARKET_ID).unwrap(), 1);
    }
}

/// **A reduce is never held to the open guards; a flip is held to them on the side it opens.**
/// ALICE long 10 @ $100 at a stored 5x under the default 3x table — past her tier, as a
/// `setMarginTiers` that lowered the cap could leave her. A sell of 10 (a pure close) rests; a sell
/// of 15 would close the 10 and OPEN a short of 5 ($550 at its $110 price, still at 5x), so it is
/// refused; a buy of any size would open, so it is refused too.
#[test]
fn a_reduce_is_never_held_to_the_open_guards_a_flip_is_on_the_side_it_opens() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    assert_eq!(position(&mut ctx, ALICE).leverage, 5);

    refused_write_free(&mut ctx, ALICE, TIER_REJECT, |ctx| {
        place_sell(ctx, ALICE, SHORT_LIQ_PRICE, QTY as u64 + 5)
    });
    refused_write_free(&mut ctx, ALICE, TIER_REJECT, |ctx| {
        try_place_order(ctx, ALICE, Side::Buy as u8, LONG_LIQ_PRICE, 1)
    });
    let _ = JournalTr::take_logs(ctx.journal_mut());
    place_sell(&mut ctx, ALICE, SHORT_LIQ_PRICE, QTY as u64).expect("a pure close rests");
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(decode_events::<OrderPlaced>(&logs).len(), 1);
    assert_eq!(storage::book_order_count(&mut ctx, ALICE, MARKET_ID).unwrap(), 1);
}

/// **The reviewer's phantom bids, without any admin action, can no longer rest.** Tiers
/// `[$0: 20x, $1,000: 10x]`; an account long 11 @ $90 at 20x ($990, tier 0) bids 1 @ $90: filled,
/// it would hold $1,080 — tier 1, whose 10x cap is below its 20x — so the tier guard of the fill
/// refuses it. That bid used to rest and be cancelled `MakerInsolvent` at its first touch: 130 such
/// accounts, two bids each, stretched a liquidation over three passes, two closing nothing. Now it
/// is refused at placement. A bid that keeps the account in tier 0 (long 10 + 1: $990) still rests.
#[test]
fn a_bid_its_fill_would_take_past_the_tier_cap_cannot_rest() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 1_000_000_000], &[20, 10]).unwrap();
    let attacker = indexed_maker(0);
    seed_position_account(&mut ctx, attacker, 11, -990_000_000, 49_500_000, 20, 200_000_000);
    refused_write_free(&mut ctx, attacker, TIER_REJECT, |ctx| {
        try_place_order(ctx, attacker, Side::Buy as u8, LONG_LIQ_PRICE, 1)
    });

    let fine = indexed_maker(1);
    seed_position_account(&mut ctx, fine, 10, -900_000_000, 45_000_000, 20, 200_000_000);
    try_place_order(&mut ctx, fine, Side::Buy as u8, LONG_LIQ_PRICE, 1).unwrap();
    assert_eq!(storage::book_order_count(&mut ctx, fine, MARKET_ID).unwrap(), 1);
}
