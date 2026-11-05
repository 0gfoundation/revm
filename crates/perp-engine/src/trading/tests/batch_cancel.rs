use super::*;
use crate::{
            batch::{
        self, PerpBatchReason, PerpBatchTag, BASE_BATCH_GAS, BATCH_STATUS_RECORD_LEN,
        MAX_BATCH_CANCEL,
    },
    errors::{perp_err, perp_fatal_invariant_err, perp_invariant_err},
    interface::IPerpDex::{
        batchCancelOrdersCall, batchCancelOrdersSignedCall, batchPlaceOrdersCall,
        batchPlaceOrdersSignedCall, cancelOrderCall, cancelOrderSignedCall, OrderCancelled,
    },
    call::selectors_map,
    types::{ApiKey, Order, OrderStatus, OrderType, Side, TimeInForce},
    CANCEL_ORDER_GAS, PLACE_ORDER_GAS,
    PerpError,
};
use ed25519_dalek::{Signer, SigningKey};

// Shared with the `batch_place` module below (same shell, same signed-call scaffolding).
pub(super) const SIGNED_TS: u64 = 1; // == BlockEnv::default() timestamp
pub(super) const SIGNED_RECV: u64 = 60;

/// Serialises every test that asserts an exact delta on `PERP_BATCH_ABORT_COUNT`.
///
/// That counter is deliberately PROCESS-WIDE (it is a diagnostic tripwire, not per-call
/// state), and cargo runs tests in parallel — so two tests that each abort once can
/// interleave between one test's `before` read and its `after` read, making a
/// `before + 1` assertion observe `+2`. Holding this lock across the read-run-read
/// window keeps the exact-delta assertion (which is the property worth pinning: the
/// call aborted exactly once) instead of weakening it to "the counter moved".
///
/// Poisoning is ignored: a panic in one of these tests is already a test failure, and
/// propagating the poison would turn it into a cascade of unrelated failures.
pub(super) static ABORT_COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Takes [`ABORT_COUNTER_LOCK`], ignoring poisoning. Hold the guard for the whole
/// read-run-read window.
pub(super) fn lock_abort_counter() -> std::sync::MutexGuard<'static, ()> {
    ABORT_COUNTER_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── helpers ────────────────────────────────────────────────────────────

fn direct_calldata(ids: &[[u8; 32]]) -> Vec<u8> {
    batchCancelOrdersCall {
        orderIds: ids.iter().map(|i| FixedBytes(*i)).collect(),
    }
    .abi_encode()
}

/// One decoded status record.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Status {
    pub(super) tag: u8,
    pub(super) order_id: [u8; 32],
    pub(super) reason: u8,
}

pub(super) fn decode_statuses(blob: &[u8]) -> Vec<Status> {
    assert_eq!(
        blob.len() % BATCH_STATUS_RECORD_LEN,
        0,
        "status blob must be a whole number of fixed-width records"
    );
    blob.chunks(BATCH_STATUS_RECORD_LEN)
        .map(|r| Status {
            tag: r[0],
            order_id: r[1..33].try_into().unwrap(),
            reason: r[33],
        })
        .collect()
}

pub(super) fn expect(tag: PerpBatchTag, id: [u8; 32], reason: PerpBatchReason) -> Status {
    Status {
        tag: tag as u8,
        order_id: id,
        reason: reason as u8,
    }
}

/// Runs a batch through the real entry point and returns the raw statuses blob.
fn batch_cancel(ctx: &mut TestCtx, caller: Address, ids: &[[u8; 32]]) -> Vec<u8> {
    let out = run_perp_dex_call(
        &direct_calldata(ids),
        30_000_000,
        caller,
        U256::ZERO,
        false,
        ctx,
    )
    .expect("batch must not hard-fail");
    assert!(
        !out.reverted,
        "batch must return Ok once the loop has begun (reverted with {:?})",
        out.bytes
    );
    assert_eq!(
        out.gas_used,
        BASE_BATCH_GAS + ids.len() as u64 * CANCEL_ORDER_GAS,
        "the FULL dynamic cost must be what the output charges"
    );
    batchCancelOrdersCall::abi_decode_returns(&out.bytes)
        .unwrap()
        .to_vec()
}

/// Directly stores an order record that was never inserted into any book level.
fn orphan_order(
    ctx: &mut TestCtx,
    id: [u8; 32],
    owner: Address,
    market_id: u64,
    side: Side,
    price: u64,
    status: OrderStatus,
) {
    storage::save_order(
        ctx,
        &id,
        &Order {
            owner: owner.0 .0,
            market_id,
            side,
            price,
            quantity: QTY,
            filled: 0,
            order_type: OrderType::Limit,
            tif: TimeInForce::Gtc,
            status,
                        reduce_only: false,
                        trigger_price: 0,
                        trigger_up: false,
                        close_position: false,
                        stp: Default::default(),
                        position_generation: 0,
                        trail_bps: 0,
        },
    )
    .unwrap();
}

