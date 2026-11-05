use super::*;
use crate::interface::IPerpDex::{
    placeAlgoOrderCall, AlgoOrderCancelled, AlgoOrderTriggered, OrderCancelled, OrderExpired,
    OrderPlaced, Trade,
};
use crate::types::{AlgoOrderType, CancelReason, StpMode, TimeInForce};

const RO: u8 = 1;
const OWN_BID: u64 = 9_950;
const MAKER_BID: u64 = 9_940;

fn fixture(own_bid: bool) -> (TestCtx, Option<[u8; 32]>) {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    let own = own_bid.then(|| {
        let out = run_place_order(
            &placeOrderCall {
                marketId: MARKET_ID,
                side: Side::Buy as u8,
                price: OWN_BID,
                quantity: 2,
                orderType: 0,
                tif: 0,
                clientOrderId: FixedBytes::default(),
                flags: 0,
            }
            .abi_encode(),
            ALICE,
            &mut ctx,
        )
        .unwrap();
        out[..32].try_into().unwrap()
    });
    (ctx, own)
}

/// A reduce-only STOP sell for ALICE (a limit `Stop` at `price`/`tif`, or a `StopMarket` when
/// `price == 0`), firing at mark <= 9_900, carrying `mode`.
fn arm(ctx: &mut TestCtx, price: u64, tif: TimeInForce, mode: StpMode) -> [u8; 32] {
    let order_type = if price == 0 { AlgoOrderType::StopMarket } else { AlgoOrderType::Stop };
    let out = crate::trading::algo::run_place_algo_order(
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: Side::Sell as u8,
            orderType: order_type as u8,
            triggerPrice: 9_900,
            price,
            quantity: QTY as u64,
            tif: tif as u8,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO | mode.to_flags(),
        }
        .abi_encode(),
        ALICE,
        ctx,
    )
    .unwrap();
    out[..32].try_into().unwrap()
}

/// The oracle tick, through the shell. It must SUCCEED; returns its logs.
fn tick(ctx: &mut TestCtx) -> Vec<primitives::Log> {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let out = run_perp_dex_call(
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: 9_900, timestamp: 31 }
            .abi_encode(),
        30_000_000,
        ADMIN,
        U256::ZERO,
        false,
        ctx,
    )
    .unwrap();
    assert!(
        !out.reverted,
        "the oracle tx must succeed: {:?}",
        String::from_utf8_lossy(&out.bytes)
    );
    JournalTr::take_logs(ctx.journal_mut())
}

/// The order-lifecycle events of the stream, by name, with the cancel / expiry reasons inline.
fn lifecycle(logs: &[primitives::Log]) -> Vec<String> {
    logs.iter()
        .filter_map(|l| {
            let t = l.data.topics();
            let h = *t.first()?;
            if h == AlgoOrderTriggered::SIGNATURE_HASH {
                Some("AlgoOrderTriggered".into())
            } else if h == AlgoOrderCancelled::SIGNATURE_HASH {
                let e = AlgoOrderCancelled::decode_raw_log(t, &l.data.data).unwrap();
                Some(format!("AlgoOrderCancelled({})", e.reason))
            } else if h == OrderPlaced::SIGNATURE_HASH {
                Some("OrderPlaced".into())
            } else if h == OrderCancelled::SIGNATURE_HASH {
                let e = OrderCancelled::decode_raw_log(t, &l.data.data).unwrap();
                Some(format!("OrderCancelled({})", e.reason))
            } else if h == OrderExpired::SIGNATURE_HASH {
                let e = OrderExpired::decode_raw_log(t, &l.data.data).unwrap();
                Some(format!(
                    "OrderExpired({},{},{})",
                    e.filledQuantity, e.expiredQuantity, e.reason
                ))
            } else if h == Trade::SIGNATURE_HASH {
                Some("Trade".into())
            } else {
                None
            }
        })
        .collect()
}

fn live(ctx: &mut TestCtx, id: [u8; 32]) -> bool {
    storage::load_order(ctx, &id).unwrap().is_some()
}

