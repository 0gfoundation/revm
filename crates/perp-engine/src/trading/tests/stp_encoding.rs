use super::batch_cancel::{decode_statuses, expect};
use super::*;
use crate::{
    batch::{PerpBatchReason, PerpBatchTag},
    interface::IPerpDex::{
        batchPlaceOrdersCall, getOpenOrdersCall, OrderPlaced, OrderRested, PlaceItem,
    },
    types::StpMode,
};

const MODES: [StpMode; 3] = [StpMode::ExpireMaker, StpMode::ExpireTaker, StpMode::ExpireBoth];

fn place_flags(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8,
    price: u64,
    order_type: u8,
    tif: u8,
    flags: u8,
) -> [u8; 32] {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: QTY,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
    .abi_encode();
    let out = run_perp_dex_call(&input, 30_000_000, caller, U256::ZERO, false, ctx).unwrap();
    assert!(!out.reverted, "{:?}", String::from_utf8_lossy(&out.bytes));
    out.bytes[..32].try_into().unwrap()
}

fn events<E: SolEvent>(ctx: &mut TestCtx) -> Vec<E> {
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|l| l.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|l| E::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .collect()
}

fn get_order_flags(ctx: &mut TestCtx, id: [u8; 32]) -> u8 {
    let ret = run_get_order(
        &getOrderCall { orderId: id.into(), marketId: MARKET_ID }.abi_encode(),
        ctx,
    )
    .unwrap();
    getOrderCall::abi_decode_returns(&ret).unwrap().flags
}

fn open_order_flags(ctx: &mut TestCtx, user: Address, id: [u8; 32]) -> u8 {
    let ret = run_get_open_orders(
        &getOpenOrdersCall { user, marketId: MARKET_ID }.abi_encode(),
        ctx,
    )
    .unwrap();
    let d = getOpenOrdersCall::abi_decode_returns(&ret).unwrap();
    let k = d.orderIds.iter().position(|o| o.0 == id).expect("listed");
    d.flags[k]
}

/// Every echo of every kind of order carries the byte that was sent, for all three modes, alone
/// and combined with reduce-only — and the stored `Order` / `OrderEntry` hold the mode.
#[test]
fn every_echo_carries_the_mode_that_was_sent() {
    for mode in MODES {
        let f = mode.to_flags();
        let mut ctx = make_ctx();
        setup(&mut ctx);
        fund(&mut ctx, ALICE, WALLET * 100);
        fund(&mut ctx, BOB, WALLET * 100);
        place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);
        place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        // GTC and PostOnly REST: OrderPlaced, OrderRested, getOrder, getOpenOrders, storage.
        for (tif, price) in [(0u8, PRICE - TICK), (3u8, PRICE - 2 * TICK)] {
            let id = place_flags(&mut ctx, ALICE, 0, price, 0, tif, f);
            let logs = JournalTr::take_logs(ctx.journal_mut());
            let placed: Vec<u8> = logs
                .iter()
                .filter(|l| l.data.topics().first() == Some(&OrderPlaced::SIGNATURE_HASH))
                .map(|l| OrderPlaced::decode_raw_log(l.data.topics(), &l.data.data).unwrap().flags)
                .collect();
            let rested: Vec<u8> = logs
                .iter()
                .filter(|l| l.data.topics().first() == Some(&OrderRested::SIGNATURE_HASH))
                .map(|l| OrderRested::decode_raw_log(l.data.topics(), &l.data.data).unwrap().flags)
                .collect();
            assert_eq!((placed, rested), (vec![f], vec![f]), "{mode:?} tif {tif}");
            assert_eq!(get_order_flags(&mut ctx, id), f, "{mode:?} getOrder");
            assert_eq!(open_order_flags(&mut ctx, ALICE, id), f, "{mode:?} getOpenOrders");
            assert_eq!(get_order(&mut ctx, id).stp, mode);
            let entry = storage::load_buy_orders(&mut ctx, ALICE, MARKET_ID)
                .unwrap()
                .into_iter()
                .find(|e| e.order_id == id)
                .unwrap();
            assert_eq!(entry.stp, mode, "the ENTRY mirrors it (getOpenOrders reads only this)");
        }

        // FOK and market take BOB's asks and leave no record: OrderPlaced is their only echo.
        place_flags(&mut ctx, ALICE, 0, PRICE + TICK, 0, 2, f);
        place_flags(&mut ctx, ALICE, 0, 0, 1, 0, f);
        let placed: Vec<u8> = events::<OrderPlaced>(&mut ctx).into_iter().map(|e| e.flags).collect();
        assert_eq!(placed, vec![f, f], "{mode:?} FOK + market");
        assert_eq!(pos(&mut ctx, ALICE).amount, 2 * QTY as i64, "both filled");

        // Reduce-only and the mode are independent bits of one byte.
        let ro = f | 1;
        let id = place_flags(&mut ctx, ALICE, 1, PRICE + 5 * TICK, 0, 0, ro);
        assert_eq!(get_order_flags(&mut ctx, id), ro, "{mode:?} reduce-only getOrder");
        assert_eq!(open_order_flags(&mut ctx, ALICE, id), ro);
        let o = get_order(&mut ctx, id);
        assert!(o.reduce_only);
        assert_eq!(o.stp, mode);
    }
}

/// In a batch, the reserved mode is a per-item `Rejected` with its OWN reason (34) — not the
/// unknown-bits catch-all — and the batch carries on.
#[test]
fn a_batch_item_with_the_reserved_mode_is_rejected_with_its_own_reason() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let base = storage::load_user_nonce(&mut ctx, ALICE).unwrap();
    let item = |flags: u8| PlaceItem {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE - TICK,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
        flags,
    };
    let input = batchPlaceOrdersCall { orders: vec![item(0x0c), item(0x08), item(0x02)] }.abi_encode();
    let out = run_perp_dex_call(&input, 30_000_000, ALICE, U256::ZERO, false, &mut ctx).unwrap();
    assert!(!out.reverted);
    let blob = batchPlaceOrdersCall::abi_decode_returns(&out.bytes).unwrap().to_vec();
    let id = derive_order_id(ALICE, base);
    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(PerpBatchTag::Rejected, [0u8; 32], PerpBatchReason::InvalidStpMode),
            expect(PerpBatchTag::Accepted, id, PerpBatchReason::None),
            expect(PerpBatchTag::Rejected, [0u8; 32], PerpBatchReason::Other),
        ]
    );
    assert_eq!(get_order(&mut ctx, id).stp, StpMode::ExpireBoth);
}