fn cancelled_ids(ctx: &mut TestCtx) -> Vec<[u8; 32]> {
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
        .map(|l| {
            OrderCancelled::decode_raw_log(l.data.topics(), &l.data.data)
                .unwrap()
                .orderId
                .0
        })
        .collect()
}

// ── 1. happy path ──────────────────────────────────────────────────────

#[test]
fn batch_cancel_all_accepted_and_index_aligned() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let b = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
    let c = place(&mut ctx, ALICE, 1, PRICE + TICK, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let ids = [a, b, c];
    let blob = batch_cancel(&mut ctx, ALICE, &ids);

    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(PerpBatchTag::Accepted, a, PerpBatchReason::None),
            expect(PerpBatchTag::Accepted, b, PerpBatchReason::None),
            expect(PerpBatchTag::Accepted, c, PerpBatchReason::None),
        ]
    );
    for id in ids {
        assert_terminal(&mut ctx, id);
    }
    // Logs come out in processing order == calldata order.
    assert_eq!(cancelled_ids(&mut ctx), vec![a, b, c]);
    // Margin fully released.
    assert_eq!(wallet(&mut ctx, ALICE), WALLET);
}

// ── 2. mixed: genuine rejects do not stop the loop ─────────────────────

#[test]
fn batch_cancel_mixed_rejects_continue_the_loop() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let good_a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let good_b = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
    let bobs = place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);

    let missing = [0x11u8; 32];
    // status Filled → present in the map but not cancellable.
    let not_cancellable = [0x22u8; 32];
    orphan_order(
        &mut ctx,
        not_cancellable,
        ALICE,
        MARKET_ID,
        Side::Buy,
        PRICE,
        OrderStatus::Filled,
    );
    // market 999 was never registered → "unknown market".
    let unknown_market = [0x33u8; 32];
    orphan_order(
        &mut ctx,
        unknown_market,
        ALICE,
        999,
        Side::Buy,
        PRICE,
        OrderStatus::Open,
    );
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let ids = [
        missing,
        good_a,
        bobs,
        not_cancellable,
        unknown_market,
        good_b,
    ];
    let blob = batch_cancel(&mut ctx, ALICE, &ids);

    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(
                PerpBatchTag::Rejected,
                missing,
                PerpBatchReason::OrderNotFound
            ),
            expect(PerpBatchTag::Accepted, good_a, PerpBatchReason::None),
            expect(PerpBatchTag::Rejected, bobs, PerpBatchReason::NotOwner),
            expect(
                PerpBatchTag::Rejected,
                not_cancellable,
                PerpBatchReason::NotCancellable
            ),
            expect(
                PerpBatchTag::Rejected,
                unknown_market,
                PerpBatchReason::UnknownMarket
            ),
            expect(PerpBatchTag::Accepted, good_b, PerpBatchReason::None),
        ]
    );
    // The two valid ids were still cancelled, in calldata order; nothing else was touched.
    assert_eq!(cancelled_ids(&mut ctx), vec![good_a, good_b]);
    assert_terminal(&mut ctx, good_a);
    assert_terminal(&mut ctx, good_b);
    assert!(storage::load_order(&mut ctx, &bobs).unwrap().is_some());
    assert!(storage::load_order(&mut ctx, &not_cancellable)
        .unwrap()
        .is_some());
    assert!(storage::load_order(&mut ctx, &unknown_market)
        .unwrap()
        .is_some());
}

/// A genuine reject must be write-clean — that is the property the driver's runtime
/// classification keys off, so pin it directly for all four cancel rejects.
#[test]
fn every_genuine_cancel_reject_is_write_clean() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let bobs = place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);
    orphan_order(
        &mut ctx,
        [0x22u8; 32],
        ALICE,
        MARKET_ID,
        Side::Buy,
        PRICE,
        OrderStatus::Filled,
    );
    orphan_order(
        &mut ctx,
        [0x33u8; 32],
        ALICE,
        999,
        Side::Buy,
        PRICE,
        OrderStatus::Open,
    );

    for id in [[0x11u8; 32], bobs, [0x22u8; 32], [0x33u8; 32]] {
        let before = JournalTr::perp_write_count(ctx.journal_mut());
        let err = cancel_order_core(ALICE, id, &mut ctx).unwrap_err();
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            before,
            "reject {err:?} must not bump the perp write counter"
        );
    }
}

