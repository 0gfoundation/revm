//! Q3 (owner decision): a closePosition conditional larger than `maxQuantity` closes in TRANCHES.
//! Each fire places `min(|position|, maxQuantity)`; a tranche that filled in full with position
//! left is re-armed under the same id — the Q1 re-arm, closePosition quantity still 0 — with
//! `deferred_work` set, so `drainDeferredWork` fires the next tranche at the same mark. A tranche
//! the book cannot fill in full is terminal, as any closePosition fire is. A fixed quantity is
//! refused at placement above maxQuantity, so it is tranched only when `updateMarket` lowered
//! maxQuantity since (Q3-12, Q3-13).
//!
//! Fixture as the parent's: single-lot bids at [`BID`], ALICE a 1x long at $100, the stop at
//! [`TRIGGER`]; `maxQuantity` lowered per test.
use super::*;
use crate::interface::IPerpDex::OrderPlaced;
use crate::trading::reduce_only::REDUCE_ONLY_CAP_REJECT;
use crate::types::{OrderType, TimeInForce};

const BOB: Address = Address::new([0xB0; 20]);
const CAROL: Address = Address::new([0xC0; 20]);

fn set_max_qty(ctx: &mut TestCtx, q: u64) {
    let mut m = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    m.max_quantity = q;
    storage::save_market(ctx, &m).unwrap();
}

/// [`tick`] at `ts`, with the block clock moved up to `ts` first (never back) so the Q2 bound
/// (a stamp at most 60 s ahead of the block) admits it.
fn tick_at(ctx: &mut TestCtx, index: u64, ts: u64) -> Vec<primitives::Log> {
    advance_block_to(ctx, ts);
    tick(ctx, index, ts)
}

fn sell_ioc_input(qty: u64) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side: Side::Sell as u8,
        price: BID,
        quantity: qty,
        orderType: 0,
        tif: 1,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode()
}

/// ALICE sells `qty` IOC at [`BID`] by hand (not reduce-only).
fn sell_ioc(ctx: &mut TestCtx, qty: u64) -> Vec<primitives::Log> {
    shell(ctx, ALICE, &sell_ioc_input(qty)).1
}

