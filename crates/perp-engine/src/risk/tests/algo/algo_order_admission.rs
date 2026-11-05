use super::*;
use crate::interface::IPerpDex::{cancelAlgoOrderCall, placeAlgoOrderCall};
use crate::types::AlgoOrderType;

const RO: u8 = 1; // reduceOnly
const CP: u8 = 2; // closePosition

#[allow(clippy::too_many_arguments)]
fn place(
    ctx: &mut TestCtx,
    side: Side,
    kind: AlgoOrderType,
    trigger: u64,
    price: u64,
    qty: u64,
    tif: u8,
    working_type: u8,
    flags: u8,
) -> Result<Bytes, PerpError> {
    crate::trading::algo::run_place_algo_order(
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: side as u8,
            orderType: kind as u8,
            triggerPrice: trigger,
            price,
            quantity: qty,
            tif,
            workingType: working_type,
            clientAlgoId: FixedBytes::default(),
            flags,
        }
        .abi_encode(),
        ALICE,
        ctx,
    )
}

/// A reduce-only STOP_MARKET — the canonical stop-loss — placed, listed and cancelled.
#[test]
fn a_stop_market_round_trips() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let out =
        place(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 8_000, 0, QTY as u64, 0, 0, RO)
            .unwrap();
    let algo_id: [u8; 32] = out[..32].try_into().unwrap();

    assert_eq!(
        storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap(),
        vec![algo_id]
    );
    // A SELL stop fires when the mark FALLS to the trigger, so it lives in the down index.
    assert_eq!(
        storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![8_000]
    );
    assert!(
        storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
        "and nowhere near the up index"
    );
    // ⚠️ It must NOT be in the book: an untriggered conditional has no price level, no queue
    // position and no open-order requirement.
    assert!(storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
    assert!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap().is_empty());

    crate::trading::algo::run_cancel_algo_order(
        &cancelAlgoOrderCall { algoId: FixedBytes(algo_id), marketId: MARKET_ID }.abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap();
    assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
    assert!(storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
    assert!(storage::load_order(&mut ctx, &algo_id).unwrap().is_none());
}

/// The trigger-direction matrix — the one thing a reader cannot verify by inspection, because
/// four (type, side) pairs collapse onto two index directions.
#[test]
fn all_four_type_side_pairs_land_in_the_right_direction() {
    for (kind, side, up) in [
        (AlgoOrderType::StopMarket, Side::Buy, true),
        (AlgoOrderType::StopMarket, Side::Sell, false),
        (AlgoOrderType::TakeProfitMarket, Side::Buy, false),
        (AlgoOrderType::TakeProfitMarket, Side::Sell, true),
    ] {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        // On the side of the $100 mark it has NOT reached yet — the other side is refused as
        // "would immediately trigger".
        let trigger = if up { 12_000 } else { 8_000 };
        place(&mut ctx, side, kind, trigger, 0, QTY as u64, 0, 0, RO).unwrap();
        let in_up = !storage::load_algo_up_prices(&mut ctx, MARKET_ID).unwrap().is_empty();
        assert_eq!(
            in_up, up,
            "{kind:?} {side:?} must fire on mark {} trigger",
            if up { ">=" } else { "<=" }
        );
    }
}

/// ⚠️ The deliberate divergence, asserted so it cannot be softened into a silent substitution:
/// Binance's DEFAULT workingType is CONTRACT_PRICE, so a client ported from there which omits
/// the field lands here, and it must be TOLD rather than quietly triggered off the mark.
#[test]
fn contract_price_working_type_is_rejected_not_substituted() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let e = place(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 8_000, 0, QTY as u64, 0, 1, RO)
        .unwrap_err();
    assert!(format!("{e:?}").contains("MARK_PRICE"), "got {e:?}");
}