/// T15 — a fired reduce-only stop-MARKET inherits its mode. ALICE's own bid sits at the top of
/// the book, a third party's behind it.
/// * EXPIRE_TAKER: the walk stops at her bid — the close expires with reason 9, her bid stays,
///   MAKER is untouched, the position is still open.
/// * EXPIRE_MAKER (the default): her bid is cancelled (reason 9) and the close fills MAKER.
/// * EXPIRE_BOTH: her bid is cancelled AND the close expires.
#[test]
fn t15_a_fired_conditional_inherits_its_stp_mode() {
    let stp = CancelReason::SelfTradePrevention as u8;
    for mode in [StpMode::ExpireTaker, StpMode::ExpireMaker, StpMode::ExpireBoth] {
        let (mut ctx, own) = fixture(true);
        let own = own.unwrap();
        place_order(&mut ctx, MAKER, Side::Buy as u8, MAKER_BID, QTY as u64);
        // A `*_MARKET` conditional sends tif 0 (unused).
        arm(&mut ctx, 0, TimeInForce::Gtc, mode);
        let logs = tick(&mut ctx);
        let q = QTY as u64;
        let (want, own_live, amount) = match mode {
            StpMode::ExpireTaker => (
                vec![
                    "AlgoOrderTriggered".into(),
                    "OrderPlaced".into(),
                    format!("OrderExpired(0,{q},{stp})"),
                ],
                true,
                QTY,
            ),
            StpMode::ExpireMaker => (
                vec![
                    "AlgoOrderTriggered".into(),
                    "OrderPlaced".into(),
                    format!("OrderCancelled({stp})"),
                    "Trade".into(),
                ],
                false,
                0,
            ),
            StpMode::ExpireBoth => (
                vec![
                    "AlgoOrderTriggered".into(),
                    "OrderPlaced".into(),
                    format!("OrderCancelled({stp})"),
                    format!("OrderExpired(0,{q},{stp})"),
                ],
                false,
                QTY,
            ),
        };
        assert_eq!(lifecycle(&logs), want, "{mode:?}");
        assert_eq!(live(&mut ctx, own), own_live, "{mode:?}");
        assert_eq!(position(&mut ctx, ALICE).amount, amount, "{mode:?}");
    }
}

/// T25's mode variants — a fired DEEP-limit GTC (8_000, below the band's 8_910 floor at the
/// 9_900 mark), so if it rests it is refused as a too-good best.
///
/// Without an own order on the path the mode is moot: every mode rests → refused →
/// `AlgoOrderCancelled(10)`. With ALICE's own bid on the path the mode decides:
/// * EXPIRE_MAKER cancels the bid and walks on, then RESTS → refused. The whole placement is
///   discarded write-clean, the STP cancel with it: her bid is still there and no
///   `OrderCancelled` was ever emitted.
/// * EXPIRE_TAKER / EXPIRE_BOTH stop at the bid and EXPIRE the remainder (reason 9) — the
///   placement is accepted, nothing rests, so nothing is refused and the trigger is spent
///   normally.
#[test]
fn t25_a_fired_deep_gtc_under_each_mode_with_and_without_an_own_order() {
    let stp = CancelReason::SelfTradePrevention as u8;
    let rejected = CancelReason::TriggerRejected as u8;
    let q = QTY as u64;
    for own_bid in [false, true] {
        for mode in [StpMode::ExpireMaker, StpMode::ExpireTaker, StpMode::ExpireBoth] {
            let (mut ctx, own) = fixture(own_bid);
            let id = arm(&mut ctx, 8_000, TimeInForce::Gtc, mode);
            let logs = tick(&mut ctx);
            let case = format!("own bid {own_bid}, {mode:?}");
            let refused = vec![
                "AlgoOrderTriggered".to_string(),
                format!("AlgoOrderCancelled({rejected})"),
            ];
            let want = match (own_bid, mode) {
                (false, _) | (true, StpMode::ExpireMaker) => refused,
                (true, StpMode::ExpireTaker) => vec![
                    "AlgoOrderTriggered".into(),
                    "OrderPlaced".into(),
                    format!("OrderExpired(0,{q},{stp})"),
                ],
                (true, StpMode::ExpireBoth) => vec![
                    "AlgoOrderTriggered".into(),
                    "OrderPlaced".into(),
                    format!("OrderCancelled({stp})"),
                    format!("OrderExpired(0,{q},{stp})"),
                ],
            };
            assert_eq!(lifecycle(&logs), want, "{case}");
            // The trigger is spent either way, and nothing ever rests.
            assert!(!live(&mut ctx, id), "{case}");
            assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID)
                .unwrap()
                .is_empty());
            assert!(
                storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
                "{case}"
            );
            assert_eq!(position(&mut ctx, ALICE).amount, QTY, "{case}");
            if let Some(own) = own {
                assert_eq!(live(&mut ctx, own), mode != StpMode::ExpireBoth, "{case}");
            }
        }
    }
}