/// `n` bids of `lot` at [`BID`] from `who`, funded first.
fn big_bids(ctx: &mut TestCtx, who: Address, n: usize, lot: u64) {
    storage::save_account(
        ctx,
        who,
        UserAccount { perp_wallet_balance: BIG, ..UserAccount::default() },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    for _ in 0..n {
        run_place_order(
            &placeOrderCall {
                marketId: MARKET_ID,
                side: Side::Buy as u8,
                price: BID,
                quantity: lot,
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
    }
}

/// Every `Trade`'s quantity, in order.
fn trade_qtys(logs: &[primitives::Log]) -> Vec<u64> {
    evs::<Trade>(logs).into_iter().map(|t| t.quantity).collect()
}

/// `OrderPlaced.quantity` of every order placed under `id`.
fn placed_qtys(logs: &[primitives::Log], id: [u8; 32]) -> Vec<u64> {
    evs::<OrderPlaced>(logs).into_iter().filter(|e| e.orderId.0 == id).map(|e| e.quantity).collect()
}

fn generation(ctx: &mut TestCtx) -> u64 {
    storage::load_position(ctx, ALICE, MARKET_ID).unwrap().generation
}

fn amount_of(ctx: &mut TestCtx, who: Address) -> i64 {
    storage::load_position(ctx, who, MARKET_ID).unwrap().amount
}

/// `getAlgoOrder(id)`'s raw return bytes.
fn algo_view(ctx: &mut TestCtx, id: [u8; 32]) -> Bytes {
    shell(ctx, ALICE, &getAlgoOrderCall { algoId: id.into() }.abi_encode()).0
}

/// A closePosition STOP_MARKET sell at [`TRIGGER`] for ALICE.
fn arm_cp(ctx: &mut TestCtx) -> [u8; 32] {
    arm(ctx, AlgoOrderType::StopMarket, TRIGGER, 0, 0, 0, CP)
}

/// The same, as `who`.
fn arm_cp_as(ctx: &mut TestCtx, who: Address) -> [u8; 32] {
    let out = crate::trading::algo::run_place_algo_order(
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: Side::Sell as u8,
            orderType: AlgoOrderType::StopMarket as u8,
            triggerPrice: TRIGGER,
            price: 0,
            quantity: 0,
            tif: 0,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: CP,
        }
        .abi_encode(),
        who,
        ctx,
    )
    .unwrap();
    out[..32].try_into().unwrap()
}

/// Q3-1: a long of 100 = 2.5 × maxQuantity 40 closes in three tranches, 40 / 40 / 20, over the
/// tick and two drains. Between tranches the record is the conditional as placed (getAlgoOrder
/// bytes identical), armed at the tail of its level, with no event of its own; the generation
/// moves only at the final close.
#[test]
fn q3_1_two_and_a_half_max_quantities_close_in_three_tranches() {
    let mut ctx = fixture(100);
    set_max_qty(&mut ctx, 40);
    let ids = bids(&mut ctx, 100, 200);
    let id = arm_cp(&mut ctx);
    let view = algo_view(&mut ctx, id);
    let g0 = generation(&mut ctx);

    let logs = tick(&mut ctx, BID, 31);
    assert_eq!(mark(&mut ctx), BID);
    assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)]);
    assert_eq!(filled(&logs), ids[..40].to_vec());
    let placed: Vec<_> = evs::<OrderPlaced>(&logs).into_iter().filter(|e| e.orderId.0 == id).collect();
    assert_eq!(placed.len(), 1);
    assert_eq!(
        (placed[0].quantity, placed[0].orderType, placed[0].tif, placed[0].flags),
        (40, OrderType::Market as u8, TimeInForce::Ioc as u8, RO),
        "the tranche is a reduce-only MARKET/IOC of maxQuantity"
    );
    assert!(expired(&logs).is_empty() && algo_cancels(&logs).is_empty());
    assert_eq!(amount(&mut ctx), 60);
    assert_eq!(flag(&mut ctx), 1, "a tranche re-arm is deferred work");
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![id], vec![TRIGGER], vec![id]));
    let r = record(&mut ctx, id).expect("re-armed");
    assert!(r.close_position);
    assert_eq!((r.quantity, r.trigger_price, r.trigger_up), (0, TRIGGER, false));
    assert_eq!(algo_view(&mut ctx, id), view, "getAlgoOrder reads exactly as before the fire");
    assert_eq!(generation(&mut ctx), g0, "a partial close keeps the binding");

    let (more, logs) = drain(&mut ctx);
    assert!(more, "40 < 60: another tranche");
    assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)]);
    assert_eq!(filled(&logs), ids[40..80].to_vec());
    assert_eq!(placed_qtys(&logs, id), vec![40]);
    assert!(expired(&logs).is_empty() && algo_cancels(&logs).is_empty());
    assert_eq!(amount(&mut ctx), 20);
    assert_eq!(algo_view(&mut ctx, id), view);
    assert_eq!(generation(&mut ctx), g0);

    let (more, logs) = drain(&mut ctx);
    assert!(!more, "the last tranche is an ordinary fire");
    assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)]);
    assert_eq!(filled(&logs), ids[80..100].to_vec());
    assert_eq!(placed_qtys(&logs, id), vec![20]);
    assert!(expired(&logs).is_empty() && algo_cancels(&logs).is_empty());

    assert_eq!(amount(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));
    assert_eq!(flag(&mut ctx), 0);
    assert_eq!(generation(&mut ctx), g0 + 1, "the final close ends the position");
    assert_eq!(bid_count(&mut ctx), 0);
}

