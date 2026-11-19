//! An order that can OPEN is NOT held to its fill's per-open guards when it RESTS: one its fill would
//! refuse — the margin-tier guard at the mark, the market's position bound — is accepted, sits in
//! the book and the BBO, and is cancelled `MakerInsolvent` (reason 2) at its first touch, the walk
//! going on past it. In front of a liquidation's book leg it is a "phantom" that spends the
//! distinct-maker cap without filling (a capped pass of only phantoms is ADL'd in that pass:
//! `liquidation_cap.rs`, `cross_liquidation.rs`).
//!
//! A rest-time check (umi 33fd6884) was reverted by the owner on 2026-11-19 and left as a TODO for
//! multi-tier leverage tables (`trading::resting::rest_in_book`). These tests pin the behaviour as
//! it stands; the position-bound case is in `position_bound.rs`.

use super::*;
use crate::interface::IPerpDex::{setMarginTypeCall, OrderCancelled, OrderRested, Trade};

fn ioc_sell(ctx: &mut TestCtx, user: Address, price: u64, qty: u64) {
    run_place_order(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: Side::Sell as u8,
            price,
            quantity: qty,
            orderType: 0,
            tif: crate::types::TimeInForce::Ioc as u8,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        }
        .abi_encode(),
        user,
        ctx,
    )
    .unwrap();
}

/// `(maker, quantity)` of every `Trade` and `(user, reason)` of every `OrderCancelled` in `logs`.
#[allow(clippy::type_complexity)]
fn trades_and_cancels(logs: &[primitives::Log]) -> (Vec<(Address, u64)>, Vec<(Address, u8)>) {
    (
        decode_events::<Trade>(logs).iter().map(|t| (t.maker, t.quantity)).collect(),
        decode_events::<OrderCancelled>(logs).iter().map(|c| (c.user, c.reason)).collect(),
    )
}

/// **Q13: a flat user whose stored leverage no tier admits can REST an opening bid; it is cancelled
/// at its first touch.** ALICE set 50x under a 50x table; lowered to 20x, `getSymbolConfig` reports
/// `(50, 0)` — no notional admits her leverage, every opening FILL is refused. A non-crossing bid @
/// $90 is still accepted and rests (`OrderRested`), and the first sell that meets it fills nothing:
/// the bid is cancelled `MakerInsolvent`, ALICE stays flat — ISOLATED and CROSSED alike. After
/// `setLeverage(20)` the same bid rests and FILLS.
#[test]
fn q13_an_opening_bid_no_tier_admits_rests_and_is_cancelled_at_its_first_touch() {
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

        let _ = JournalTr::take_logs(ctx.journal_mut());
        try_place_order(&mut ctx, ALICE, Side::Buy as u8, LONG_LIQ_PRICE, 1)
            .expect("an opening rest is not judged against its fill's guards");
        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_eq!(decode_events::<OrderRested>(&logs).len(), 1, "cross {cross}");
        assert_eq!(storage::book_order_count(&mut ctx, ALICE, MARKET_ID).unwrap(), 1);

        ioc_sell(&mut ctx, MAKER, LONG_LIQ_PRICE, 1);
        let logs = JournalTr::take_logs(ctx.journal_mut());
        let (trades, cancels) = trades_and_cancels(&logs);
        assert!(trades.is_empty(), "cross {cross}: {trades:?}");
        assert_eq!(cancels, [(ALICE, CancelReason::MakerInsolvent as u8)], "cross {cross}");
        assert_eq!(position(&mut ctx, ALICE).amount, 0);
        assert_eq!(storage::book_order_count(&mut ctx, ALICE, MARKET_ID).unwrap(), 0);

        set_leverage(&mut ctx, 20).unwrap();
        assert_eq!(symbol_config(&mut ctx, ALICE, MARKET_ID), (20, u64::MAX));
        try_place_order(&mut ctx, ALICE, Side::Buy as u8, LONG_LIQ_PRICE, 1).unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());
        ioc_sell(&mut ctx, MAKER, LONG_LIQ_PRICE, 1);
        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_eq!(trades_and_cancels(&logs), (vec![(ALICE, 1)], vec![]), "cross {cross}");
        assert_eq!(position(&mut ctx, ALICE).amount, 1);
    }
}

/// **The reviewer's phantom bids need no admin action.** Tiers `[$0: 20x, $1,000: 10x]`, mark $100,
/// three accounts long at 20x each bid 1 @ $90 — all three rest. The fill's tier guard values the
/// position at the MARK:
///
/// | account | long + 1 | at its own $90 | at the $100 mark | fill |
/// |---|---|---|---|---|
/// | `phantom` | 11 + 1 | $1,080 | $1,200 (10x tier) | refused |
/// | `gap`     | 10 + 1 | $990   | $1,100 (10x tier) | refused |
/// | `fine`    | 8 + 1  | $810   | $900 (20x tier)   | fills   |
///
/// One IOC sell of 1 @ $90 walks the level in arrival order: `phantom` and `gap` are cancelled
/// `MakerInsolvent`, the walk goes on, and `fine` fills. (The reverted rest-time check valued at the
/// order's own price: it refused `phantom`, but `gap` — $990 there — rested anyway.)
#[test]
fn a_bid_whose_fill_crosses_a_tier_boundary_rests_and_is_cancelled_at_its_first_touch() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 1_000_000_000], &[20, 10]).unwrap();
    let phantom = indexed_maker(0);
    seed_position_account(&mut ctx, phantom, 11, -990_000_000, 49_500_000, 20, 200_000_000);
    let gap = indexed_maker(1);
    seed_position_account(&mut ctx, gap, 10, -900_000_000, 45_000_000, 20, 200_000_000);
    let fine = indexed_maker(2);
    seed_position_account(&mut ctx, fine, 8, -720_000_000, 36_000_000, 20, 200_000_000);
    for u in [phantom, gap, fine] {
        try_place_order(&mut ctx, u, Side::Buy as u8, LONG_LIQ_PRICE, 1).unwrap();
        assert_eq!(storage::book_order_count(&mut ctx, u, MARKET_ID).unwrap(), 1);
    }
    let _ = JournalTr::take_logs(ctx.journal_mut());

    ioc_sell(&mut ctx, MAKER, LONG_LIQ_PRICE, 1);
    let logs = JournalTr::take_logs(ctx.journal_mut());
    let insolvent = CancelReason::MakerInsolvent as u8;
    assert_eq!(
        trades_and_cancels(&logs),
        (vec![(fine, 1)], vec![(phantom, insolvent), (gap, insolvent)])
    );
    assert_eq!(position(&mut ctx, phantom).amount, 11);
    assert_eq!(position(&mut ctx, gap).amount, 10);
    assert_eq!(position(&mut ctx, fine).amount, 9);
}
