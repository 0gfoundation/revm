//! Q1 on a FIRED conditional order (spec §4, Option A). A fired child walks the book under the
//! ordinary 64-touch cap; when the cap stops it with quantity left, its remainder EXPIRES
//! (`OrderExpired`, reason 0), and the conditional is RE-ARMED in the fixed trigger index with the
//! leftover quantity and `deferred_work` set, so `drainDeferredWork` finishes it at the same mark.
//! A re-armed trailing stop becomes a fixed stop at the price that fired it (still position-bound);
//! a triggered FOK that would pass the cap is voided (`TriggerRejected`).
//!
//! Book: single-lot BIDS at $95.00 from distinct maker accounts (≤ 200 orders each); ALICE is a
//! 1x long at $100 the sell stop closes. Every oracle tick and drain goes through the call shell.
use super::*;
use crate::interface::IPerpDex::{
    cancelAlgoOrderCall, getAlgoOrderCall, placeAlgoOrderCall, AlgoOrderCancelled,
    AlgoOrderTriggered, OrderExpired, OrderRested, Trade,
};
use crate::types::{AlgoOrderType, CancelReason, Order};

const RO: u8 = 1;
const CP: u8 = 2;
/// Where every maker bid rests, and the mark the firing tick publishes.
const BID: u64 = 9_500;
/// The fixed stop price: fires on a mark `<= 9_600`.
const TRIGGER: u64 = 9_600;
const UNIT: i64 = 100_000_000; // one contract at $100, in quote units
const BIG: i64 = 1_000_000_000_000_000;
const GAS: u64 = 30_000_000;
const TIF_GTC: u8 = 0;
const TIF_FOK: u8 = 2;

fn maker(i: usize) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0x5c;
    b[12..].copy_from_slice(&(i as u64).to_be_bytes());
    Address::from(b)
}

/// The risk fixture's market at a $100 mark, plus ALICE long `amount` at $100, fully margined (1x),
/// with a large wallet so no liquidation can interfere.
fn fixture(amount: i64) -> TestCtx {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, amount, -amount * UNIT, amount * UNIT, 1, BIG);
    ctx
}