// ── 3. abort-forward ───────────────────────────────────────────────────

/// Real post-write error: an order whose price level is absent from the book. `decr_level_count`
/// WRITES the (materialised, count-0) level, and only then does the sell-side stale-BBO guard in
/// `remove_from_book_after_cancel` raise `[INVARIANT]` because the cached best_ask is 0. Under
/// commit-only that write cannot be undone → abort-forward.
#[test]
fn batch_cancel_aborts_forward_on_a_post_write_invariant() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    // Bids only, so best_ask stays 0.
    let good = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let untouched = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
    let orphan = [0x44u8; 32];
    orphan_order(
        &mut ctx,
        orphan,
        ALICE,
        MARKET_ID,
        Side::Sell,
        PRICE + TICK,
        OrderStatus::Open,
    );
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let _abort_guard = lock_abort_counter();
    let aborts_before = batch::perp_batch_abort_count();

    let ids = [good, orphan, untouched];
    // batch_cancel() already asserts the call did NOT revert.
    let blob = batch_cancel(&mut ctx, ALICE, &ids);

    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(PerpBatchTag::Accepted, good, PerpBatchReason::None),
            expect(PerpBatchTag::Aborted, orphan, PerpBatchReason::Invariant),
            expect(PerpBatchTag::NotAttempted, untouched, PerpBatchReason::None),
        ]
    );
    assert_eq!(batch::perp_batch_abort_count(), aborts_before + 1);
    // The committed prefix survives WITH its logs (the whole point of abort-forward).
    assert_eq!(cancelled_ids(&mut ctx), vec![good]);
    assert_terminal(&mut ctx, good);
    // The tail was never attempted: still resting.
    assert!(storage::load_order(&mut ctx, &untouched).unwrap().is_some());
}

/// Driver-level pin of the three classification branches, fed synthetic errors so the mapping
/// from "did this item bump the perp write counter?" to Rejected/Aborted is tested in isolation
/// (no reliance on any error string).
#[test]
fn driver_classifies_by_write_count_not_by_message() {
    // Produces aborts (so it bumps the global counter) even though it asserts no delta —
    // it must still hold the lock or it corrupts a concurrent test's exact-delta assertion.
    let _abort_guard = lock_abort_counter();
    let echo = |k: usize| [k as u8; 32];
    // `drive_batch` now asks for (index, tag) — the tag lets the place path report the id an
    // ABORT burned; the cancel path (and this driver test) is tag-agnostic.
    let echo_at = |k: usize, _tag: PerpBatchTag| echo(k);

    // (a) write-clean error → Rejected, loop continues.
    let mut ctx = make_ctx();
    let run = batch::drive_batch(&mut ctx, ALICE, 3, echo_at, |_ctx, k| {
        if k == 1 {
            Err(perp_invariant_err("write-clean invariant"))
        } else {
            Ok((PerpBatchTag::Accepted, [k as u8; 32]))
        }
    })
    .unwrap();
    assert_eq!((run.accepted, run.aborted_at), (2, None));
    assert_eq!(
        decode_statuses(&run.statuses),
        vec![
            expect(PerpBatchTag::Accepted, echo(0), PerpBatchReason::None),
            expect(PerpBatchTag::Rejected, echo(1), PerpBatchReason::Invariant),
            expect(PerpBatchTag::Accepted, echo(2), PerpBatchReason::None),
        ],
        "an error that wrote NOTHING is a genuine reject even when it is an [INVARIANT]"
    );

    // (b) error AFTER a perp write → Aborted + NotAttempted tail, still Ok.
    let mut ctx = make_ctx();
    let run = batch::drive_batch(&mut ctx, ALICE, 4, echo_at, |ctx, k| {
        if k == 1 {
            storage::save_user_nonce(ctx, ALICE, 7)?;
            Err(perp_err("plain reject, but it wrote first"))
        } else {
            Ok((PerpBatchTag::Accepted, [k as u8; 32]))
        }
    })
    .unwrap();
    // The place path advances the order nonce by `accepted` + the aborted item's own id.
    assert_eq!((run.accepted, run.aborted_at), (1, Some(1)));
    assert_eq!(
        decode_statuses(&run.statuses),
        vec![
            expect(PerpBatchTag::Accepted, echo(0), PerpBatchReason::None),
            expect(PerpBatchTag::Aborted, echo(1), PerpBatchReason::Other),
            expect(PerpBatchTag::NotAttempted, echo(2), PerpBatchReason::None),
            expect(PerpBatchTag::NotAttempted, echo(3), PerpBatchReason::None),
        ],
        "a plain perp_err that WROTE is an abort, regardless of its message"
    );

    // (c) Fatal propagates untouched, even though the loop had already begun.
    let mut ctx = make_ctx();
    let err = batch::drive_batch(&mut ctx, ALICE, 3, echo_at, |_ctx, k| {
        if k == 1 {
            Err(perp_fatal_invariant_err("node-level"))
        } else {
            Ok((PerpBatchTag::Accepted, [k as u8; 32]))
        }
    })
    .unwrap_err();
    assert!(matches!(err, PerpError::Fatal(_)), "got {err:?}");
}