/// Q3-2: the boundary. |position| == maxQuantity is one ordinary fire (no re-arm); one more lot is
/// two tranches, 40 and 1.
#[test]
fn q3_2_exactly_max_quantity_is_one_fire_and_one_more_is_two() {
    let mut ctx = fixture(40);
    set_max_qty(&mut ctx, 40);
    let ids = bids(&mut ctx, 50, 200);
    let id = arm_cp(&mut ctx);
    let logs = tick(&mut ctx, BID, 31);
    assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)]);
    assert_eq!(filled(&logs), ids[..40].to_vec());
    assert_eq!(amount(&mut ctx), 0);
    assert_eq!(flag(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));

    let mut ctx = fixture(41);
    set_max_qty(&mut ctx, 40);
    let ids = bids(&mut ctx, 50, 200);
    let id = arm_cp(&mut ctx);
    let logs = tick(&mut ctx, BID, 31);
    assert_eq!(placed_qtys(&logs, id), vec![40]);
    assert_eq!(filled(&logs), ids[..40].to_vec());
    assert_eq!(amount(&mut ctx), 1);
    assert_eq!(flag(&mut ctx), 1);
    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(placed_qtys(&logs, id), vec![1]);
    assert_eq!(filled(&logs), ids[40..41].to_vec());
    assert_eq!(amount(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(flag(&mut ctx), 0);
}

/// Q3-3: the mark leaves the trigger between tranches. Not latched: the order waits armed (no
/// fire, the flag cleared by that tick, a drain is a no-op), and the next tick that satisfies the
/// trigger fires the next tranche.
#[test]
fn q3_3_a_tranche_waits_armed_while_the_trigger_does_not_hold() {
    let mut ctx = fixture(100);
    set_max_qty(&mut ctx, 40);
    let ids = bids(&mut ctx, 100, 200);
    let id = arm_cp(&mut ctx);

    let logs = tick_at(&mut ctx, BID, 31);
    assert_eq!(filled(&logs), ids[..40].to_vec());
    assert_eq!(amount(&mut ctx), 60);

    let logs = tick_at(&mut ctx, 9_900, 61);
    assert!(mark(&mut ctx) > TRIGGER, "fixture: this tick must not fire");
    assert!(triggered(&logs).is_empty() && filled(&logs).is_empty());
    assert_eq!(flag(&mut ctx), 0, "nothing deferred while the trigger does not hold");
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![id], vec![TRIGGER], vec![id]));
    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    assert!(triggered(&logs).is_empty() && filled(&logs).is_empty());
    assert_eq!(amount(&mut ctx), 60);

    let logs = tick_at(&mut ctx, BID, 91);
    assert_eq!(mark(&mut ctx), BID, "median(9_500, 9_100, 9_500)");
    assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)]);
    assert_eq!(filled(&logs), ids[40..80].to_vec());
    assert_eq!(amount(&mut ctx), 20);
    assert_eq!(flag(&mut ctx), 1);

    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(filled(&logs), ids[80..100].to_vec());
    assert_eq!(amount(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
}

/// Q3-4: the owner closes part of the position by hand between tranches. The binding holds (a
/// partial close keeps the generation) and the next fire re-sizes from what is left: 30, the last.
#[test]
fn q3_4_a_manual_partial_close_between_tranches_shrinks_the_next_one() {
    let mut ctx = fixture(100);
    set_max_qty(&mut ctx, 40);
    let ids = bids(&mut ctx, 100, 200);
    let id = arm_cp(&mut ctx);
    tick(&mut ctx, BID, 31);
    let g = generation(&mut ctx);

    sell_ioc(&mut ctx, 30);
    assert_eq!(amount(&mut ctx), 30);
    assert_eq!(generation(&mut ctx), g);

    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)]);
    assert_eq!(placed_qtys(&logs, id), vec![30]);
    assert_eq!(filled(&logs), ids[70..100].to_vec());
    assert_eq!(amount(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(flag(&mut ctx), 0);
}

/// Q3-5: the owner closes (or flips) the position by hand between tranches — in two IOCs, since
/// one over maxQuantity is refused. The re-armed order is spent at its next fire as
/// `PositionClosed` (11): it never trades, and it never opens the new short.
#[test]
fn q3_5_a_position_closed_or_flipped_between_tranches_spends_the_order() {
    for (second, left) in [(20, 0), (30, -10)] {
        let mut ctx = fixture(100);
        set_max_qty(&mut ctx, 40);
        bids(&mut ctx, 110, 200);
        let id = arm_cp(&mut ctx);
        tick(&mut ctx, BID, 31);
        assert_eq!(amount(&mut ctx), 60);

        let e = run_place_order(&sell_ioc_input(40 + second), ALICE, &mut ctx).unwrap_err();
        assert!(format!("{e:?}").contains("placeOrder: quantity exceeds maximum"), "{e:?}");
        sell_ioc(&mut ctx, 40);
        sell_ioc(&mut ctx, second);
        assert_eq!(amount(&mut ctx), left);

        let (more, logs) = drain(&mut ctx);
        assert!(!more);
        assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)]);
        assert_eq!(algo_cancels(&logs), vec![(id, CancelReason::PositionClosed as u8)]);
        assert!(evs::<Trade>(&logs).is_empty());
        assert_eq!(amount(&mut ctx), left, "a spent closePosition never opens");
        assert!(record(&mut ctx, id).is_none());
        assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));
        assert_eq!(flag(&mut ctx), 0);
    }
}