/// `n` single-lot bids at [`BID`]: maker 0's first 200 (or `per` if smaller), then maker 1's, … —
/// in FIFO order.
fn bids(ctx: &mut TestCtx, n: usize, per: usize) -> Vec<[u8; 32]> {
    (0..n)
        .map(|i| {
            let m = maker(i / per);
            if i % per == 0 {
                storage::save_account(
                    ctx,
                    m,
                    UserAccount { perp_wallet_balance: BIG, ..UserAccount::default() },
                    AccountUpdateReason::Adjustment,
                )
                .unwrap();
            }
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
                m,
                ctx,
            )
            .unwrap();
            placeOrderCall::abi_decode_returns(&out).unwrap().0
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn arm(
    ctx: &mut TestCtx,
    kind: AlgoOrderType,
    trigger: u64,
    price: u64,
    qty: u64,
    tif: u8,
    flags: u8,
) -> [u8; 32] {
    let out = crate::trading::algo::run_place_algo_order(
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: Side::Sell as u8,
            orderType: kind as u8,
            triggerPrice: trigger,
            price,
            quantity: qty,
            tif,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags,
        }
        .abi_encode(),
        ALICE,
        ctx,
    )
    .unwrap();
    out[..32].try_into().unwrap()
}

fn take(ctx: &mut TestCtx) -> Vec<primitives::Log> {
    JournalTr::take_logs(ctx.journal_mut())
}

fn evs<E: SolEvent>(logs: &[primitives::Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

/// A successful shell call by `caller`; returns its logs.
fn shell(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> (Bytes, Vec<primitives::Log>) {
    let _ = take(ctx);
    let out = run_perp_dex_call(input, GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(!out.reverted, "unexpected revert: {:?}", out.bytes);
    (out.bytes, take(ctx))
}

/// The oracle tick at `index`, `ts` (aligned to 15).
fn tick(ctx: &mut TestCtx, index: u64, ts: u64) -> Vec<primitives::Log> {
    let input =
        updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: ts }.abi_encode();
    shell(ctx, ADMIN, &input).1
}

/// `drainDeferredWork` by a stranger: `(moreRemaining, logs)`.
fn drain(ctx: &mut TestCtx) -> (bool, Vec<primitives::Log>) {
    let (out, logs) =
        shell(ctx, Address::new([0xDD; 20]), &drainDeferredWorkCall { marketId: MARKET_ID }.abi_encode());
    (drainDeferredWorkCall::abi_decode_returns(&out).unwrap(), logs)
}

fn mark(ctx: &mut TestCtx) -> u64 {
    storage::load_market(ctx, MARKET_ID).unwrap().unwrap().mark_price
}

fn flag(ctx: &mut TestCtx) -> u8 {
    storage::load_market(ctx, MARKET_ID).unwrap().unwrap().deferred_work
}

fn amount(ctx: &mut TestCtx) -> i64 {
    storage::load_position(ctx, ALICE, MARKET_ID).unwrap().amount
}

/// Maker order ids each `Trade` filled, asserting every fill is one lot at [`BID`].
fn filled(logs: &[primitives::Log]) -> Vec<[u8; 32]> {
    evs::<Trade>(logs)
        .into_iter()
        .map(|t| {
            assert_eq!((t.price, t.quantity, t.taker), (BID, 1, ALICE));
            t.makerOrderId.0
        })
        .collect()
}

fn expired(logs: &[primitives::Log]) -> Vec<([u8; 32], u64, u64, u8)> {
    evs::<OrderExpired>(logs)
        .into_iter()
        .map(|e| (e.orderId.0, e.filledQuantity, e.expiredQuantity, e.reason))
        .collect()
}

fn triggered(logs: &[primitives::Log]) -> Vec<([u8; 32], u64, u64)> {
    evs::<AlgoOrderTriggered>(logs)
        .into_iter()
        .map(|e| (e.algoId.0, e.triggerPrice, e.markPrice))
        .collect()
}

fn algo_cancels(logs: &[primitives::Log]) -> Vec<([u8; 32], u8)> {
    evs::<AlgoOrderCancelled>(logs).into_iter().map(|e| (e.algoId.0, e.reason)).collect()
}

/// The fixed DOWN index at `price`, the down price list, and ALICE's conditional list.
fn armed(ctx: &mut TestCtx, price: u64) -> (Vec<[u8; 32]>, Vec<u64>, Vec<[u8; 32]>) {
    (
        storage::load_algo_level(ctx, MARKET_ID, false, price).unwrap(),
        storage::load_algo_down_prices(ctx, MARKET_ID).unwrap(),
        storage::load_user_algo_orders(ctx, ALICE, MARKET_ID).unwrap(),
    )
}

fn record(ctx: &mut TestCtx, id: [u8; 32]) -> Option<Order> {
    storage::load_order(ctx, &id).unwrap()
}

fn bid_count(ctx: &mut TestCtx) -> u64 {
    storage::load_bid_count(ctx, MARKET_ID, BID).unwrap()
}

/// T10: a fixed STOP_MARKET reduce-only sell of 100 on a long of 100, against 150 bids (2 makers).
/// The tick fires it: 64 fills, child `OrderExpired{64, 36, 0}`, the record RE-ARMED with quantity
/// 36 at the same trigger (index + `ualg`), `deferred_work` set. The drain fires it again at the
/// same mark: 36 fills, the record gone, flat, the bit cleared.
#[test]
fn t10_a_capped_stop_market_re_arms_the_remainder_and_the_drain_finishes_it() {
    let mut ctx = fixture(100);
    let ids = bids(&mut ctx, 150, 100);
    let id = arm(&mut ctx, AlgoOrderType::StopMarket, TRIGGER, 0, 100, 0, RO);

    let logs = tick(&mut ctx, BID, 31);
    assert_eq!(mark(&mut ctx), BID, "fixture: the tick publishes mark == index");
    assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)]);
    assert_eq!(filled(&logs), ids[..64].to_vec());
    assert_eq!(expired(&logs), vec![(id, 64, 36, 0)]);
    assert!(algo_cancels(&logs).is_empty());
    assert_eq!(amount(&mut ctx), 36);
    let r = record(&mut ctx, id).expect("re-armed");
    assert_eq!((r.quantity, r.trigger_price, r.trigger_up), (36, TRIGGER, false));
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![id], vec![TRIGGER], vec![id]));
    assert_eq!(flag(&mut ctx), 1, "a fire the cap stopped is deferred work");

    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)]);
    assert_eq!(filled(&logs), ids[64..100].to_vec());
    assert!(expired(&logs).is_empty() && algo_cancels(&logs).is_empty());
    assert_eq!(amount(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));
    assert_eq!(flag(&mut ctx), 0);
    assert_eq!(bid_count(&mut ctx), 50);
}