// ── 4. pre-loop length faults revert the whole call ────────────────────

#[test]
fn empty_and_oversized_batches_revert_write_clean() {
    for ids in [
        Vec::<[u8; 32]>::new(),
        vec![[0x11u8; 32]; MAX_BATCH_CANCEL + 1],
    ] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

        let out = run_perp_dex_call(
            &direct_calldata(&ids),
            u64::MAX, // never let gas be the reason
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(out.reverted, "N = {} must revert the whole call", ids.len());
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "a pre-loop fault must be write-clean"
        );
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
    }
    // …and MAX itself is accepted (all ids missing → all Rejected).
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let ids: Vec<[u8; 32]> = (0..MAX_BATCH_CANCEL)
        .map(|i| {
            let mut b = [0u8; 32];
            b[..8].copy_from_slice(&(i as u64).to_be_bytes());
            b
        })
        .collect();
    let blob = batch_cancel(&mut ctx, ALICE, &ids);
    assert_eq!(blob.len(), MAX_BATCH_CANCEL * BATCH_STATUS_RECORD_LEN);
    assert!(decode_statuses(&blob)
        .iter()
        .all(|s| s.tag == PerpBatchTag::Rejected as u8
            && s.reason == PerpBatchReason::OrderNotFound as u8));
}

// ── 5. gas ─────────────────────────────────────────────────────────────

#[test]
fn batch_unit_matches_single_selector_cost() {
    // The per-item unit must BE the single-selector cost, not a re-invented number.
    assert_eq!(
        selectors_map()
            .get(&cancelOrderCall::SELECTOR)
            .expect("cancelOrder in table")
            .0,
        CANCEL_ORDER_GAS
    );
    assert_eq!(
        selectors_map()
            .get(&cancelOrderSignedCall::SELECTOR)
            .unwrap()
            .0,
        CANCEL_ORDER_GAS
    );
    // Same for place — and the value itself is pinned, since it is now a shared const.
    assert_eq!(CANCEL_ORDER_GAS, 80_000);
    assert_eq!(PLACE_ORDER_GAS, 200_000);
    assert_eq!(
        selectors_map()
            .get(&placeOrderCall::SELECTOR)
            .expect("placeOrder in table")
            .0,
        PLACE_ORDER_GAS
    );
    assert_eq!(
        selectors_map()
            .get(&placeOrderSignedCall::SELECTOR)
            .unwrap()
            .0,
        PLACE_ORDER_GAS
    );
    // …and the batch selectors' table entry is only the ENVELOPE FLOOR.
    for sel in [
        batchCancelOrdersCall::SELECTOR,
        batchCancelOrdersSignedCall::SELECTOR,
        batchPlaceOrdersCall::SELECTOR,
        batchPlaceOrdersSignedCall::SELECTOR,
    ] {
        let (cost, can_be_static) = *selectors_map().get(&sel).expect("batch sel in table");
        assert_eq!(cost, BASE_BATCH_GAS);
        assert!(!can_be_static);
    }
}

#[test]
fn insufficient_gas_limit_is_out_of_gas_before_any_write() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let b = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

    let full = BASE_BATCH_GAS + 2 * CANCEL_ORDER_GAS;
    let err = run_perp_dex_call(
        &direct_calldata(&[a, b]),
        full - 1,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, PerpError::OutOfGas), "got {err:?}");
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes_before,
        "OutOfGas must precede every write"
    );
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
    assert!(storage::load_order(&mut ctx, &a).unwrap().is_some());

    // Exactly the computed cost succeeds and charges exactly that.
    let out = run_perp_dex_call(
        &direct_calldata(&[a, b]),
        full,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!out.reverted);
    assert_eq!(out.gas_used, full);
}

