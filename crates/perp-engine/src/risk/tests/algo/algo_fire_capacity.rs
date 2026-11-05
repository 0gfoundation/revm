use super::*;
use crate::interface::IPerpDex::{
    AlgoOrderCancelled, OrderCancelled, OrderPlaced, placeAlgoOrderCall,
};
use crate::types::{AlgoOrderType, CancelReason, TimeInForce};

const RO: u8 = 1;
const CP: u8 = 2;

/// Which way ALICE is exposed. Every scenario runs on both, because the two closing sides rank
/// the queue in opposite directions (`distance_from_touch`), and a capacity rule that is right
/// for one and wrong for the other is exactly the shape of the priority bug.
#[derive(Clone, Copy, Debug)]
enum Exposure {
    Long,
    Short,
}

/// One bracket's prices. LONG: the TP sells ABOVE the mark, the stop sells when the mark falls
/// to 99, and a bid at 98.50 is the in-band liquidity it closes into. SHORT is the mirror. At
/// a 99 / 101 mark this 5x position is still above maintenance, so the trigger — not the
/// liquidation sweep — is what closes it.
struct Bracket {
    close_side: Side,
    tp_price: u64,
    trigger: u64,
    tick: u64,
    liquidity_price: u64,
}

fn bracket(exposure: Exposure) -> Bracket {
    match exposure {
        Exposure::Long => Bracket {
            close_side: Side::Sell,
            tp_price: 10_500,
            trigger: 9_900,
            tick: 9_900,
            liquidity_price: 9_850,
        },
        Exposure::Short => Bracket {
            close_side: Side::Buy,
            tp_price: 9_500,
            trigger: 10_100,
            tick: 10_100,
            liquidity_price: 10_150,
        },
    }
}

/// ALICE 10 contracts the given way at $100, 5x, and MAKER resting the opposite side at the
/// bracket's liquidity price for the full size.
fn seed(ctx: &mut TestCtx, exposure: Exposure) -> Bracket {
    setup_market(ctx);
    let (amount, v_quote) = match exposure {
        Exposure::Long => (QTY, -ENTRY_VALUE),
        Exposure::Short => (-QTY, ENTRY_VALUE),
    };
    seed_position_account(ctx, ALICE, amount, v_quote, MARGIN, 5, USER_WALLET as i64);
    let b = bracket(exposure);
    place_order(ctx, MAKER, b.close_side.opposite() as u8, b.liquidity_price, QTY as u64);
    b
}

/// The take-profit half: a reduce-only LIMIT resting in the book on the closing side.
fn rest_take_profit(ctx: &mut TestCtx, b: &Bracket, qty: u64) -> [u8; 32] {
    let out = run_place_order(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: b.close_side as u8,
            price: b.tp_price,
            quantity: qty,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
            flags: RO,
        }
        .abi_encode(),
        ALICE,
        ctx,
    )
    .unwrap();
    out[..32].try_into().unwrap()
}