/// T11: a closePosition stop on a long of 150 against 200 bids fires 64, 64 and 22 (the tick and two
/// drains). Its stored quantity is unchanged by each re-arm (it re-sizes from |position| at each
/// fire).
#[test]
fn t11_a_capped_close_position_re_arms_with_its_quantity_unchanged() {
    let mut ctx = fixture(150);
    let ids = bids(&mut ctx, 200, 100);
    let id = arm(&mut ctx, AlgoOrderType::StopMarket, TRIGGER, 0, 0, 0, CP);
    let q0 = record(&mut ctx, id).unwrap().quantity;

    let logs = tick(&mut ctx, BID, 31);
    assert_eq!(filled(&logs), ids[..64].to_vec());
    assert_eq!(expired(&logs), vec![(id, 64, 86, 0)]);
    assert_eq!(amount(&mut ctx), 86);
    let r = record(&mut ctx, id).unwrap();
    assert!(r.close_position);
    assert_eq!(r.quantity, q0);
    assert_eq!(flag(&mut ctx), 1);

    let (more, logs) = drain(&mut ctx);
    assert!(more, "capped again: 86 > 64");
    assert_eq!(filled(&logs), ids[64..128].to_vec());
    assert_eq!(expired(&logs), vec![(id, 64, 22, 0)]);
    assert_eq!(amount(&mut ctx), 22);
    assert_eq!(record(&mut ctx, id).unwrap().quantity, q0);

    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(filled(&logs), ids[128..150].to_vec());
    assert!(expired(&logs).is_empty());
    assert_eq!(amount(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));
    assert_eq!(flag(&mut ctx), 0);
}

/// T12: T10 without the drain. The next tick's mark does not reach the trigger, so the remainder
/// stays armed with quantity 36 (not latched); `cancelAlgoOrder` then removes it cleanly —
/// `AlgoOrderCancelled(User)`, the index level and price list and `ualg` all empty.
#[test]
fn t12_an_undrained_remainder_stays_armed_and_cancels_cleanly() {
    let mut ctx = fixture(100);
    bids(&mut ctx, 150, 100);
    let id = arm(&mut ctx, AlgoOrderType::StopMarket, TRIGGER, 0, 100, 0, RO);
    tick(&mut ctx, BID, 31);
    assert_eq!(amount(&mut ctx), 36);

    let logs = tick(&mut ctx, 9_900, 61);
    assert!(mark(&mut ctx) > TRIGGER, "fixture: this tick must not fire");
    assert!(triggered(&logs).is_empty());
    assert_eq!(record(&mut ctx, id).unwrap().quantity, 36);
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![id], vec![TRIGGER], vec![id]));
    assert_eq!(amount(&mut ctx), 36);

    let (_, logs) = shell(
        &mut ctx,
        ALICE,
        &cancelAlgoOrderCall { algoId: id.into(), marketId: MARKET_ID }.abi_encode(),
    );
    assert_eq!(algo_cancels(&logs), vec![(id, CancelReason::User as u8)]);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));
}