/// A huge DECLARED length must be rejected by the pre-decode read, before
/// `DynSeqToken::decode_from` gets to `vec_try_with_capacity(len)` — i.e. no allocation, no
/// panic, no OOM. Only the envelope floor is charged.
#[test]
fn hostile_length_word_reverts_without_allocating() {
    // selector || offset(0x20) || length || <no elements>
    let mut huge_u32 = vec![0u8; 4 + 64];
    huge_u32[..4].copy_from_slice(&batchCancelOrdersCall::SELECTOR);
    huge_u32[35] = 0x20;
    huge_u32[64..68].copy_from_slice(&u32::MAX.to_be_bytes()); // 4 294 967 295 elements

    let mut huge_u256 = huge_u32.clone();
    huge_u256[36..68].fill(0xff); // does not even fit u32

    // Non-canonical head offset (0x40 instead of 0x20) is rejected too.
    let mut bad_offset = huge_u32.clone();
    bad_offset[35] = 0x40;
    bad_offset[64..68].copy_from_slice(&1u32.to_be_bytes());

    // Truncated: length says 2 but no element bytes follow.
    let mut truncated = huge_u32.clone();
    truncated[64..68].copy_from_slice(&2u32.to_be_bytes());

    for (name, input) in [
        ("u32::MAX length", huge_u32),
        ("u256 length", huge_u256),
        ("non-canonical offset", bad_offset),
        ("length exceeds calldata", truncated),
    ] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
        let out =
            run_perp_dex_call(&input, u64::MAX, ALICE, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted, "{name} must revert");
        assert_eq!(
            out.gas_used, BASE_BATCH_GAS,
            "{name}: an unreadable length word charges only the envelope floor"
        );
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "{name} must be write-clean"
        );
    }
}

#[test]
fn gas_math_saturates_instead_of_wrapping() {
    // N near u64::MAX / unit must not wrap into a small number that passes the gas check.
    assert_eq!(batch::batch_gas(usize::MAX, CANCEL_ORDER_GAS), u64::MAX);
    assert_eq!(
        batch::batch_gas(3, CANCEL_ORDER_GAS),
        BASE_BATCH_GAS + 3 * CANCEL_ORDER_GAS
    );
    // A length word that would wrap `len * 32` is rejected outright by the pre-decode read.
    assert!(batch::batch_dynamic_gas(batchCancelOrdersCall::SELECTOR, &[0u8; 8]).is_none());
}

/// An over-cap batch reverts in `check_batch_len` having done ZERO work, so it must be billed the
/// envelope floor only. `N = MAX + 1 = 257` would otherwise cost `20_000 + 257 * 80_000` ≈ 20.6M
/// gas for a call that performs nothing at all.
#[test]
fn over_cap_batch_is_billed_only_the_envelope_floor() {
    let over = direct_calldata(&vec![[0x11u8; 32]; MAX_BATCH_CANCEL + 1]);
    assert_eq!(
        batch::batch_dynamic_gas(batchCancelOrdersCall::SELECTOR, &over),
        None,
        "an over-cap declared length must not be billed per item"
    );

    let mut ctx = make_ctx();
    setup(&mut ctx);
    let out = run_perp_dex_call(&over, u64::MAX, ALICE, U256::ZERO, false, &mut ctx).unwrap();
    assert!(out.reverted, "over-cap must still revert the whole call");
    assert!(String::from_utf8_lossy(&out.bytes).contains("batch too large"));
    assert_eq!(out.gas_used, BASE_BATCH_GAS);

    // N == MAX is billed in full, so the clamp cannot silently under-bill a legal batch.
    let at_cap = direct_calldata(&vec![[0x11u8; 32]; MAX_BATCH_CANCEL]);
    assert_eq!(
        batch::batch_dynamic_gas(batchCancelOrdersCall::SELECTOR, &at_cap),
        Some(BASE_BATCH_GAS + MAX_BATCH_CANCEL as u64 * CANCEL_ORDER_GAS)
    );
}

// ── 6. signed batch ────────────────────────────────────────────────────

fn signed_calldata(
    sk: &SigningKey,
    account: Address,
    key_id: u8,
    timestamp: u64,
    recv_window: u64,
    ids: &[[u8; 32]],
    // Ids actually put on the wire; `None` = same as the signed set.
    wire_ids: Option<&[[u8; 32]]>,
) -> Vec<u8> {
    let signed: Vec<FixedBytes<32>> = ids.iter().map(|i| FixedBytes(*i)).collect();
    let msg = batch_cancel_message(TEST_CHAIN_ID, account, key_id, timestamp, recv_window, &signed);
    let sig = sk.sign(&msg);
    batchCancelOrdersSignedCall {
        account,
        keyId: key_id,
        timestamp,
        recvWindow: recv_window,
        orderIds: wire_ids
            .unwrap_or(ids)
            .iter()
            .map(|i| FixedBytes(*i))
            .collect(),
        signature: sig.to_bytes().to_vec().into(),
    }
    .abi_encode()
}

