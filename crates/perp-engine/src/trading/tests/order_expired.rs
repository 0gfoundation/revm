use super::*;
use crate::interface::IPerpDex::OrderExpired;

/// Every `OrderExpired` in the journal, decoded, paired with the full event-name sequence so
/// ORDERING can be asserted in the same breath as the payload. Drains the journal once —
/// `take_event_names` would consume the logs this needs to decode.
fn take_expiries(ctx: &mut TestCtx) -> (Vec<&'static str>, Vec<OrderExpired>) {
    let logs = JournalTr::take_logs(ctx.journal_mut());
    let expiries = logs
        .iter()
        .filter(|log| log.data.topics().first() == Some(&OrderExpired::SIGNATURE_HASH))
        .map(|log| OrderExpired::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect();
    (
        crate::events::stream_test_support::stream_shape(&logs)
            .into_iter()
            .map(|(n, _)| n)
            .collect(),
        expiries,
    )
}

/// THE CASE THAT HAD NO TERMINAL EVENT. An IOC that fills part of its quantity now closes with
/// one, AFTER its fills — which is the measured Binance frame order (R15: the last fill still
/// reports `PARTIALLY_FILLED`; the terminal state arrives as a separate, fill-less frame).
#[test]
fn a_partially_filled_ioc_emits_one_expiry_after_its_last_trade() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // BOB's ask: only QTY available
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let id = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 1); // IOC buy for twice that

    let (names, expiries) = take_expiries(&mut ctx);
    assert_eq!(expiries.len(), 1, "exactly one terminal event, got {names:?}");
    let e = &expiries[0];
    assert_eq!(e.user, ALICE);
    assert_eq!(e.marketId, MARKET_ID);
    assert_eq!(e.orderId.0, id);
    assert_eq!(
        (e.filledQuantity, e.expiredQuantity),
        (QTY, QTY),
        "half executed, half discarded"
    );
    assert_eq!(
        e.filledQuantity + e.expiredQuantity,
        QTY * 2,
        "the two quantities must reconstruct the submitted quantity — that is what lets a \
         consumer reconcile `z` without summing Trade logs it hopes it received all of"
    );

    // ORDERING is the load-bearing half. `OrderPlaced` first, the fill in the middle, the
    // terminal event LAST — never annotated onto the fill, and never before its own placement.
    assert_eq!(names.first().copied(), Some("OrderPlaced"), "{names:?}");
    assert_eq!(names.last().copied(), Some("OrderExpired"), "{names:?}");
    let expiry_at = names.iter().position(|n| *n == "OrderExpired").unwrap();
    let trade_at = names.iter().position(|n| *n == "Trade").expect("a fill ran");
    assert!(trade_at < expiry_at, "the expiry follows the fills: {names:?}");
    assert!(
        !names.contains(&"OrderRested"),
        "an IOC remainder never rests — that is why this is not an OrderCancelled: {names:?}"
    );
    assert!(
        !names.contains(&"OrderCancelled"),
        "and it must NOT be reported as a cancel (the projector regression): {names:?}"
    );

    // delete-on-terminal: the record is gone, so this event is the ONLY remaining evidence of
    // how the order ended.
    assert_terminal(&mut ctx, id);
}

/// Same for a market order, which is the second and last call site of
/// `cancel_unfilled_remainder`.
#[test]
fn a_partially_filled_market_order_emits_one_expiry() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let id = place(&mut ctx, ALICE, 0, 0, QTY * 2, 1, 1); // market buy, price ignored

    let (names, expiries) = take_expiries(&mut ctx);
    assert_eq!(expiries.len(), 1, "{names:?}");
    assert_eq!(expiries[0].orderId.0, id);
    assert_eq!((expiries[0].filledQuantity, expiries[0].expiredQuantity), (QTY, QTY));
    assert_eq!(names.last().copied(), Some("OrderExpired"), "{names:?}");
}

/// THE NON-FIRING SET, all four branches in one place — because the value of this event is that
/// its firing set is a fact about the engine rather than a rule each consumer re-derives.
///
/// A FULL FILL has no remainder (its terminal state is already unambiguous from `z == q`); a
/// GTC remainder RESTS; a FOK REVERTS rather than expiring; and a PostOnly either rests or
/// rejects.
#[test]
fn nothing_else_expires() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    // ── 1. A fully filled IOC. Remainder 0 → `cancel_unfilled_remainder` does nothing.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 1); // IOC, exactly fillable
    let (names, expiries) = take_expiries(&mut ctx);
    assert!(
        expiries.is_empty(),
        "a fully filled IOC has no remainder to expire: {names:?}"
    );

    // ── 2. A GTC that partially fills RESTS its remainder.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 0); // GTC
    let (names, expiries) = take_expiries(&mut ctx);
    assert!(
        expiries.is_empty(),
        "a GTC remainder rests, it does not expire: {names:?}"
    );
    assert!(names.contains(&"OrderRested"), "{names:?}");

    // ── 3. A FOK that cannot complete REVERTS. `ensure_fok_filled` (and the pre-check
    //       `check_fok_feasibility` before it) return Err, so the whole order is rejected and
    //       there is no partially-filled FOK for a remainder to belong to.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let err = run_place_order(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: 0,
            price: PRICE,
            quantity: QTY * 2, // more than the book holds
            orderType: 0,
            tif: 2, // FOK
            clientOrderId: FixedBytes::default(),
                        flags: 0,
        }
        .abi_encode(),
        ALICE,
        &mut ctx,
    );
    assert!(err.is_err(), "an unfillable FOK must reject, not expire");
    let (names, expiries) = take_expiries(&mut ctx);
    assert!(expiries.is_empty(), "a rejected FOK emits no expiry: {names:?}");

    // ── 4. PostOnly rests (it never matches at all), so likewise no expiry.
    let _ = JournalTr::take_logs(ctx.journal_mut());
    place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 3); // PostOnly, below the ask
    let (names, expiries) = take_expiries(&mut ctx);
    assert!(expiries.is_empty(), "PostOnly rests: {names:?}");
    assert!(names.contains(&"OrderRested"), "{names:?}");
}

/// A zero-fill expiry still reports honest quantities — `filledQuantity` 0 and the whole order
/// discarded — rather than being suppressed for having no fills.
#[test]
fn a_zero_fill_expiry_reports_the_whole_quantity() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 1); // IOC against an empty book

    let (names, expiries) = take_expiries(&mut ctx);
    assert_eq!(expiries.len(), 1, "{names:?}");
    assert_eq!((expiries[0].filledQuantity, expiries[0].expiredQuantity), (0, QTY));
    assert_eq!(names, vec!["OrderPlaced", "OrderExpired"], "{names:?}");
}
