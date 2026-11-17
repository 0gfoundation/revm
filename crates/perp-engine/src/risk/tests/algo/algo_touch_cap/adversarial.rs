//! Adversarial cases for Q1 on fired conditionals (from the breaker pass).
use super::*;

fn bid_from(ctx: &mut TestCtx, who: Address) -> [u8; 32] {
    let out = run_place_order(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: Side::Buy as u8,
            price: BID,
            quantity: 1,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        }
        .abi_encode(),
        who,
        ctx,
    )
    .unwrap();
    placeOrderCall::abi_decode_returns(&out).unwrap().0
}

fn fund(ctx: &mut TestCtx, who: Address) {
    storage::save_account(
        ctx,
        who,
        UserAccount { perp_wallet_balance: BIG, ..UserAccount::default() },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

fn sell_ioc(ctx: &mut TestCtx, qty: u64) -> Vec<primitives::Log> {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: Side::Sell as u8,
        price: BID,
        quantity: qty,
        orderType: 0,
        tif: 1,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode();
    shell(ctx, ALICE, &input).1
}

/// No call fires the same conditional twice, and no fire walks more than 64 makers: a fixed stop and
/// a trailing stop both capped in the same tick, the trailing one re-armed into the fixed index at a
/// NEW price ahead of the fixed one.
#[test]
fn brk_fixed_and_trailing_both_capped_never_fire_twice_per_call() {
    let mut ctx = fixture(300);
    bids(&mut ctx, 400, 200);
    let fixed = arm(&mut ctx, AlgoOrderType::StopMarket, TRIGGER, 0, 100, 0, RO);
    let trail = arm(&mut ctx, AlgoOrderType::TrailingStopMarket, 0, 100, 100, 0, RO);
    let mut calls = vec![tick(&mut ctx, BID, 31)];
    loop {
        let (more, logs) = drain(&mut ctx);
        calls.push(logs);
        if !more {
            break;
        }
        assert!(calls.len() < 20, "drain loop must terminate");
    }
    for (k, logs) in calls.iter().enumerate() {
        let t = triggered(logs);
        for id in [fixed, trail] {
            assert!(t.iter().filter(|x| x.0 == id).count() <= 1, "call {k}: fired twice");
        }
        assert!(filled(logs).len() <= 64 * t.len(), "call {k}: {} fills for {} fires", filled(logs).len(), t.len());
    }
    assert_eq!(amount(&mut ctx), 100);
    assert!(record(&mut ctx, fixed).is_none() && record(&mut ctx, trail).is_none());
    assert_eq!(flag(&mut ctx), 0);
}

/// A trailing stop re-armed as a fixed stop, whose position the owner then FLIPS by hand, is spent
/// as `PositionClosed` at its next fire — it never opens on the new short.
#[test]
fn brk_rearmed_trailing_then_manual_flip_is_position_closed() {
    let mut ctx = fixture(100);
    bids(&mut ctx, 250, 125);
    let id = arm(&mut ctx, AlgoOrderType::TrailingStopMarket, 0, 100, 100, 0, RO);
    tick(&mut ctx, BID, 31);
    assert_eq!(amount(&mut ctx), 36);
    assert!(record(&mut ctx, id).unwrap().trigger_price != 0);
    sell_ioc(&mut ctx, 36);
    sell_ioc(&mut ctx, 50);
    assert_eq!(amount(&mut ctx), -50);
    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(algo_cancels(&logs), vec![(id, CancelReason::PositionClosed as u8)]);
    assert!(filled(&logs).is_empty());
    assert_eq!(amount(&mut ctx), -50);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(armed(&mut ctx, 9_900), (vec![], vec![], vec![]));
}

/// Termination when the touched makers are all skip-type (frozen, then the owner's own STP-cancelled
/// bids): every capped fire fills NOTHING but still removes 64 live makers, so the drain ends.
#[test]
fn brk_zero_fill_capped_fires_still_terminate() {
    let mut ctx = fixture(10);
    let frozen = maker(900);
    fund(&mut ctx, frozen);
    for _ in 0..64 {
        bid_from(&mut ctx, frozen);
    }
    storage::set_in_liquidation_for_test(&mut ctx, frozen, true).unwrap();
    for _ in 0..64 {
        bid_from(&mut ctx, ALICE);
    }
    let live = maker(901);
    fund(&mut ctx, live);
    for _ in 0..10 {
        bid_from(&mut ctx, live);
    }
    let id = arm(&mut ctx, AlgoOrderType::StopMarket, TRIGGER, 0, 0, 0, CP);
    let logs = tick(&mut ctx, BID, 31);
    assert!(filled(&logs).is_empty());
    assert_eq!(evs::<crate::interface::IPerpDex::OrderCancelled>(&logs).len(), 64);
    assert_eq!(flag(&mut ctx), 1);
    let mut drains = 0;
    loop {
        let (more, logs) = drain(&mut ctx);
        drains += 1;
        assert!(evs::<crate::interface::IPerpDex::OrderCancelled>(&logs).len() + filled(&logs).len() <= 64);
        if !more {
            break;
        }
        assert!(drains < 10, "drain loop must terminate");
    }
    assert_eq!(drains, 2);
    assert_eq!(amount(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
}