pub(super) fn register_key(ctx: &mut TestCtx, user: Address, sk: &SigningKey) {
    storage::save_api_key(
        ctx,
        user,
        0,
        ApiKey {
            pubkey: sk.verifying_key().to_bytes(),
            expiry: 0,
        },
    )
    .unwrap();
}

#[test]
fn signed_batch_cancel_happy_path() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register_key(&mut ctx, ALICE, &sk);
    let a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let b = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let input = signed_calldata(&sk, ALICE, 0, SIGNED_TS, SIGNED_RECV, &[a, b], None);
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(!out.reverted, "{:?}", out.bytes);
    assert_eq!(out.gas_used, BASE_BATCH_GAS + 2 * CANCEL_ORDER_GAS);
    let blob = batchCancelOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
    assert_eq!(
        decode_statuses(&blob),
        vec![
            expect(PerpBatchTag::Accepted, a, PerpBatchReason::None),
            expect(PerpBatchTag::Accepted, b, PerpBatchReason::None),
        ]
    );
    assert_eq!(cancelled_ids(&mut ctx), vec![a, b]);
}

#[test]
fn signed_batch_rejects_tampered_content() {
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let a = [0xa1u8; 32];
    let b = [0xb2u8; 32];
    let c = [0xc3u8; 32];

    // (i) one id swapped on the wire; (ii) an id appended (N changes); (iii) order permuted.
    for (name, signed, wire) in [
        ("swapped id", vec![a, b], vec![a, c]),
        ("N grew", vec![a, b], vec![a, b, c]),
        ("N shrank", vec![a, b], vec![a]),
        ("order permuted", vec![a, b], vec![b, a]),
    ] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        register_key(&mut ctx, ALICE, &sk);
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
        let input =
            signed_calldata(&sk, ALICE, 0, SIGNED_TS, SIGNED_RECV, &signed, Some(&wire));
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted, "{name} must fail verification");
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "{name}: signature failure is pre-write"
        );
    }
}

/// The single-order rule "a rejected signature stays replayable in-window" must NOT carry over:
/// a batch returns Ok, so the signature is burned unconditionally after verification — even for
/// a batch in which EVERY item was rejected.
#[test]
fn signed_batch_signature_is_burned_even_when_every_item_was_rejected() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register_key(&mut ctx, ALICE, &sk);

    let ids = [[0x11u8; 32], [0x12u8; 32]];
    let input = signed_calldata(&sk, ALICE, 0, SIGNED_TS, SIGNED_RECV, &ids, None);

    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(!out.reverted);
    let blob = batchCancelOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
    assert!(
        decode_statuses(&blob)
            .iter()
            .all(|s| s.tag == PerpBatchTag::Rejected as u8),
        "every item should have been rejected (unknown ids)"
    );

    // Replay of the very same signature reverts the WHOLE call.
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(out.reverted, "replay must revert");
    let reason = String::from_utf8_lossy(&out.bytes).to_string();
    assert!(reason.contains("duplicate signature"), "got {reason}");
}

#[test]
fn signed_batch_replay_after_partial_acceptance_reverts() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register_key(&mut ctx, ALICE, &sk);
    let a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let input = signed_calldata(
        &sk,
        ALICE,
        0,
        SIGNED_TS,
        SIGNED_RECV,
        &[a, [0x11u8; 32]],
        None,
    );

    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(!out.reverted);
    let blob = batchCancelOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
    let st = decode_statuses(&blob);
    assert_eq!(st[0].tag, PerpBatchTag::Accepted as u8);
    assert_eq!(st[1].tag, PerpBatchTag::Rejected as u8);

    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(
        out.reverted,
        "partially-accepted batch must not be replayable"
    );
}

#[test]
fn signed_batch_requires_key_window_and_expiry() {
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let ids = [[0x11u8; 32]];

    // No key registered.
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let input = signed_calldata(&sk, ALICE, 0, SIGNED_TS, SIGNED_RECV, &ids, None);
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(out.reverted);
    assert!(String::from_utf8_lossy(&out.bytes).contains("no api key"));

    // Timestamp in the future → outside recvWindow.
    let mut ctx = make_ctx();
    setup(&mut ctx);
    register_key(&mut ctx, ALICE, &sk);
    let input = signed_calldata(&sk, ALICE, 0, 10_000, SIGNED_RECV, &ids, None);
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(out.reverted);
    assert!(String::from_utf8_lossy(&out.bytes).contains("future"));

    // Expired key.
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_api_key(
        &mut ctx,
        ALICE,
        0,
        ApiKey {
            pubkey: sk.verifying_key().to_bytes(),
            expiry: 1,
        },
    )
    .unwrap();
    let input = signed_calldata(&sk, ALICE, 0, SIGNED_TS, SIGNED_RECV, &ids, None);
    let out =
        run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
    assert!(out.reverted);
    assert!(String::from_utf8_lossy(&out.bytes).contains("expired"));
}