#[test]
fn rejections() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let bad = |ctx: &mut TestCtx, c: placeAlgoOrderCall, want: &str| {
        let e = crate::trading::algo::run_place_algo_order(&c.abi_encode(), ALICE, ctx)
            .unwrap_err();
        assert!(format!("{e:?}").contains(want), "want {want:?}, got {e:?}");
    };
    let base = placeAlgoOrderCall {
        algoType: 0,
        marketId: MARKET_ID,
        side: Side::Sell as u8,
        orderType: AlgoOrderType::StopMarket as u8,
        triggerPrice: 8_000,
        price: 0,
        quantity: QTY as u64,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags: RO,
    };
    bad(&mut ctx, placeAlgoOrderCall { algoType: 1, ..base.clone() }, "CONDITIONAL");
    // 4 is TRAILING_STOP_MARKET now; past it, nothing. A trailing stop's `price` slot is its
    // callback rate, so this one's `price: 0` is what is refused (see `trailing::tests`).
    bad(&mut ctx, placeAlgoOrderCall { orderType: 5, ..base.clone() }, "invalid orderType");
    bad(&mut ctx, placeAlgoOrderCall { orderType: 4, ..base.clone() }, "callbackRate");
    bad(&mut ctx, placeAlgoOrderCall { triggerPrice: 0, ..base.clone() }, "triggerPrice");
    bad(&mut ctx, placeAlgoOrderCall { flags: 0, ..base.clone() }, "reduceOnly or closePosition");
    // Bits 3:2 are the STP mode now, so 0b1000 alone is a legal mode with no reduce-only bit —
    // refused for THAT, not as an unknown bit. The RO bit is kept on the next two so the
    // reduce-only scope check cannot mask the error under test.
    bad(&mut ctx, placeAlgoOrderCall { flags: 0b1000, ..base.clone() }, "reduceOnly or closePosition");
    bad(&mut ctx, placeAlgoOrderCall { flags: 0b1101, ..base.clone() }, "invalid self-trade prevention mode");
    bad(&mut ctx, placeAlgoOrderCall { flags: 0b1_0001, ..base.clone() }, "unknown flag");
    // closePosition is *_MARKET only, and excludes an explicit quantity.
    bad(
        &mut ctx,
        placeAlgoOrderCall { orderType: AlgoOrderType::Stop as u8, price: 8_000, flags: CP, ..base.clone() },
        "closePosition requires",
    );
    bad(&mut ctx, placeAlgoOrderCall { flags: CP, ..base.clone() }, "exclusive");
    bad(&mut ctx, placeAlgoOrderCall { quantity: 0, ..base.clone() }, "quantity must be non-zero");
    // A *_MARKET type carries no resting price and no tif.
    bad(&mut ctx, placeAlgoOrderCall { price: 8_000, ..base.clone() }, "price must be 0");
    bad(&mut ctx, placeAlgoOrderCall { tif: 1, ..base.clone() }, "tif is unused");
    // A limit type must carry one.
    bad(
        &mut ctx,
        placeAlgoOrderCall { orderType: AlgoOrderType::Stop as u8, price: 0, ..base.clone() },
        "price is required",
    );
}

/// The per-market cap is not cosmetic: liquidation's and ADL's cancel-all walk this list from
/// inside an already-capped sweep, so an unbounded list would put unbounded work there.
#[test]
fn the_per_market_cap_is_enforced() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    for i in 0..crate::trading::algo::MAX_ALGO_ORDERS_PER_MARKET {
        // All below the $100 mark, so none would trigger immediately.
        place(
            &mut ctx,
            Side::Sell,
            AlgoOrderType::StopMarket,
            5_000 + (i as u64 + 1) * 100,
            0,
            QTY as u64,
            0,
            0,
            RO,
        )
        .unwrap();
    }
    let e = place(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 7_000, 0, QTY as u64, 0, 0, RO)
        .unwrap_err();
    assert!(format!("{e:?}").contains("too many open conditional"), "got {e:?}");
}

/// Liquidation and ADL both clear a market's book for the owner. A conditional order is NOT in
/// the book, so it has to be cleared explicitly — otherwise a liquidated account keeps a live
/// stop-loss aimed at a position that no longer exists, which would fire against whatever it
/// opens next.
#[test]
fn liquidation_cancels_conditional_orders_too() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    seed_absorbing_short(&mut ctx, 1);
    place(&mut ctx, Side::Sell, AlgoOrderType::StopMarket, 8_000, 0, QTY as u64, 0, 0, RO)
        .unwrap();
    assert_eq!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(), 1);

    oracle_tick(&mut ctx, 7_500, 31);

    assert_eq!(position(&mut ctx, ALICE).amount, 0, "fixture: ALICE really was liquidated");
    assert!(
        storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty(),
        "her conditional orders must go with the position"
    );
    assert!(
        storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
        "and leave the market trigger index, not just her own list"
    );
}