#[allow(clippy::too_many_arguments)]
fn arm(
    ctx: &mut TestCtx,
    side: Side,
    kind: AlgoOrderType,
    trigger: u64,
    price: u64,
    qty: u64,
    tif: TimeInForce,
    flags: u8,
) -> [u8; 32] {
    let out = crate::trading::algo::run_place_algo_order(
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: side as u8,
            orderType: kind as u8,
            triggerPrice: trigger,
            price,
            quantity: qty,
            tif: tif as u8,
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

/// The stop-loss half, in either of its two spellings.
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// `closePosition` STOP_MARKET: no quantity, the whole position at trigger time.
    ClosePosition,
    /// reduce-only STOP_MARKET for an explicit 10.
    Quantity,
}

fn arm_stop(ctx: &mut TestCtx, b: &Bracket, stop: Stop) -> [u8; 32] {
    let (qty, flags) = match stop {
        Stop::ClosePosition => (0, CP),
        Stop::Quantity => (QTY as u64, RO),
    };
    arm(ctx, b.close_side, AlgoOrderType::StopMarket, b.trigger, 0, qty, TimeInForce::Gtc, flags)
}

/// Everything one oracle tick says about the stop and the take-profit.
struct Fired {
    /// `OrderPlaced.quantity` of the order the stop became; `None` = nothing was placed.
    placed: Option<u64>,
    /// `AlgoOrderCancelled.reason` for the stop, if it was spent without a placement.
    stop_cancelled: Option<u8>,
    /// `OrderCancelled.reason` for the take-profit, if the eviction pass removed it.
    tp_cancelled: Option<u8>,
}

fn tick(ctx: &mut TestCtx, b: &Bracket, stop: [u8; 32], tp: [u8; 32]) -> Fired {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    oracle_tick(ctx, b.tick, 31);
    let logs = JournalTr::take_logs(ctx.journal_mut());
    Fired {
        placed: decode_events::<OrderPlaced>(&logs)
            .into_iter()
            .find(|e| e.orderId.0 == stop)
            .map(|e| e.quantity),
        stop_cancelled: decode_events::<AlgoOrderCancelled>(&logs)
            .into_iter()
            .find(|e| e.algoId.0 == stop)
            .map(|e| e.reason),
        tp_cancelled: decode_events::<OrderCancelled>(&logs)
            .into_iter()
            .find(|e| e.orderId.0 == tp)
            .map(|e| e.reason),
    }
}

/// THE bracket, at full size: long 10, reduce-only TP for 10 resting, stop fires. The stop
/// closes all 10, and the TP — which would now OPEN the other way — is evicted after the fill.
#[test]
fn a_stop_beside_a_full_size_reduce_only_take_profit_closes_the_whole_position() {
    for exposure in [Exposure::Long, Exposure::Short] {
        for stop in [Stop::ClosePosition, Stop::Quantity] {
            let mut ctx = make_ctx();
            let b = seed(&mut ctx, exposure);
            let tp = rest_take_profit(&mut ctx, &b, QTY as u64);
            let sl = arm_stop(&mut ctx, &b, stop);

            let fired = tick(&mut ctx, &b, sl, tp);

            let case = format!("{exposure:?} / {stop:?}");
            assert_eq!(
                fired.stop_cancelled, None,
                "{case}: the stop must PLACE, not expire for 'no capacity'"
            );
            assert_eq!(fired.placed, Some(QTY as u64), "{case}: sized by |position| alone");
            assert_eq!(position(&mut ctx, ALICE).amount, 0, "{case}: the stop closed it");
            assert_eq!(
                fired.tp_cancelled,
                Some(CancelReason::ReduceOnlyWouldOpen as u8),
                "{case}: flat now, so the TP is on the opening side and the eviction pass \
                 removes it"
            );
            assert!(storage::load_order(&mut ctx, &tp).unwrap().is_none(), "{case}");
        }
    }
}

/// The half-size bracket, which the old rule mis-sized rather than killed: it closed only the
/// 5 the TP did not "reserve". The stop closes all 10 and the TP goes the same way.
#[test]
fn a_stop_beside_a_half_size_reduce_only_take_profit_still_closes_everything() {
    for exposure in [Exposure::Long, Exposure::Short] {
        for stop in [Stop::ClosePosition, Stop::Quantity] {
            let mut ctx = make_ctx();
            let b = seed(&mut ctx, exposure);
            let tp = rest_take_profit(&mut ctx, &b, QTY as u64 / 2);
            let sl = arm_stop(&mut ctx, &b, stop);

            let fired = tick(&mut ctx, &b, sl, tp);

            let case = format!("{exposure:?} / {stop:?}");
            assert_eq!(fired.placed, Some(QTY as u64), "{case}: 10, not 10 − 5");
            assert_eq!(position(&mut ctx, ALICE).amount, 0, "{case}");
            assert_eq!(
                fired.tp_cancelled,
                Some(CancelReason::ReduceOnlyWouldOpen as u8),
                "{case}"
            );
        }
    }
}

/// A limit stop whose fired order is IOC is just as immediate as a market one — it fills before
/// anything its owner has resting — so it is bounded by the position alone too. The rule keys
/// on the order KIND (`QueuePosition::for_kind`), not on "market vs limit".
#[test]
fn a_fired_limit_ioc_is_immediate_too() {
    for exposure in [Exposure::Long, Exposure::Short] {
        let mut ctx = make_ctx();
        let b = seed(&mut ctx, exposure);
        let tp = rest_take_profit(&mut ctx, &b, QTY as u64);
        // STOP (limit) at the liquidity price, IOC: crosses the maker and nothing rests.
        let sl = arm(
            &mut ctx,
            b.close_side,
            AlgoOrderType::Stop,
            b.trigger,
            b.liquidity_price,
            QTY as u64,
            TimeInForce::Ioc,
            RO,
        );

        let fired = tick(&mut ctx, &b, sl, tp);

        assert_eq!(fired.placed, Some(QTY as u64), "{exposure:?}");
        assert_eq!(position(&mut ctx, ALICE).amount, 0, "{exposure:?}");
        assert_eq!(
            fired.tp_cancelled,
            Some(CancelReason::ReduceOnlyWouldOpen as u8),
            "{exposure:?}"
        );
    }
}

/// The other half of the rule, unchanged: a fired limit order that RESTS (GTC) takes a place in
/// its owner's queue, so the prefix condition applies to it at its own limit price — exactly as
/// it would to the same order sent through `placeOrder`. Here the TP already commits the whole
/// position, so the stop finds no room and is spent; the TP and the position are untouched.
#[test]
fn a_fired_limit_that_would_rest_is_still_held_to_the_prefix_condition() {
    for exposure in [Exposure::Long, Exposure::Short] {
        let mut ctx = make_ctx();
        let b = seed(&mut ctx, exposure);
        let tp = rest_take_profit(&mut ctx, &b, QTY as u64);
        // A GTC limit just inside the TP — nearer the touch, so the full-size TP behind it
        // leaves it nothing.
        let rest_at = match exposure {
            Exposure::Long => b.tp_price - 100,
            Exposure::Short => b.tp_price + 100,
        };
        let sl = arm(
            &mut ctx,
            b.close_side,
            AlgoOrderType::Stop,
            b.trigger,
            rest_at,
            QTY as u64,
            TimeInForce::Gtc,
            RO,
        );

        let fired = tick(&mut ctx, &b, sl, tp);

        assert_eq!(fired.placed, None, "{exposure:?}: no room in the prefix");
        assert_eq!(
            fired.stop_cancelled,
            Some(CancelReason::ReduceOnlyWouldOpen as u8),
            "{exposure:?}"
        );
        assert_eq!(position(&mut ctx, ALICE).amount.unsigned_abs(), QTY as u64, "{exposure:?}");
        assert_eq!(fired.tp_cancelled, None, "{exposure:?}: the TP is untouched");
        assert!(storage::load_order(&mut ctx, &tp).unwrap().is_some(), "{exposure:?}");
    }
}