// ── 7. determinism + reason-code table ─────────────────────────────────

#[test]
fn status_blob_is_byte_identical_across_runs() {
    let scenario = || {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let bobs = place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);
        let b = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
        orphan_order(
            &mut ctx,
            [0x22u8; 32],
            ALICE,
            999,
            Side::Buy,
            PRICE,
            OrderStatus::Open,
        );
        let ids = [[0x11u8; 32], a, bobs, [0x22u8; 32], b];
        batch_cancel(&mut ctx, ALICE, &ids)
    };
    let first = scenario();
    assert_eq!(first, scenario());
    assert_eq!(first, scenario());
    assert_eq!(first.len(), 5 * BATCH_STATUS_RECORD_LEN);
}

/// Pins the reason-code mapping so a message rename fails here instead of silently degrading
/// every code to `Other`. (The mapping is reporting-only — control flow uses the write counter.)
#[test]
fn reason_code_table_is_pinned() {
    use batch::reason_code;
    assert_eq!(
        reason_code(&perp_err("cancelOrder: order not found")),
        PerpBatchReason::OrderNotFound
    );
    assert_eq!(
        reason_code(&perp_err("cancelOrder: not owner")),
        PerpBatchReason::NotOwner
    );
    assert_eq!(
        reason_code(&perp_err("cancelOrder: order not cancellable")),
        PerpBatchReason::NotCancellable
    );
    assert_eq!(
        reason_code(&perp_err("cancelOrder: unknown market")),
        PerpBatchReason::UnknownMarket
    );
    // Place path (Phase 2). Every message below is copied from `validate_place_order` /
    // `execute_*` / `rest_in_book` / `finalize_compute`; a rename must fail HERE.
    for (msg, want) in [
        ("unknown market", PerpBatchReason::PlaceUnknownMarket),
        ("market not active", PerpBatchReason::MarketNotActive),
        ("invalid side", PerpBatchReason::InvalidSide),
        ("invalid orderType", PerpBatchReason::InvalidOrderType),
        ("invalid tif", PerpBatchReason::InvalidTif),
        (
            "tif not allowed for market order",
            PerpBatchReason::TifNotAllowedForOrderType,
        ),
        (
            "quantity below minimum",
            PerpBatchReason::QuantityBelowMinimum,
        ),
        (
            "quantity exceeds maximum",
            PerpBatchReason::QuantityAboveMaximum,
        ),
        (
            "quantity not multiple of step_size",
            PerpBatchReason::QuantityStepSize,
        ),
        ("limit order price must be > 0", PerpBatchReason::PriceZero),
        ("price exceeds maximum", PerpBatchReason::PriceAboveMaximum),
        (
            "price not multiple of tick_size",
            PerpBatchReason::PriceTickSize,
        ),
        (
            "PostOnly order would match",
            PerpBatchReason::PostOnlyWouldMatch,
        ),
        (
            "FOK order cannot be fully filled",
            PerpBatchReason::FokUnfillable,
        ),
        (
            "insufficient perp wallet for margin",
            PerpBatchReason::InsufficientMargin,
        ),
        (
            "open would breach maintenance margin",
            PerpBatchReason::OpenIntoInsolvency,
        ),
        (
            "fee recipient not initialised",
            PerpBatchReason::FeeRecipientNotSet,
        ),
        ("max open orders reached", PerpBatchReason::MaxOpenOrders),
        (
            "invalid self-trade prevention mode",
            PerpBatchReason::InvalidStpMode,
        ),
        ("reserve delta overflow", PerpBatchReason::ArithmeticGuard),
        ("fee reserve overflow", PerpBatchReason::ArithmeticGuard),
        (
            "fills+rest requirement overflow",
            PerpBatchReason::ArithmeticGuard,
        ),
        ("total required overflow", PerpBatchReason::ArithmeticGuard),
        ("order nonce overflow", PerpBatchReason::ArithmeticGuard),
        ("brand new place reject", PerpBatchReason::Other),
    ] {
        assert_eq!(
            reason_code(&perp_err(format!("placeOrder: {msg}"))),
            want,
            "placeOrder: {msg}"
        );
    }
    // The cross margin place-path refusals (they carry Binance codes, so they are matched by
    // their whole text).
    for (msg, want) in [
        (
            crate::trading::CROSS_INSUFFICIENT_MARGIN_REJECT,
            PerpBatchReason::InsufficientMargin,
        ),
        (crate::trading::ISOLATED_SILO_CASH_REJECT, PerpBatchReason::InsufficientMargin),
        (crate::trading::CROSS_MAINTENANCE_REJECT, PerpBatchReason::OpenIntoInsolvency),
        (crate::trading::IN_LIQUIDATION_REJECT, PerpBatchReason::UserInLiquidation),
    ] {
        assert_eq!(reason_code(&perp_err(msg)), want, "{msg}");
    }
    // Non-`placeOrder:`-prefixed engine guards reachable from the place path.
    assert_eq!(
        reason_code(&perp_err("settlement: realized PnL overflow")),
        PerpBatchReason::ArithmeticGuard
    );
    assert_eq!(
        reason_code(&perp_err("perp wallet: amount exceeds i64::MAX")),
        PerpBatchReason::ArithmeticGuard
    );
    assert_eq!(
        reason_code(&perp_err("split_position_fill: opening value underflow")),
        PerpBatchReason::ArithmeticGuard
    );
    // Reject codes are PARTITIONED by path: a place reject never lands in the cancel band and
    // vice versa (both selectors have an "unknown market", and they must not share a code).
    assert_ne!(
        reason_code(&perp_err("placeOrder: unknown market")),
        reason_code(&perp_err("cancelOrder: unknown market"))
    );
    assert_eq!(
        reason_code(&perp_invariant_err("anything")),
        PerpBatchReason::Invariant
    );
    // An `[INVARIANT] ` prefix wins over any place/cancel text inside it.
    assert_eq!(
        reason_code(&perp_invariant_err("placeOrder: unknown market")),
        PerpBatchReason::Invariant
    );
    assert_eq!(
        reason_code(&perp_err("something brand new")),
        PerpBatchReason::Other
    );
    assert_eq!(
        reason_code(&PerpError::OutOfGas),
        PerpBatchReason::Other
    );
    // Tag / reason wire values are consensus-adjacent client contract: pin them.
    assert_eq!(
        [
            PerpBatchTag::Rejected as u8,
            PerpBatchTag::Accepted as u8,
            PerpBatchTag::Filled as u8,
            PerpBatchTag::Aborted as u8,
            PerpBatchTag::NotAttempted as u8,
        ],
        [0, 1, 2, 3, 4]
    );
    assert_eq!(BATCH_STATUS_RECORD_LEN, 34);
    assert_eq!(MAX_BATCH_CANCEL, 256);
    assert_eq!(crate::batch::MAX_BATCH_PLACE, 64);
    // Wire values of every reason code: consensus-adjacent client contract.
    assert_eq!(
        [
            PerpBatchReason::None as u8,
            PerpBatchReason::OrderNotFound as u8,
            PerpBatchReason::NotOwner as u8,
            PerpBatchReason::NotCancellable as u8,
            PerpBatchReason::UnknownMarket as u8,
            PerpBatchReason::PlaceUnknownMarket as u8,
            PerpBatchReason::MarketNotActive as u8,
            PerpBatchReason::InvalidSide as u8,
            PerpBatchReason::InvalidOrderType as u8,
            PerpBatchReason::InvalidTif as u8,
            PerpBatchReason::QuantityBelowMinimum as u8,
            PerpBatchReason::QuantityAboveMaximum as u8,
            PerpBatchReason::QuantityStepSize as u8,
            PerpBatchReason::PriceZero as u8,
            PerpBatchReason::PriceAboveMaximum as u8,
            PerpBatchReason::PriceTickSize as u8,
            PerpBatchReason::PostOnlyWouldMatch as u8,
            PerpBatchReason::FokUnfillable as u8,
            PerpBatchReason::InsufficientMargin as u8,
            PerpBatchReason::OpenIntoInsolvency as u8,
            PerpBatchReason::FeeRecipientNotSet as u8,
            PerpBatchReason::TifNotAllowedForOrderType as u8,
            PerpBatchReason::MaxOpenOrders as u8,
            PerpBatchReason::InvalidStpMode as u8,
            PerpBatchReason::ArithmeticGuard as u8,
            PerpBatchReason::Invariant as u8,
            PerpBatchReason::Other as u8,
        ],
        [
            0, 1, 2, 3, 4, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
            33, 34, 253, 254, 255
        ]
    );
}

/// Batch selectors are not view calls.
#[test]
fn batch_selectors_are_not_static_callable() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let err = run_perp_dex_call(
        &direct_calldata(&[[0x11u8; 32]]),
        30_000_000,
        ALICE,
        U256::ZERO,
        true,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(err, PerpError::StaticRestrictionViolation),
        "got {err:?}"
    );
}