/// T13: a trailing stop (sell 100, callback 1%, activated at the $100 mark) fires at the tick's
/// $95 mark on the stop `10_000 − 1% = 9_900`. Capped at 64, it is re-armed as a FIXED stop at
/// 9_900 (`trigger_up = false`), keeping `trail_bps` (still position-bound) and leaving no trailing
/// link; the drain closes the other 36. A separate run cancels it while re-armed: no index entry,
/// no link, nothing left.
#[test]
fn t13_a_capped_trailing_stop_re_arms_as_a_fixed_stop_at_the_fired_price() {
    const STOP: u64 = 9_900;
    for cancel in [false, true] {
        let mut ctx = fixture(100);
        let ids = bids(&mut ctx, 150, 100);
        let id = arm(&mut ctx, AlgoOrderType::TrailingStopMarket, 0, 100, 100, 0, RO);
        assert_eq!(record(&mut ctx, id).unwrap().trail_bps, 100);

        let logs = tick(&mut ctx, BID, 31);
        assert_eq!(triggered(&logs), vec![(id, STOP, BID)]);
        assert_eq!(filled(&logs), ids[..64].to_vec());
        assert_eq!(expired(&logs), vec![(id, 64, 36, 0)]);
        let r = record(&mut ctx, id).unwrap();
        assert_eq!(
            (r.quantity, r.trigger_price, r.trigger_up, r.trail_bps),
            (36, STOP, false, 100)
        );
        assert!(r.is_position_bound());
        assert!(crate::trading::algo::trailing::load_link(&mut ctx, &id).unwrap().is_none());
        assert_eq!(armed(&mut ctx, STOP), (vec![id], vec![STOP], vec![id]));
        assert_eq!(flag(&mut ctx), 1);
        // getAlgoOrder: still type 4 with its callback, but reported as the fixed stop it now is —
        // activated, no peak, the current stop = the trigger it re-armed at.
        let (ret, _) =
            shell(&mut ctx, ALICE, &getAlgoOrderCall { algoId: id.into() }.abi_encode());
        let v = getAlgoOrderCall::abi_decode_returns(&ret).unwrap();
        assert_eq!(
            (v.orderType, v.triggerPrice, v.quantity, v.callbackRateBps),
            (AlgoOrderType::TrailingStopMarket as u8, STOP, 36, 100)
        );
        assert_eq!((v.activated, v.peakPrice, v.currentStop), (true, 0, STOP));

        if cancel {
            let (_, logs) = shell(
                &mut ctx,
                ALICE,
                &cancelAlgoOrderCall { algoId: id.into(), marketId: MARKET_ID }.abi_encode(),
            );
            assert_eq!(algo_cancels(&logs), vec![(id, CancelReason::User as u8)]);
            assert_eq!(amount(&mut ctx), 36);
        } else {
            let (more, logs) = drain(&mut ctx);
            assert!(!more);
            assert_eq!(triggered(&logs), vec![(id, STOP, BID)]);
            assert_eq!(filled(&logs), ids[64..100].to_vec());
            assert_eq!(amount(&mut ctx), 0);
        }
        assert!(record(&mut ctx, id).is_none());
        assert_eq!(armed(&mut ctx, STOP), (vec![], vec![], vec![]));
        assert!(crate::trading::algo::trailing::load_link(&mut ctx, &id).unwrap().is_none());
    }
}

/// T14: a STOP (limit) sell of 100 at 9_400 GTC on a long of 100 against 150 crossing bids: 64
/// fills, the remainder EXPIRES rather than resting behind bids it crosses (no `OrderRested`, no
/// resting order), and the conditional is re-armed with 36. The same order as a FOK cannot fill 100
/// within the cap: it is voided with `AlgoOrderCancelled(TriggerRejected)`, nothing traded.
#[test]
fn t14_a_triggered_gtc_clamps_and_re_arms_a_triggered_fok_is_voided() {
    for tif in [TIF_GTC, TIF_FOK] {
        let mut ctx = fixture(100);
        let ids = bids(&mut ctx, 150, 100);
        let id = arm(&mut ctx, AlgoOrderType::Stop, TRIGGER, 9_400, 100, tif, RO);
        let logs = tick(&mut ctx, BID, 31);
        assert_eq!(triggered(&logs), vec![(id, TRIGGER, BID)]);
        assert!(evs::<OrderRested>(&logs).is_empty());
        assert!(storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
        if tif == TIF_GTC {
            assert_eq!(filled(&logs), ids[..64].to_vec());
            assert_eq!(expired(&logs), vec![(id, 64, 36, 0)]);
            assert!(algo_cancels(&logs).is_empty());
            assert_eq!(amount(&mut ctx), 36);
            let r = record(&mut ctx, id).unwrap();
            assert_eq!((r.quantity, r.trigger_price, r.price), (36, TRIGGER, 9_400));
            assert_eq!(armed(&mut ctx, TRIGGER), (vec![id], vec![TRIGGER], vec![id]));
            assert_eq!(flag(&mut ctx), 1);
        } else {
            assert!(filled(&logs).is_empty() && expired(&logs).is_empty());
            assert_eq!(algo_cancels(&logs), vec![(id, CancelReason::TriggerRejected as u8)]);
            assert_eq!(CancelReason::TriggerRejected as u8, 10);
            assert_eq!(amount(&mut ctx), 100);
            assert!(record(&mut ctx, id).is_none());
            assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));
            assert_eq!(flag(&mut ctx), 0);
            assert_eq!(bid_count(&mut ctx), 150);
        }
    }
}

