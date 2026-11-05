use super::*;
use crate::interface::IPerpDex::{
    placeAlgoOrderCall, AlgoOrderCancelled, AlgoOrderTriggered, OrderPlaced,
};
use crate::types::{AlgoOrderType, CancelReason, TimeInForce};

const RO: u8 = 1;

/// A reduce-only limit STOP sell for ALICE at `price` with `tif`, firing at mark <= 9_900.
fn arm(ctx: &mut TestCtx, price: u64, tif: TimeInForce) -> [u8; 32] {
    let out = crate::trading::algo::run_place_algo_order(
        &placeAlgoOrderCall {
            algoType: 0,
            marketId: MARKET_ID,
            side: Side::Sell as u8,
            orderType: AlgoOrderType::Stop as u8,
            triggerPrice: 9_900,
            price,
            quantity: QTY as u64,
            tif: tif as u8,
            workingType: 0,
            clientAlgoId: FixedBytes::default(),
            flags: RO,
        }
        .abi_encode(),
        ALICE,
        ctx,
    )
    .unwrap();
    out[..32].try_into().unwrap()
}

/// ALICE: a healthy 5x long the stop protects. KEEPER: a thinner long that is BELOW maintenance
/// at the 9_900 mark (equity 140 vs ~165), so the same tick must liquidate it — the sweep serves
/// triggers first, which is exactly what a propagated trigger reject used to cut off. WHALE is
/// the ADL counterparty that lets that liquidation complete without a book.
fn fixture() -> TestCtx {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    seed_position_account(&mut ctx, KEEPER, QTY, -ENTRY_VALUE, 150_000_000, 5, 0);
    seed_absorbing_short(&mut ctx, 1);
    ctx
}

/// The oracle tick, through the shell. It must SUCCEED.
fn tick_through_shell(ctx: &mut TestCtx) {
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
        "a refused trigger must not revert the oracle tx: {:?}",
        String::from_utf8_lossy(&out.bytes)
    );
}

/// The trigger's own events, in stream order, and nothing of the refused ordinary order.
fn assert_trigger_rejected(ctx: &mut TestCtx, id: [u8; 32]) {
    let logs = JournalTr::take_logs(ctx.journal_mut());
    let mine: Vec<&'static str> = logs
        .iter()
        .filter_map(|l| {
            let t = l.data.topics();
            let about_id = t.get(3).map(|x| x.0) == Some(id);
            match t.first() {
                Some(h) if *h == AlgoOrderTriggered::SIGNATURE_HASH && about_id => {
                    Some("triggered")
                }
                Some(h) if *h == AlgoOrderCancelled::SIGNATURE_HASH && about_id => {
                    let e = AlgoOrderCancelled::decode_raw_log(t, &l.data.data).unwrap();
                    assert_eq!(e.reason, CancelReason::TriggerRejected as u8);
                    assert_eq!(e.user, ALICE);
                    Some("cancelled(10)")
                }
                Some(h) if *h == OrderPlaced::SIGNATURE_HASH && about_id => Some("placed"),
                _ => None,
            }
        })
        .collect();
    assert_eq!(
        mine,
        vec!["triggered", "cancelled(10)"],
        "a refused placement is log-clean: its buffered OrderPlaced is never emitted"
    );
    assert!(storage::load_order(ctx, &id).unwrap().is_none(), "the record is deleted, not orphaned");
    assert!(storage::load_algo_down_prices(ctx, MARKET_ID).unwrap().is_empty());
    assert!(storage::load_user_algo_orders(ctx, ALICE, MARKET_ID).unwrap().is_empty());
    assert_eq!(position(ctx, ALICE).amount, QTY, "nothing of the refused order executed");
    assert_eq!(
        position(ctx, KEEPER).amount,
        0,
        "the same tick's liquidation sweep still ran after the refused trigger"
    );
}

/// A fired FOK with nothing to fill against.
#[test]
fn a_fired_fok_that_cannot_fill_is_cancelled() {
    let mut ctx = fixture();
    let id = arm(&mut ctx, 9_950, TimeInForce::Fok);
    tick_through_shell(&mut ctx);
    assert_trigger_rejected(&mut ctx, id);
}

/// A fired PostOnly that would cross a resting bid (in band, so the tick keeps it). The bid is
/// deep enough that KEEPER's liquidation, later in the same tick, cannot exhaust it — so it is
/// still the best bid the PostOnly was refused against.
#[test]
fn a_fired_post_only_that_would_cross_is_cancelled() {
    let mut ctx = fixture();
    place_order(&mut ctx, MAKER, Side::Buy as u8, 9_960, QTY as u64 + 5);
    let id = arm(&mut ctx, 9_950, TimeInForce::PostOnly);
    tick_through_shell(&mut ctx);
    assert_trigger_rejected(&mut ctx, id);
    assert_eq!(storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(), vec![9_960]);
}

/// A fired deep-limit GTC whose rest is refused: an ask far below the band (lower bound 8_910
/// at the 9_900 mark) would become the new best — `RefusedTooGoodBest`.
#[test]
fn a_fired_gtc_whose_rest_is_refused_is_cancelled() {
    let mut ctx = fixture();
    let id = arm(&mut ctx, 8_000, TimeInForce::Gtc);
    tick_through_shell(&mut ctx);
    assert_trigger_rejected(&mut ctx, id);
    assert!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap().is_empty(), "never rested");
}

/// Control: the SAME stop with a placeable price fires and rests — the cancel is reserved for a
/// matcher refusal, it is not what every limit trigger now does.
#[test]
fn a_placeable_fired_gtc_still_rests() {
    let mut ctx = fixture();
    let id = arm(&mut ctx, 9_950, TimeInForce::Gtc);
    tick_through_shell(&mut ctx);
    assert_eq!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(), vec![9_950]);
    assert_eq!(storage::load_order(&mut ctx, &id).unwrap().unwrap().trigger_price, 0);
    let cancelled = JournalTr::take_logs(ctx.journal_mut())
        .iter()
        .filter(|l| l.data.topics().first() == Some(&AlgoOrderCancelled::SIGNATURE_HASH))
        .count();
    assert_eq!(cancelled, 0);
    assert_eq!(position(&mut ctx, KEEPER).amount, 0);
}