/// Q3-6: tranches combined with the Q1 touch cap. maxQuantity 100, long 250; the book is 64
/// single lots then 20 lots of 10. The first tranche (100) is capped at 64 touches — the Q1 re-arm,
/// `OrderExpired{64, 36}`; the next (min(186, 100) = 100) fills in full over 10 makers — a tranche
/// re-arm, no expiry; the last (86) closes the position.
#[test]
fn q3_6_tranches_combine_with_the_touch_cap() {
    let mut ctx = fixture(250);
    set_max_qty(&mut ctx, 100);
    bids(&mut ctx, 64, 200);
    big_bids(&mut ctx, maker(7), 20, 10);
    let id = arm_cp(&mut ctx);

    let logs = tick(&mut ctx, BID, 31);
    assert_eq!(trade_qtys(&logs), vec![1; 64]);
    assert_eq!(expired(&logs), vec![(id, 64, 36, 0)]);
    assert_eq!(amount(&mut ctx), 186);
    assert_eq!(flag(&mut ctx), 1);

    let (more, logs) = drain(&mut ctx);
    assert!(more);
    assert_eq!(placed_qtys(&logs, id), vec![100]);
    assert_eq!(trade_qtys(&logs), vec![10; 10]);
    assert!(expired(&logs).is_empty());
    assert_eq!(amount(&mut ctx), 86);

    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    let mut want = vec![10; 8];
    want.push(6);
    assert_eq!(trade_qtys(&logs), want);
    assert_eq!(amount(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(bid_count(&mut ctx), 2);
}

/// Q3-7: the reduce-only cap. ALICE rests 9 reduce-only sells, then arms the closePosition. After
/// the first tranche the re-armed conditional holds no slot: a 10th resting reduce-only sell is
/// admitted and an 11th refused. The drains finish the position, and going flat evicts all 10.
#[test]
fn q3_7_a_re_armed_close_position_holds_no_reduce_only_slot() {
    let mut ctx = fixture(100);
    set_max_qty(&mut ctx, 40);
    bids(&mut ctx, 100, 200);
    let ro_sell = |ctx: &mut TestCtx, price: u64| {
        run_place_order(
            &placeOrderCall {
                marketId: MARKET_ID,
                side: Side::Sell as u8,
                price,
                quantity: 1,
                orderType: 0,
                tif: TIF_GTC,
                clientOrderId: FixedBytes::default(),
                flags: RO,
            }
            .abi_encode(),
            ALICE,
            ctx,
        )
    };
    for price in 20_000..=20_008 {
        ro_sell(&mut ctx, price).unwrap();
    }
    let id = arm_cp(&mut ctx);

    tick(&mut ctx, BID, 31);
    assert_eq!(amount(&mut ctx), 60);
    assert!(record(&mut ctx, id).is_some());
    ro_sell(&mut ctx, 20_100).expect("the 10th resting reduce-only order");
    let e = ro_sell(&mut ctx, 20_101).unwrap_err();
    assert!(format!("{e:?}").contains(REDUCE_ONLY_CAP_REJECT), "{e:?}");

    let (more, _) = drain(&mut ctx);
    assert!(more);
    let (more, _) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(amount(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
    assert!(storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
}

/// Q3-8: a fixed-quantity conditional within maxQuantity is not tranched — placement holds its
/// quantity to maxQuantity — so it fires once and ends, whatever position is left. (A trailing
/// closePosition cannot exist: placement refuses it.) Above maxQuantity it is refused at
/// placement, as before.
#[test]
fn q3_8_fixed_quantity_orders_are_not_tranched() {
    for (kind, trigger, stop) in [
        (AlgoOrderType::TrailingStopMarket, 0, 9_900),
        (AlgoOrderType::StopMarket, TRIGGER, TRIGGER),
    ] {
        let mut ctx = fixture(100);
        set_max_qty(&mut ctx, 40);
        let ids = bids(&mut ctx, 100, 200);
        // A trailing stop's `price` is its callback rate: 100 bps.
        let price = if kind == AlgoOrderType::TrailingStopMarket { 100 } else { 0 };
        let e = crate::trading::algo::run_place_algo_order(
            &placeAlgoOrderCall {
                algoType: 0,
                marketId: MARKET_ID,
                side: Side::Sell as u8,
                orderType: kind as u8,
                triggerPrice: trigger,
                price,
                quantity: 41,
                tif: 0,
                workingType: 0,
                clientAlgoId: FixedBytes::default(),
                flags: RO,
            }
            .abi_encode(),
            ALICE,
            &mut ctx,
        )
        .unwrap_err();
        assert!(format!("{e:?}").contains("placeAlgoOrder: quantity exceeds maxQuantity"), "{e:?}");

        let id = arm(&mut ctx, kind, trigger, price, 40, 0, RO);
        let logs = tick(&mut ctx, BID, 31);
        assert_eq!(triggered(&logs), vec![(id, stop, BID)]);
        assert_eq!(filled(&logs), ids[..40].to_vec());
        assert_eq!(amount(&mut ctx), 60);
        assert!(record(&mut ctx, id).is_none());
        assert_eq!(flag(&mut ctx), 0);
    }
}

/// Q3-9: a keeper loop over three owners' closePosition stops at one trigger, each long 100 at
/// maxQuantity 40. Every call fires all three exactly once (a re-armed id lands behind the level
/// snapshot); the tick and two drains close all three.
#[test]
fn q3_9_a_keeper_loop_finishes_three_owners_tranches() {
    let mut ctx = fixture(100);
    for who in [BOB, CAROL] {
        seed_position_account(&mut ctx, who, 100, -100 * UNIT, 100 * UNIT, 1, BIG);
    }
    set_max_qty(&mut ctx, 40);
    bids(&mut ctx, 300, 200);
    let algo_ids: Vec<_> = [ALICE, BOB, CAROL].into_iter().map(|w| arm_cp_as(&mut ctx, w)).collect();

    let mut fires = vec![triggered(&tick(&mut ctx, BID, 31)).len()];
    let mut drains = 0;
    loop {
        let (more, logs) = drain(&mut ctx);
        drains += 1;
        fires.push(triggered(&logs).len());
        if !more {
            break;
        }
        assert!(drains < 10, "the drain loop must terminate");
    }
    assert_eq!(fires, vec![3, 3, 3]);
    assert_eq!(drains, 2);
    for (who, id) in [ALICE, BOB, CAROL].into_iter().zip(algo_ids) {
        assert_eq!(amount_of(&mut ctx, who), 0);
        assert!(record(&mut ctx, id).is_none());
    }
    assert_eq!(flag(&mut ctx), 0);
}

/// Q3-10: the book runs out mid-way. The second tranche (40) finds only 20 bids: its remainder
/// EXPIRES and the conditional ENDS — terminal, as any closePosition fire the book cannot fill.
#[test]
fn q3_10_a_tranche_the_book_cannot_fill_ends_the_order() {
    let mut ctx = fixture(100);
    set_max_qty(&mut ctx, 40);
    let ids = bids(&mut ctx, 60, 200);
    let id = arm_cp(&mut ctx);
    let logs = tick(&mut ctx, BID, 31);
    assert_eq!(filled(&logs), ids[..40].to_vec());
    assert_eq!(flag(&mut ctx), 1);

    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(filled(&logs), ids[40..60].to_vec());
    assert_eq!(expired(&logs), vec![(id, 20, 20, 0)]);
    assert!(algo_cancels(&logs).is_empty());
    assert_eq!(amount(&mut ctx), 40);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));
    assert_eq!(flag(&mut ctx), 0);
}

/// Q3-11: cancelled between tranches (T12's shape): `AlgoOrderCancelled(User)`, the index level,
/// price list and owner list empty, the position as the first tranche left it.
#[test]
fn q3_11_a_re_armed_tranche_cancels_cleanly() {
    let mut ctx = fixture(100);
    set_max_qty(&mut ctx, 40);
    bids(&mut ctx, 100, 200);
    let id = arm_cp(&mut ctx);
    tick(&mut ctx, BID, 31);
    assert_eq!(amount(&mut ctx), 60);

    let (_, logs) = shell(
        &mut ctx,
        ALICE,
        &cancelAlgoOrderCall { algoId: id.into(), marketId: MARKET_ID }.abi_encode(),
    );
    assert_eq!(algo_cancels(&logs), vec![(id, CancelReason::User as u8)]);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));
    assert_eq!(amount(&mut ctx), 60);
}

/// Q3-12: placement holds a fixed quantity to maxQuantity, but `updateMarket` can lower
/// maxQuantity afterwards. A fixed-quantity stop whose child is immediate and may fill in part —
/// STOP_MARKET, a limit STOP with tif IOC, TRAILING_STOP_MARKET — then closes in tranches as a
/// closePosition does: 40 at a lowered maxQuantity of 30 fires 30, is re-armed for the other 10
/// (a re-armed trailing stop as the fixed stop at the price that fired it, as Q1 does), and the
/// drain fires the 10. It used to place 40, be refused, and be voided as `TriggerRejected` with
/// nothing closed.
#[test]
fn q3_12_a_fixed_quantity_stop_above_a_lowered_max_quantity_closes_in_tranches() {
    const TIF_IOC: u8 = 1;
    for (kind, trigger, price, tif, stop) in [
        (AlgoOrderType::StopMarket, TRIGGER, 0, 0, TRIGGER),
        (AlgoOrderType::Stop, TRIGGER, BID, TIF_IOC, TRIGGER),
        // A trailing stop's `price` is its callback rate: 100 bps, so it fires at 9_900.
        (AlgoOrderType::TrailingStopMarket, 0, 100, 0, 9_900),
    ] {
        let mut ctx = fixture(100);
        let ids = bids(&mut ctx, 100, 200);
        let id = arm(&mut ctx, kind, trigger, price, 40, tif, RO);
        set_max_qty(&mut ctx, 30);

        let logs = tick(&mut ctx, BID, 31);
        assert_eq!(triggered(&logs), vec![(id, stop, BID)], "{kind:?}");
        assert_eq!(placed_qtys(&logs, id), vec![30], "{kind:?}: one tranche of maxQuantity");
        assert_eq!(filled(&logs), ids[..30].to_vec(), "{kind:?}");
        assert!(expired(&logs).is_empty() && algo_cancels(&logs).is_empty(), "{kind:?}");
        assert_eq!(amount(&mut ctx), 70);
        assert_eq!(flag(&mut ctx), 1, "{kind:?}: a tranche re-arm is deferred work");
        let r = record(&mut ctx, id).expect("re-armed");
        assert_eq!((r.quantity, r.trigger_price, r.trigger_up), (10, stop, false), "{kind:?}");
        assert_eq!(armed(&mut ctx, stop), (vec![id], vec![stop], vec![id]), "{kind:?}");

        let (more, logs) = drain(&mut ctx);
        assert!(!more, "{kind:?}");
        assert_eq!(triggered(&logs), vec![(id, stop, BID)], "{kind:?}");
        assert_eq!(placed_qtys(&logs, id), vec![10], "{kind:?}");
        assert_eq!(filled(&logs), ids[30..40].to_vec(), "{kind:?}");
        assert_eq!(amount(&mut ctx), 60, "{kind:?}: exactly the order's 40 closed");
        assert!(record(&mut ctx, id).is_none(), "{kind:?}");
        assert_eq!(armed(&mut ctx, stop), (vec![], vec![], vec![]), "{kind:?}");
        assert_eq!(flag(&mut ctx), 0, "{kind:?}");
    }
}

/// Q3-13: the children that cannot be tranched. A FOK is all-or-nothing, and a resting (GTC) child
/// would hold the id a re-arm needs: above a lowered maxQuantity their child is refused at trigger
/// and the order voided as `TriggerRejected` (10) with nothing traded — as a triggered FOK past the
/// touch cap is (T14). A closePosition is always MARKET, so this never applies to one.
#[test]
fn q3_13_a_fok_or_resting_child_above_a_lowered_max_quantity_is_voided() {
    for tif in [TIF_FOK, TIF_GTC] {
        let mut ctx = fixture(100);
        bids(&mut ctx, 100, 200);
        let id = arm(&mut ctx, AlgoOrderType::Stop, TRIGGER, BID, 40, tif, RO);
        set_max_qty(&mut ctx, 30);

        let logs = tick(&mut ctx, BID, 31);
        assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)], "tif {tif}");
        assert!(evs::<Trade>(&logs).is_empty(), "tif {tif}");
        assert_eq!(algo_cancels(&logs), vec![(id, CancelReason::TriggerRejected as u8)], "tif {tif}");
        assert_eq!(amount(&mut ctx), 100);
        assert!(record(&mut ctx, id).is_none());
        assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));
        assert_eq!(flag(&mut ctx), 0);
        assert_eq!(bid_count(&mut ctx), 100);
    }
}