/// T15: a re-armed closePosition whose position the owner then closes by hand (two IOC sells, 64
/// and 22 — each within the cap) is spent at its next fire with `AlgoOrderCancelled(PositionClosed)`:
/// the position's generation moved.
#[test]
fn t15_a_re_armed_close_position_whose_position_ended_is_spent_as_position_closed() {
    let mut ctx = fixture(150);
    bids(&mut ctx, 200, 100);
    let id = arm(&mut ctx, AlgoOrderType::StopMarket, TRIGGER, 0, 0, 0, CP);
    tick(&mut ctx, BID, 31);
    assert_eq!(amount(&mut ctx), 86);
    let generation = storage::load_position(&mut ctx, ALICE, MARKET_ID).unwrap().generation;

    let mut logs = Vec::new();
    for qty in [64, 22] {
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
        logs.extend(shell(&mut ctx, ALICE, &input).1);
    }
    assert_eq!(amount(&mut ctx), 0);
    assert_ne!(storage::load_position(&mut ctx, ALICE, MARKET_ID).unwrap().generation, generation);

    let (more, drained) = drain(&mut ctx);
    logs.extend(drained);
    assert!(!more);
    assert_eq!(algo_cancels(&logs), vec![(id, CancelReason::PositionClosed as u8)]);
    assert_eq!(amount(&mut ctx), 0, "a spent closePosition never opens");
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(armed(&mut ctx, TRIGGER), (vec![], vec![], vec![]));
    assert_eq!(flag(&mut ctx), 0);
}

/// T16: termination. A closePosition long of 1000 against 1000 bids (5 makers × 200). Each capped
/// fire removes exactly 64 makers, so ⌈1000/64⌉ = 16 fires close it: one in the tick, then one per
/// drain — 15 drains, the first 14 answering `moreRemaining = true` and the 15th (40 fills, not
/// capped) `false`.
#[test]
fn t16_a_keeper_drain_loop_terminates_after_ceil_book_over_64_fires() {
    let mut ctx = fixture(1_000);
    bids(&mut ctx, 1_000, 200);
    let id = arm(&mut ctx, AlgoOrderType::StopMarket, TRIGGER, 0, 0, 0, CP);
    let logs = tick(&mut ctx, BID, 31);
    let mut fires = triggered(&logs).len();
    let mut fills = vec![filled(&logs).len()];
    let mut drains = 0;
    loop {
        let (more, logs) = drain(&mut ctx);
        drains += 1;
        fires += triggered(&logs).len();
        fills.push(filled(&logs).len());
        if !more {
            break;
        }
        assert!(drains < 100, "the drain loop must terminate");
    }
    assert_eq!(drains, 15);
    assert_eq!(fires, 16);
    let mut want = vec![64; 15];
    want.push(40);
    assert_eq!(fills, want);
    assert_eq!(amount(&mut ctx), 0);
    assert!(record(&mut ctx, id).is_none());
    assert_eq!(flag(&mut ctx), 0);
    assert_eq!(bid_count(&mut ctx), 0);
}
