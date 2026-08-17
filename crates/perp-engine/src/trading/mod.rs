//! Trading engine: place/cancel/query orders with on-chain order-book matching.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use crate::host::PerpHost;
use primitives::{keccak256, Address, Bytes, FixedBytes, Log};

use ed25519_dalek::{Signature, VerifyingKey};

mod liquidation;
mod settlement;

pub(crate) use liquidation::{
    execute_liquidation_market_order, run_adl, settle_liquidation_residual_at_mark_price,
};
use settlement::{MakerFillOutcome, TakerSettlement};

/// Maximum distinct maker accounts a single liquidation close may touch.
///
/// Any unfilled quantity after reaching the cap follows the existing residual/ADL path. This keeps
/// the balance after-image count bounded independently of order-book size.
pub(crate) const MAX_LIQUIDATION_MAKER_ACCOUNTS: usize = 128;

use crate::{
        batch::{self, PerpBatchTag},
    errors::{perp_err, perp_invariant_err},
    interface::IPerpDex::{
        self, batchCancelOrdersCall, batchCancelOrdersSignedCall, batchPlaceOrdersCall,
        batchPlaceOrdersSignedCall, cancelOrderCall, cancelOrderSignedCall, getBookLevelCall,
        getBookPricesCall, getMarketFeeTotalCall, getOpenOrdersCall, getOpenOrdersReturn,
        getOrderCall, getOrderReturn, placeOrderCall, placeOrderSignedCall, PlaceItem,
    },
    math::{
        calc_trading_fee,
        calc_value,
    },
    risk::record_mid_price_sample_for_best_quote_change,
    storage,
    types::{ApiKey, Order, OrderEntry, OrderStatus, OrderType, Side, TimeInForce},
    PERP_DEX_ADDRESS,
    PerpError,
};

// ── Public entry-points ───────────────────────────────────────────────────────

/// `placeOrder(uint64 marketId, uint8 side, uint64 price, uint64 quantity, uint8 orderType, uint8 tif) returns (bytes32 orderId)`
pub fn run_place_order<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = placeOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("placeOrder: invalid calldata"))?;

    let (order_id, bumped_nonce) = peek_order_id(context, caller)?;
    place_order_core(
        caller,
        order_id,
        args.marketId,
        args.side,
        args.price,
        args.quantity,
        args.orderType,
        args.tif,
        args.clientOrderId.0,
        context,
    )?;
    // Placement succeeded — persist the nonce bump (commit-only: rejected placements above
    // returned early and left the nonce untouched).
    commit_order_nonce(context, caller, bumped_nonce)?;
    Ok(Bytes::from(placeOrderCall::abi_encode_returns(
        &FixedBytes(order_id),
    )))
}

/// `placeOrderSigned(address account, ..., uint64 timestamp, bytes signature) returns (bytes32 orderId)`
///
/// orderId = keccak256(signature). Replay protection lives in the seen-signature set (commit-only
/// #23): delete-on-terminal removes a filled/cancelled order from the map, so the order-id is no
/// longer a durable replay witness — the seen-set is. A second submission of the same signature
/// hits the same seen marker and is rejected. The marker is time-bucketed and reclaimed once the
/// signature's recv window has fully elapsed (a stale replay is rejected by `check_recv_window`
/// first, so reclaiming the marker is safe).
pub fn run_place_order_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = placeOrderSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("placeOrderSigned: invalid calldata"))?;

    let api_key = storage::load_api_key(context, args.account, args.keyId)?
        .ok_or_else(|| perp_err("placeOrderSigned: no api key registered for account"))?;

    check_recv_window(context, args.timestamp, args.recvWindow)
        .map_err(|e| perp_err(&format!("placeOrderSigned: {e}")))?;

    check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("placeOrderSigned: {e}")))?;

    let pubkey = api_key.pubkey;

    // Canonical message (fixed-layout, 96 bytes):
    //   "perpdex_v1_order"(16) || account(20) || marketId(8) || side(1)
    //   || price(8) || quantity(8) || orderType(1) || tif(1) || clientOrderId(16)
    //   || timestamp(8) || recvWindow(8) || keyId(1)
    let mut msg = [0u8; 96];
    msg[..16].copy_from_slice(b"perpdex_v1_order");
    msg[16..36].copy_from_slice(args.account.as_slice());
    msg[36..44].copy_from_slice(&args.marketId.to_be_bytes());
    msg[44] = args.side;
    msg[45..53].copy_from_slice(&args.price.to_be_bytes());
    msg[53..61].copy_from_slice(&args.quantity.to_be_bytes());
    msg[61] = args.orderType;
    msg[62] = args.tif;
    msg[63..79].copy_from_slice(&args.clientOrderId.0);
    msg[79..87].copy_from_slice(&args.timestamp.to_be_bytes());
    msg[87..95].copy_from_slice(&args.recvWindow.to_be_bytes());
    msg[95] = args.keyId;

    verify_ed25519(&pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(&format!("placeOrderSigned: {e}")))?;

    // orderId = keccak256(signature); the SAME hash keys the seen-signature replay set.
    let order_id: [u8; 32] = keccak256(args.signature.as_ref()).0;
    if storage::is_signature_seen(context, &order_id)? {
        return Err(perp_err(
            "placeOrderSigned: duplicate signature (already submitted)",
        ));
    }

    place_order_core(
        args.account,
        order_id,
        args.marketId,
        args.side,
        args.price,
        args.quantity,
        args.orderType,
        args.tif,
        args.clientOrderId.0,
        context,
    )?;

    // Placement succeeded — burn the signature (commit-only: a REJECTED placement above returned
    // early WITHOUT marking, so it stays replayable within its recv window, exactly as the old
    // order-map guard behaved). Index it under its signed timestamp for time-bucketed GC, then
    // sweep one expired bucket.
    let block_ts: u64 = context.timestamp();
    storage::mark_signature_seen(context, &order_id, args.timestamp)?;
    storage::gc_seen_buckets(context, block_ts)?;

    Ok(Bytes::from(placeOrderSignedCall::abi_encode_returns(
        &FixedBytes(order_id),
    )))
}

/// `cancelOrderSigned(address account, bytes32 orderId, uint64 timestamp, bytes signature)`
///
/// Replay protection is implicit: cancelling an already-cancelled order is rejected by
/// cancel_order_core ("order not cancellable").
pub fn run_cancel_order_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = cancelOrderSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("cancelOrderSigned: invalid calldata"))?;

    let api_key = storage::load_api_key(context, args.account, args.keyId)?
        .ok_or_else(|| perp_err("cancelOrderSigned: no api key registered for account"))?;

    check_recv_window(context, args.timestamp, args.recvWindow)
        .map_err(|e| perp_err(&format!("cancelOrderSigned: {e}")))?;

    check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("cancelOrderSigned: {e}")))?;

    let pubkey = api_key.pubkey;

    // Canonical message (fixed-layout, 94 bytes):
    //   "perpdex_v1_cancel"(17) || account(20) || orderId(32) || marketId(8) || timestamp(8) || recvWindow(8) || keyId(1)
    // marketId is part of the signed message for ABI compatibility but otherwise ignored.
    let mut msg = [0u8; 94];
    msg[..17].copy_from_slice(b"perpdex_v1_cancel");
    msg[17..37].copy_from_slice(args.account.as_slice());
    msg[37..69].copy_from_slice(args.orderId.as_slice());
    msg[69..77].copy_from_slice(&args.marketId.to_be_bytes());
    msg[77..85].copy_from_slice(&args.timestamp.to_be_bytes());
    msg[85..93].copy_from_slice(&args.recvWindow.to_be_bytes());
    msg[93] = args.keyId;

    verify_ed25519(&pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(&format!("cancelOrderSigned: {e}")))?;

    cancel_order_core(args.account, args.orderId.0, context)
}

/// `cancelOrder(bytes32 orderId, uint64 marketId)`
///
/// `marketId` is accepted for ABI compatibility but ignored — the order is
/// looked up globally by `orderId`.
pub fn run_cancel_order<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = cancelOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("cancelOrder: invalid calldata"))?;
    cancel_order_core(caller, args.orderId.0, context)
}

// ── Batch cancel (Phase 1) ───────────────────────────────────────────────────
//
// The shell lives in `perp_dex::batch`: pre-decode length reading, the up-front gas bound, the
// numeric reason codes, the 34-byte status records and the abort-forward driver — all selector-
// agnostic, so `batchPlaceOrders` (Phase 2) reuses them with a different per-item closure.
//
// `cancel_order_core` is reused VERBATIM: no forked matching, settlement or margin logic. All four
// of its genuine rejects (order not found / not owner / not cancellable / unknown market) are pure
// reads, which is why cancel is the clean unit to build the shell on — a rejected item marks no
// typed-store key dirty, contributes zero keys to the block delta, and is invisible to the
// commitment.
//
// Atomicity is abort-forward (see the `batch` module docs for why "propagate → whole-batch revert"
// cannot be implemented under commit-only). Everything that can revert the WHOLE call is checked
// before the loop and is pre-write, so the commit-only write-then-error tripwire stays clean:
// bad calldata, `N == 0`, `N > MAX_BATCH_CANCEL`, gas (in `run_perp_dex_call`), `depth() > 1`
// (likewise), and — for the signed variant — signature/recvWindow/api-key/replay failures.

/// `batchCancelOrders(bytes32[] orderIds) returns (bytes statuses)`
///
/// Cancels each id in strict calldata order on behalf of `caller`. Returns the index-aligned status
/// blob; see [`batch`] for the record layout.
pub fn run_batch_cancel_orders<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    // Defensive pre-decode length read, repeated here on purpose: the gas path in
    // `run_perp_dex_call` must not be the only thing that distrusts the declared length word, since
    // `abi_decode_validate` reserves capacity for it before looking at the calldata size.
    batch::CANCEL_DIRECT_LAYOUT.checked_len(input_bytes)?;
    let args = batchCancelOrdersCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("batchCancelOrders: invalid calldata"))?;
    let ids = args.orderIds;
    // MAX is enforced on the DECODED length (authoritative), so a legal-but-non-minimal encoding
    // cannot fail a valid batch.
    batch::check_batch_len(ids.len(), batch::MAX_BATCH_CANCEL, "batchCancelOrders")?;

    let run = batch::drive_batch(
        context,
        caller,
        ids.len(),
        |k, _| ids[k].0,
        |ctx, k| {
            cancel_order_core(caller, ids[k].0, ctx).map(|_| (PerpBatchTag::Accepted, ids[k].0))
        },
    )?;
    Ok(Bytes::from(batchCancelOrdersCall::abi_encode_returns(
        &Bytes::from(run.statuses),
    )))
}

/// `batchCancelOrdersSigned(address account, uint8 keyId, uint64 timestamp, uint64 recvWindow, bytes32[] orderIds, bytes signature) returns (bytes statuses)`
///
/// ONE ed25519 signature authorises the whole batch. Canonical fixed-layout message, big-endian:
///
/// ```text
/// "perpdex_v1_batch_cancel"(23) || account(20) || keyId(1) || timestamp(8) || recvWindow(8)
///   || N(4) || N x orderId(32)
/// ```
///
/// `N` is inside the digest, so the batch's size, content and order cannot be tampered with.
pub fn run_batch_cancel_orders_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    batch::CANCEL_SIGNED_LAYOUT.checked_len(input_bytes)?;
    let args = batchCancelOrdersSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("batchCancelOrdersSigned: invalid calldata"))?;
    let ids = args.orderIds;
    batch::check_batch_len(
        ids.len(),
        batch::MAX_BATCH_CANCEL,
        "batchCancelOrdersSigned",
    )?;

    // Same validation ORDER as the other signed entry points: api key → recvWindow → expiry →
    // verify. Every step below is a pure read, so any failure reverts the whole call write-clean.
    let api_key = storage::load_api_key(context, args.account, args.keyId)?
        .ok_or_else(|| perp_err("batchCancelOrdersSigned: no api key registered for account"))?;

    check_recv_window(context, args.timestamp, args.recvWindow)
        .map_err(|e| perp_err(format!("batchCancelOrdersSigned: {e}")))?;

    check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(format!("batchCancelOrdersSigned: {e}")))?;

    let pubkey = api_key.pubkey;
    let msg = batch_cancel_message(
        args.account,
        args.keyId,
        args.timestamp,
        args.recvWindow,
        &ids,
    );
    verify_ed25519(&pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(format!("batchCancelOrdersSigned: {e}")))?;

    // Replay guard is MANDATORY here. The single-order rule — "a rejected signature stays replayable
    // inside its recv window" — must NOT carry over: a batch returns Ok, so leaving the signature
    // unburned would let a partially-accepted (or even all-rejected) batch be resubmitted and partly
    // re-execute. Hence: check before the loop (duplicate → whole-call revert, pre-write) and burn
    // immediately after verification, UNCONDITIONALLY, before the first item runs.
    let sig_hash: [u8; 32] = keccak256(args.signature.as_ref()).0;
    if storage::is_signature_seen(context, &sig_hash)? {
        return Err(perp_err(
            "batchCancelOrdersSigned: duplicate signature (already submitted)",
        ));
    }
    // ── last pre-loop fault has passed; the first write happens here ──
    let block_ts: u64 = context.timestamp();
    storage::mark_signature_seen(context, &sig_hash, args.timestamp)?;
    gc_seen_buckets_best_effort(context, block_ts)?;

    let account = args.account;
    let run = batch::drive_batch(
        context,
        account,
        ids.len(),
        |k, _| ids[k].0,
        |ctx, k| {
            cancel_order_core(account, ids[k].0, ctx).map(|_| (PerpBatchTag::Accepted, ids[k].0))
        },
    )?;
    Ok(Bytes::from(
        batchCancelOrdersSignedCall::abi_encode_returns(&Bytes::from(run.statuses)),
    ))
}

/// Best-effort seen-signature GC.
///
/// The sweep is pure housekeeping — it drops replay markers whose recv window expired a full
/// retention margin ago — but it is a fallible WRITE, and it runs after the signature has been
/// burned. Under commit-only an `Err` here would revert a call whose burn is already committed:
/// that signature becomes permanently unusable while its work never happened. So an ordinary error
/// is dropped on the floor. A half-done sweep is harmless by the same retention argument
/// (`storage::gc_seen_buckets`): the markers it drops can no longer cause a false reject, and a
/// bucket it fails to finish is re-visited or simply leaks stale markers until a wipe.
///
/// A [`PerpError::Fatal`] still propagates — that is a node/DB fault, not a housekeeping
/// failure, and it must not be masked (same rule `drive_batch` applies to per-item errors).
fn gc_seen_buckets_best_effort<H: PerpHost>(
    context: &mut H,
    block_ts: u64,
) -> Result<(), PerpError> {
    match storage::gc_seen_buckets(context, block_ts) {
        Err(PerpError::Fatal(e)) => Err(PerpError::Fatal(e)),
        _ => Ok(()),
    }
}

/// Canonical batch-cancel digest preimage (64-byte header + 32 bytes per id):
/// `"perpdex_v1_batch_cancel"(23) || account(20) || keyId(1) || timestamp(8) || recvWindow(8)
///  || N(4) || N x orderId(32)`, all integers big-endian.
///
/// `N` is the DECODED id count, so a tampered length cannot be made to verify.
pub(crate) fn batch_cancel_message(
    account: Address,
    key_id: u8,
    timestamp: u64,
    recv_window: u64,
    ids: &[FixedBytes<32>],
) -> Vec<u8> {
    const TAG: &[u8; 23] = b"perpdex_v1_batch_cancel";
    const HEADER: usize = 23 + 20 + 1 + 8 + 8 + 4;
    let mut msg = Vec::with_capacity(HEADER + ids.len() * 32);
    msg.extend_from_slice(TAG);
    msg.extend_from_slice(account.as_slice());
    msg.push(key_id);
    msg.extend_from_slice(&timestamp.to_be_bytes());
    msg.extend_from_slice(&recv_window.to_be_bytes());
    msg.extend_from_slice(&(ids.len() as u32).to_be_bytes());
    for id in ids {
        msg.extend_from_slice(id.as_slice());
    }
    msg
}

// ── Batch place (Phase 2) ────────────────────────────────────────────────────
//
// Same shell as batch cancel (`perp_dex::batch`), same abort-forward semantics, and
// `place_order_core` is reused unchanged — no forked matching, margin or settlement logic. Two
// things are specific to placement:
//
// 1. **orderId / nonce.** `save_order` has NO collision guard, so the id rule must be exact. The
//    direct path keeps using the per-user nonce chain, but it cannot call `peek_order_id` per item:
//    the bump is only persisted at the end, so every peek would hand back the SAME id. Instead the
//    base nonce is read once, item `k` derives from `base + ids_consumed_before_k`
//    ([`derive_order_id`], byte-identical math to the single-order path), and the nonce is committed
//    ONCE, advanced by exactly the number of items that consumed an id. **An id is consumed iff the
//    item was accepted** (an aborted item counts too — it wrote state, possibly under its id). A
//    rejected item consumes nothing, so ids come out gapless and no later batch can re-derive a live
//    one. The signed path never touches the nonce at all: `orderId[k] =
//    keccak256(signature || u32BE(k))`, distinct per index and bound to the one signature (the
//    single-order `keccak256(signature)` would collide across the N items).
//
// 2. **Two accepted tags.** `place_order_core` returns the taker order's final status:
//    `Open`/`PartiallyFilled` ⇒ it rests ⇒ [`PerpBatchTag::Accepted`]; anything terminal (fully
//    `Filled`, or an IOC/FOK/market remainder that `Expired`) ⇒ [`PerpBatchTag::Filled`], i.e. no
//    resting record was left. The `Trade` / `PositionChanged` logs carry the detail.
//
// A rejected item is log-clean as well as write-clean: after Phase 0 the `OrderPlaced` log is
// buffered and flushed at the first apply, which every genuine reject precedes.

/// `batchPlaceOrders(PlaceItem[] orders) returns (bytes statuses)`
///
/// Places each item in strict calldata order on behalf of `caller`; each item matches the book as
/// left by the previous one. Returns the index-aligned status blob; see [`batch`] for the record
/// layout. The `orderId` field is the placed id for an accepted item, the BURNED id for an aborted
/// one (it consumed an id and the nonce advances past it), and ZERO for a rejected /
/// never-attempted one (no id was consumed, so none exists).
pub fn run_batch_place_orders<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    // Defensive pre-decode length read, repeated here on purpose (see `run_batch_cancel_orders`):
    // `abi_decode_validate` reserves capacity for the declared length before looking at the
    // calldata size, and one `PlaceItem` is 224 bytes, so the bound is 7× tighter than cancel's.
    batch::PLACE_DIRECT_LAYOUT.checked_len(input_bytes)?;
    let args = batchPlaceOrdersCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("batchPlaceOrders: invalid calldata"))?;
    let orders = args.orders;
    batch::check_batch_len(orders.len(), batch::MAX_BATCH_PLACE, "batchPlaceOrders")?;

    // Read the nonce ONCE. Nothing inside the loop can bump it: `load/save_user_nonce` is touched
    // only by `peek_order_id`/`commit_order_nonce`, whose other caller is the liquidation close —
    // and no liquidation can run from a placement (the auto-liq sweep lives in
    // `run_update_index_price`).
    let base_nonce = storage::load_user_nonce(context, caller)?;
    // `Cell` because both closures need it: `run_item` advances it, `echo_id` reads the id of the
    // item that is failing. Single-threaded, no borrow conflict.
    let ids_consumed = core::cell::Cell::new(0u64);
    // The id the CURRENT item derived, published before anything can fail under it.
    let item_id = core::cell::Cell::new([0u8; 32]);
    let run = batch::drive_batch(
        context,
        caller,
        orders.len(),
        // A rejected / never-attempted placement consumed no id (report zero). An ABORTED one did:
        // it wrote — possibly under that id — and `commit_batch_order_nonce` advances the nonce past
        // it, so the caller must be told which id was burned.
        |_, tag| match tag {
            PerpBatchTag::Aborted => item_id.get(),
            _ => [0u8; 32],
        },
        |ctx, k| {
            let nonce = base_nonce
                .checked_add(ids_consumed.get())
                .ok_or_else(|| perp_err("placeOrder: order nonce overflow"))?;
            let order_id = derive_order_id(caller, nonce);
            item_id.set(order_id);
            let tag = place_batch_item(ctx, caller, order_id, &orders[k])?;
            // Reached only on acceptance — a reject returned above and consumed no id.
            ids_consumed.set(ids_consumed.get() + 1);
            Ok((tag, order_id))
        },
    )?;
    let ids_consumed = ids_consumed.get();
    debug_assert_eq!(run.accepted as u64, ids_consumed);
    commit_batch_order_nonce(context, caller, base_nonce, ids_consumed, run.aborted_at)?;
    Ok(Bytes::from(batchPlaceOrdersCall::abi_encode_returns(
        &Bytes::from(run.statuses),
    )))
}

/// `batchPlaceOrdersSigned(address account, uint8 keyId, uint64 timestamp, uint64 recvWindow, PlaceItem[] orders, bytes signature) returns (bytes statuses)`
///
/// ONE ed25519 signature authorises the whole batch. Canonical fixed-layout message, big-endian:
///
/// ```text
/// "perpdex_v1_batch_order"(22) || account(20) || keyId(1) || timestamp(8) || recvWindow(8)
///   || N(4) || N x [ marketId(8) || side(1) || price(8) || quantity(8) || orderType(1) || tif(1)
///                    || clientOrderId(16) ]
/// ```
///
/// `N` is inside the digest, so the batch's size, content and order cannot be tampered with.
/// Order ids are `keccak256(signature || u32BE(k))` — the per-user nonce is NOT used or advanced.
pub fn run_batch_place_orders_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    batch::PLACE_SIGNED_LAYOUT.checked_len(input_bytes)?;
    let args = batchPlaceOrdersSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("batchPlaceOrdersSigned: invalid calldata"))?;
    let orders = args.orders;
    batch::check_batch_len(
        orders.len(),
        batch::MAX_BATCH_PLACE,
        "batchPlaceOrdersSigned",
    )?;

    // Same validation ORDER as every other signed entry point: api key → recvWindow → expiry →
    // verify → replay. Every step below is a pure read, so any failure reverts the whole call
    // write-clean.
    let api_key = storage::load_api_key(context, args.account, args.keyId)?
        .ok_or_else(|| perp_err("batchPlaceOrdersSigned: no api key registered for account"))?;

    check_recv_window(context, args.timestamp, args.recvWindow)
        .map_err(|e| perp_err(format!("batchPlaceOrdersSigned: {e}")))?;

    check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(format!("batchPlaceOrdersSigned: {e}")))?;

    let pubkey = api_key.pubkey;
    let msg = batch_place_message(
        args.account,
        args.keyId,
        args.timestamp,
        args.recvWindow,
        &orders,
    );
    verify_ed25519(&pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(format!("batchPlaceOrdersSigned: {e}")))?;
    // Verification already proved the length; re-taken as a fixed array so per-item id derivation
    // needs no allocation.
    let signature: [u8; 64] = args
        .signature
        .as_ref()
        .try_into()
        .map_err(|_| perp_err("batchPlaceOrdersSigned: signature must be 64 bytes"))?;

    // Replay guard is MANDATORY (see `run_batch_cancel_orders_signed`): a batch returns Ok, so an
    // unburned signature would let a partially-accepted batch be resubmitted and partly re-execute.
    // It is also what keeps the signed id derivation collision-free across calls — the same
    // signature can never run twice.
    let sig_hash: [u8; 32] = keccak256(signature).0;
    if storage::is_signature_seen(context, &sig_hash)? {
        return Err(perp_err(
            "batchPlaceOrdersSigned: duplicate signature (already submitted)",
        ));
    }
    // ── last pre-loop fault has passed; the first write happens here ──
    let block_ts: u64 = context.timestamp();
    storage::mark_signature_seen(context, &sig_hash, args.timestamp)?;
    gc_seen_buckets_best_effort(context, block_ts)?;

    let account = args.account;
    let run = batch::drive_batch(
        context,
        account,
        orders.len(),
        // An ABORTED item's id was consumed (it wrote, possibly under that id), so report it; a
        // rejected / never-attempted item consumed none. Derivation is a pure function of `k`.
        |k, tag| match tag {
            PerpBatchTag::Aborted => signed_batch_order_id(&signature, k as u32),
            _ => [0u8; 32],
        },
        |ctx, k| {
            let order_id = signed_batch_order_id(&signature, k as u32);
            let tag = place_batch_item(ctx, account, order_id, &orders[k])?;
            Ok((tag, order_id))
        },
    )?;
    Ok(Bytes::from(batchPlaceOrdersSignedCall::abi_encode_returns(
        &Bytes::from(run.statuses),
    )))
}

/// One batch item: `place_order_core` verbatim, plus the status → tag mapping.
fn place_batch_item<H: PerpHost>(
    context: &mut H,
    account: Address,
    order_id: [u8; 32],
    item: &PlaceItem,
) -> Result<PerpBatchTag, PerpError> {
    let status = place_order_core(
        account,
        order_id,
        item.marketId,
        item.side,
        item.price,
        item.quantity,
        item.orderType,
        item.tif,
        item.clientOrderId.0,
        context,
    )?;
    Ok(if status.is_terminal() {
        // Fully filled, or an IOC/FOK/market remainder that expired: accepted, but nothing rests.
        PerpBatchTag::Filled
    } else {
        PerpBatchTag::Accepted
    })
}

/// Persists the batch's single nonce advance: `base + (accepted ids) + (1 if an item aborted)`.
///
/// The aborted item is counted because it WROTE perp state before failing, possibly under its
/// derived id (a book entry / level FIFO slot). Burning that id is the safe side of the trade:
/// re-deriving it in a later batch would let `save_order` — which has no collision guard — silently
/// overwrite live state.
///
/// Skips the write entirely when nothing was consumed, so an all-rejected batch contributes zero
/// keys to the block delta (a redundant same-value write would still mark the key dirty).
fn commit_batch_order_nonce<H: PerpHost>(
    context: &mut H,
    account: Address,
    base_nonce: u64,
    accepted: u64,
    aborted_at: Option<usize>,
) -> Result<(), PerpError> {
    let consumed = accepted + u64::from(aborted_at.is_some());
    if consumed == 0 {
        return Ok(());
    }
    // checked, NOT saturating: at `base_nonce == u64::MAX` a saturating add would clamp and leave
    // the nonce on a value already spent, so the next placement would re-derive a LIVE order id and
    // `save_order` (no collision guard) would silently overwrite it. Same guard the per-item
    // derivation in the loop uses, same message.
    let next = base_nonce
        .checked_add(consumed)
        .ok_or_else(|| perp_err("placeOrder: order nonce overflow"))?;
    commit_order_nonce(context, account, next)
}

/// Signed-batch order id: `keccak256(signature(64) || k(4, big-endian))`.
///
/// Distinct per index and bound to the one signature. The single-order signed path uses the raw
/// `keccak256(signature)`, which would give all N items of a batch the SAME id.
fn signed_batch_order_id(signature: &[u8; 64], k: u32) -> [u8; 32] {
    let mut buf = [0u8; 68];
    buf[..64].copy_from_slice(signature);
    buf[64..].copy_from_slice(&k.to_be_bytes());
    keccak256(buf).0
}

/// Canonical batch-place digest preimage (63-byte header + 43 bytes per item):
/// `"perpdex_v1_batch_order"(22) || account(20) || keyId(1) || timestamp(8) || recvWindow(8) || N(4)
///  || N x [ marketId(8) || side(1) || price(8) || quantity(8) || orderType(1) || tif(1)
///           || clientOrderId(16) ]`, all integers big-endian.
///
/// `N` is the DECODED item count, so neither the length nor any field of any item can be tampered
/// with. Note the digest packs the items TIGHTLY (43 bytes each) — it is not the ABI encoding
/// (224 bytes each).
pub(crate) fn batch_place_message(
    account: Address,
    key_id: u8,
    timestamp: u64,
    recv_window: u64,
    orders: &[PlaceItem],
) -> Vec<u8> {
    const TAG: &[u8; 22] = b"perpdex_v1_batch_order";
    const HEADER: usize = 22 + 20 + 1 + 8 + 8 + 4;
    /// marketId(8) + side(1) + price(8) + quantity(8) + orderType(1) + tif(1) + clientOrderId(16)
    const ITEM: usize = 43;
    let mut msg = Vec::with_capacity(HEADER + orders.len() * ITEM);
    msg.extend_from_slice(TAG);
    msg.extend_from_slice(account.as_slice());
    msg.push(key_id);
    msg.extend_from_slice(&timestamp.to_be_bytes());
    msg.extend_from_slice(&recv_window.to_be_bytes());
    msg.extend_from_slice(&(orders.len() as u32).to_be_bytes());
    for o in orders {
        msg.extend_from_slice(&o.marketId.to_be_bytes());
        msg.push(o.side);
        msg.extend_from_slice(&o.price.to_be_bytes());
        msg.extend_from_slice(&o.quantity.to_be_bytes());
        msg.push(o.orderType);
        msg.push(o.tif);
        msg.extend_from_slice(&o.clientOrderId.0);
    }
    debug_assert_eq!(msg.len(), HEADER + orders.len() * ITEM);
    msg
}

/// `getOrder(bytes32 orderId, uint64 marketId) returns (address owner, uint64 marketId, uint8 side, uint64 price, uint64 quantity, uint64 filled, uint8 status)`
///
/// The input `marketId` is accepted for ABI compatibility but ignored — the
/// order is looked up globally by `orderId`.
pub fn run_get_order<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getOrder: invalid calldata"))?;
    let order_id: [u8; 32] = args.orderId.0;

    let order = storage::load_order_ref(context, &order_id)?
        .ok_or_else(|| perp_err("getOrder: order not found"))?;

    let owner = Address::from(order.owner);
    Ok(Bytes::from(getOrderCall::abi_encode_returns(
        &getOrderReturn {
            owner,
            marketId: order.market_id,
            side: order.side as u8,
            price: order.price,
            quantity: order.quantity,
            filled: order.filled,
            status: order.status as u8,
        },
    )))
}

/// `getOpenOrders(address user, uint64 marketId) returns (bytes32[] orderIds, uint8[] sides, uint64[] prices, uint64[] remainingQuantities)`
pub fn run_get_open_orders<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getOpenOrdersCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getOpenOrders: invalid calldata"))?;

    let buy_entries = storage::load_buy_orders_ref(context, args.user, args.marketId)?;
    let sell_entries = storage::load_sell_orders_ref(context, args.user, args.marketId)?;
    let total = buy_entries.len() + sell_entries.len();

    let mut order_ids = Vec::with_capacity(total);
    let mut sides = Vec::with_capacity(total);
    let mut prices = Vec::with_capacity(total);
    let mut remaining_quantities = Vec::with_capacity(total);

    for entry in buy_entries.iter() {
        order_ids.push(FixedBytes(entry.order_id));
        sides.push(Side::Buy as u8);
        prices.push(entry.price);
        remaining_quantities.push(entry.amount);
    }
    for entry in sell_entries.iter() {
        order_ids.push(FixedBytes(entry.order_id));
        sides.push(Side::Sell as u8);
        prices.push(entry.price);
        remaining_quantities.push(entry.amount);
    }

    Ok(Bytes::from(getOpenOrdersCall::abi_encode_returns(
        &getOpenOrdersReturn {
            orderIds: order_ids,
            sides,
            prices,
            remainingQuantities: remaining_quantities,
        },
    )))
}

/// `getMarketFeeTotal(uint64 marketId) returns (uint64 totalFee)`
pub fn run_get_market_fee_total<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getMarketFeeTotalCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarketFeeTotal: invalid calldata"))?;
    let total = storage::load_market_fee_total(context, args.marketId)?;
    Ok(Bytes::from(getMarketFeeTotalCall::abi_encode_returns(
        &total,
    )))
}

/// `getBookPrices(uint64 marketId, uint8 side) returns (uint64[] prices)`
pub fn run_get_book_prices<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getBookPricesCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getBookPrices: invalid calldata"))?;

    let side = Side::from_u8(args.side).ok_or_else(|| perp_err("getBookPrices: invalid side"))?;
    // BTreeSet is ascending; return the API's best-first order: bids DESC (rev), asks ASC.
    let prices: Vec<u64> = match side {
        Side::Buy => storage::load_bid_prices_ref(context, args.marketId)?
            .iter()
            .rev()
            .copied()
            .collect(),
        Side::Sell => storage::load_ask_prices_ref(context, args.marketId)?
            .iter()
            .copied()
            .collect(),
    };

    Ok(Bytes::from(getBookPricesCall::abi_encode_returns(&prices)))
}

/// `getBookLevel(uint64 marketId, uint8 side, uint64 price) returns (bytes32[] orderIds)`
pub fn run_get_book_level<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getBookLevelCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getBookLevel: invalid calldata"))?;

    let side = Side::from_u8(args.side).ok_or_else(|| perp_err("getBookLevel: invalid side"))?;
    let level = match side {
        Side::Buy => storage::load_bid_level_arc(context, args.marketId, args.price)?,
        Side::Sell => storage::load_ask_level_arc(context, args.marketId, args.price)?,
    };
    let order_ids = level.ids.iter().copied().map(FixedBytes).collect();

    Ok(Bytes::from(getBookLevelCall::abi_encode_returns(
        &order_ids,
    )))
}

// ── Timestamp / recvWindow helpers ────────────────────────────────────────────

const MAX_RECV_WINDOW: u64 = 60; // seconds
const CLOCK_SKEW_ALLOWANCE: u64 = 5; // seconds of future tolerance

pub(crate) fn check_api_key_expiry<H: PerpHost>(
    context: &mut H,
    key: &ApiKey,
) -> Result<(), &'static str> {
    if key.expiry == 0 {
        return Ok(());
    }
    let block_ts: u64 = context.timestamp();
    if block_ts >= key.expiry {
        return Err("api key has expired");
    }
    Ok(())
}

pub(crate) fn check_recv_window<H: PerpHost>(
    context: &mut H,
    timestamp: u64,
    recv_window: u64,
) -> Result<(), &'static str> {
    let block_ts: u64 = context.timestamp();
    let window = recv_window.min(MAX_RECV_WINDOW);

    if timestamp > block_ts + CLOCK_SKEW_ALLOWANCE {
        return Err("timestamp is in the future");
    }
    if block_ts.saturating_sub(timestamp) > window {
        return Err("timestamp expired (outside recvWindow)");
    }
    Ok(())
}

// ── ed25519 helpers ───────────────────────────────────────────────────────────

pub(crate) fn verify_ed25519(
    pubkey_bytes: &[u8; 32],
    message: &[u8],
    signature_bytes: &[u8],
) -> Result<(), &'static str> {
    let pubkey =
        VerifyingKey::from_bytes(pubkey_bytes).map_err(|_| "invalid ed25519 public key")?;
    let sig_arr: &[u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| "signature must be 64 bytes")?;
    let signature = Signature::from_bytes(sig_arr);
    pubkey
        .verify_strict(message, &signature)
        .map_err(|_| "signature verification failed")
}

// ── Core order logic (shared by direct and signed paths) ─────────────────────

/// Allocate the next order ID for `account` using the per-user nonce counter.
/// Derives the next order id from the CURRENT nonce WITHOUT bumping it (commit-only #23: the
/// nonce write happens only after the placement fully succeeds — a rejected placement leaves the
/// nonce untouched, so the same id is reused, matching the old revert-rollback behavior).
/// Returns `(order_id, bumped_nonce)`; the caller persists the bump via
/// [`commit_order_nonce`] on the success path.
pub(super) fn peek_order_id<H: PerpHost>(
    context: &mut H,
    account: Address,
) -> Result<([u8; 32], u64), PerpError> {
    let nonce = storage::load_user_nonce(context, account)?;
    Ok((derive_order_id(account, nonce), nonce + 1))
}

/// The one and only direct-path order-id derivation: `keccak256(account(20) || nonce(8 BE))`.
///
/// Factored out because `batchPlaceOrders` cannot call [`peek_order_id`] per item — the nonce is not
/// persisted until the end of the batch, so every item would peek the SAME id. The batch derives
/// item `k`'s id from `base_nonce + ids_consumed_so_far` instead, and must do so with byte-identical
/// math to the single-order path (`save_order` has no collision guard).
pub(super) fn derive_order_id(account: Address, nonce: u64) -> [u8; 32] {
    let mut buf = [0u8; 28];
    buf[..20].copy_from_slice(account.as_slice());
    buf[20..28].copy_from_slice(&nonce.to_be_bytes());
    keccak256(&buf).0
}

/// Persists the nonce bump reserved by [`peek_order_id`]. Call ONLY after the placement
/// succeeded (all genuine rejects passed).
pub(super) fn commit_order_nonce<H: PerpHost>(
    context: &mut H,
    account: Address,
    bumped_nonce: u64,
) -> Result<(), PerpError> {
    storage::save_user_nonce(context, account, bumped_nonce)
}

struct ValidatedOrder {
    market: crate::types::Market,
    side: Side,
    order_type: OrderType,
    tif: TimeInForce,
}

/// Validate → build in memory → execute → ONE final persist. Returns the taker order's FINAL status,
/// which is the whole outcome the callers need: `Open`/`PartiallyFilled` = it rests, and anything
/// `is_terminal()` = it fully filled or its IOC/FOK/market remainder expired, leaving no record.
/// (`batchPlaceOrders` maps that split onto the `Accepted` / `Filled` status tags. The single-order
/// entry points ignore it — they answer with the order id and let the caller read the logs.)
fn place_order_core<H: PerpHost>(
    account: Address,
    order_id: [u8; 32],
    market_id: u64,
    side_u8: u8,
    price: u64,
    quantity: u64,
    order_type_u8: u8,
    tif_u8: u8,
    client_order_id: [u8; 16],
    context: &mut H,
) -> Result<OrderStatus, PerpError> {
    let validated = validate_place_order(
        context,
        market_id,
        side_u8,
        price,
        quantity,
        order_type_u8,
        tif_u8,
    )?;
    // Per-user market-index cap. This is the ONLY way into a market the user is not already
    // active in — a position can only appear through a fill of an order they placed, and every
    // other position writer (maker fill, liquidation, ADL, funding) acts on a market where the
    // user already holds a position or a resting order. Placed with the other genuine rejects,
    // BEFORE any write, so the commit-only "no reject after a write" rule holds.
    storage::ensure_user_market_admission(context, account, market_id)?;

    // commit-only #23: build the Order in memory. The OrderPlaced log is BUFFERED (not emitted):
    // every genuine reject still lies ahead, and the batch selectors catch a per-item error without
    // reverting the frame — so the log is flushed by `emit_pending_order_placed` at the first APPLY
    // point (match flush / rest / the final persist below), keeping its original stream position.
    let (mut taker_order, pending) = announce_new_order(
        account,
        &order_id,
        market_id,
        price,
        quantity,
        client_order_id,
        &validated,
    );
    let mut pending = Some(pending);

    match validated.order_type {
        OrderType::Limit => execute_limit_order(
            context,
            account,
            order_id,
            market_id,
            price,
            quantity,
            client_order_id,
            validated,
            &mut taker_order,
            &mut pending,
        )?,
        OrderType::Market => execute_market_order(
            context,
            account,
            order_id,
            market_id,
            price,
            quantity,
            validated,
            &mut taker_order,
            &mut pending,
        )?,
    }
    // Fallback flush: the order is accepted (every genuine reject returned above) but reached no
    // other apply site — an IOC/market order that expired against an empty book matches nothing and
    // rests nothing, yet legitimately emits OrderPlaced. No-op when an apply already flushed it.
    emit_pending_order_placed(context, &mut pending);
    // Single final persist of the taker order. delete-on-terminal: a Filled/Expired taker leaves
    // NO record (it fully filled or its IOC/FOK/market remainder expired — never resting); an
    // Open/PartiallyFilled taker rested, so it is saved live (its book entry / level FIFO / live
    // count were already written by `rest_in_book`).
    let status = taker_order.status;
    if status.is_terminal() {
        storage::delete_order(context, &order_id)?;
    } else {
        // taker_order is owned here — move it in instead of cloning through save_order(&Order).
        storage::save_order_owned(context, &order_id, taker_order)?;
    }
    Ok(status)
}

fn validate_place_order<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side_u8: u8,
    price: u64,
    quantity: u64,
    order_type_u8: u8,
    tif_u8: u8,
) -> Result<ValidatedOrder, PerpError> {
    let market = storage::load_market(context, market_id)?
        .ok_or_else(|| perp_err("placeOrder: unknown market"))?;
    if !market.active {
        return Err(perp_err("placeOrder: market not active"));
    }

    let side = Side::from_u8(side_u8).ok_or_else(|| perp_err("placeOrder: invalid side"))?;
    let order_type = OrderType::from_u8(order_type_u8)
        .ok_or_else(|| perp_err("placeOrder: invalid orderType"))?;
    let tif = TimeInForce::from_u8(tif_u8).ok_or_else(|| perp_err("placeOrder: invalid tif"))?;

    if quantity < market.min_quantity {
        return Err(perp_err("placeOrder: quantity below minimum"));
    }
    if quantity > market.max_quantity {
        return Err(perp_err("placeOrder: quantity exceeds maximum"));
    }
    if market.step_size > 0 && quantity % market.step_size != 0 {
        return Err(perp_err("placeOrder: quantity not multiple of step_size"));
    }
    if order_type == OrderType::Limit {
        if price == 0 {
            return Err(perp_err("placeOrder: limit order price must be > 0"));
        }
        if price > market.max_price {
            return Err(perp_err("placeOrder: price exceeds maximum"));
        }
        if market.tick_size > 0 && price % market.tick_size != 0 {
            return Err(perp_err("placeOrder: price not multiple of tick_size"));
        }
        // No placement-time price band: an out-of-band limit order is allowed to REST
        // (a deep passive order is harmless — it can only ever fill if the mark legitimately
        // reaches it). The off-mark guard lives at FILL time in `match_order` /
        // `check_fok_feasibility` (see `math::mark_band_bounds`), which is immune to
        // post-placement mark drift and also bounds market-order slippage. The upper price
        // sanity cap (`price > market.max_price`) above still prevents absurd book pollution.
    }

    Ok(ValidatedOrder {
        market,
        side,
        order_type,
        tif,
    })
}

/// The `OrderPlaced` log of a placement that is not yet known-accepted.
///
/// `announce_new_order` runs BEFORE every genuine reject (PostOnly-would-cross, FOK-unfillable,
/// the taker K9 / wallet-cover / fills+rest rejects in `finalize_compute`, `rest_in_book`'s margin
/// rejects), so it may not emit: today a per-call `Err` reverts the frame and truncates the logs,
/// but the batch selectors CATCH a per-item error and return `Ok` overall — the frame does not
/// revert, and an `OrderPlaced` for an order that never existed would survive in the receipts.
///
/// The EVM journal is append-only (a log cannot be un-emitted), so the log is BUFFERED here and
/// flushed by [`emit_pending_order_placed`] at the first APPLY point the place path reaches. That
/// preserves the original log-stream position: nothing between the announce and the first apply
/// emits a log (the match walk is write- and log-free; every `Trade` / `PositionChanged` /
/// `OrderCancelled` / `InsuranceFund*` is deferred into `MatchRegistry::flush`), so `OrderPlaced`
/// still comes first for its order.
pub(super) struct PendingOrderPlaced {
    user: Address,
    market_id: u64,
    order_id: [u8; 32],
    side: u8,
    price: u64,
    quantity: u64,
    order_type: u8,
    tif: u8,
    client_order_id: [u8; 16],
}

/// Flushes the buffered `OrderPlaced` — **exactly once**: the `take()` makes every later call a
/// no-op, so all apply sites can call it unconditionally. Also a no-op for callers that have no
/// pending log (the liquidation close emits its own `OrderPlaced` and passes `&mut None`).
pub(super) fn emit_pending_order_placed<H: PerpHost>(
    context: &mut H,
    pending: &mut Option<PendingOrderPlaced>,
) {
    let Some(p) = pending.take() else {
        return;
    };
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderPlaced {
            user: p.user,
            marketId: p.market_id,
            orderId: FixedBytes(p.order_id),
            side: p.side,
            price: p.price,
            quantity: p.quantity,
            orderType: p.order_type,
            tif: p.tif,
            clientOrderId: FixedBytes(p.client_order_id),
        }
        .to_log_data(),
    });
}

/// Builds the taker `Order` in memory plus its not-yet-emitted `OrderPlaced`
/// ([`PendingOrderPlaced`]). Read-only: no storage write, and — unlike before — no log either.
fn announce_new_order(
    account: Address,
    order_id: &[u8; 32],
    market_id: u64,
    price: u64,
    quantity: u64,
    client_order_id: [u8; 16],
    validated: &ValidatedOrder,
) -> (Order, PendingOrderPlaced) {
    let order = Order {
        owner: account.0 .0,
        market_id,
        side: validated.side,
        price,
        quantity,
        filled: 0,
        order_type: validated.order_type,
        tif: validated.tif,
        status: OrderStatus::Open,
    };

    let pending = PendingOrderPlaced {
        user: account,
        market_id,
        order_id: *order_id,
        side: validated.side as u8,
        price,
        quantity,
        order_type: validated.order_type as u8,
        tif: validated.tif as u8,
        client_order_id,
    };

    (order, pending)
}

/// Binance-style TIF expiry (IOC/market not fully filled) → Expired, not Cancelled (which is
/// reserved for user-initiated cancels). In-memory only; the caller performs the final persist.
fn cancel_unfilled_remainder(taker_order: &mut Order, remaining: u64) {
    if remaining > 0 {
        taker_order.status = OrderStatus::Expired;
    }
}

fn ensure_fok_filled(remaining: u64) -> Result<(), PerpError> {
    if remaining == 0 {
        Ok(())
    } else {
        Err(perp_err("placeOrder: FOK order cannot be fully filled"))
    }
}

/// PostOnly cross check. Reads the BBO ONCE (both sides, one MarketHot probe) and RETURNS it so the
/// caller can thread it into `rest_in_book` (no match runs on the PostOnly path, so the BBO stays
/// current from here to the rest).
fn ensure_post_only_does_not_cross<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    price: u64,
) -> Result<(u64, u64), PerpError> {
    let h = storage::load_market_hot(context, market_id)?;
    let (best_bid, best_ask) = (h.best_bid, h.best_ask);
    match side {
        Side::Buy => {
            if best_ask != 0 && best_ask <= price {
                return Err(perp_err("placeOrder: PostOnly order would match"));
            }
        }
        Side::Sell => {
            if best_bid != 0 && best_bid >= price {
                return Err(perp_err("placeOrder: PostOnly order would match"));
            }
        }
    }

    Ok((best_bid, best_ask))
}

fn remove_order_entry(
    entries: &mut std::collections::VecDeque<OrderEntry>,
    order_id: &[u8; 32],
    side_label: &str,
) -> Result<OrderEntry, PerpError> {
    let idx = entries
        .iter()
        .position(|e| &e.order_id == order_id)
        .ok_or_else(|| {
            perp_invariant_err(format!(
                "{side_label} entry for order {:?} not found during cancel",
                order_id
            ))
        })?;
    // VecDeque::remove returns Option (unlike Vec::remove); idx came from position() so it is Some.
    Ok(entries
        .remove(idx)
        .expect("index from position() is in bounds"))
}

/// #B: index of `order_id` in a PRICE-SORTED list, found via binary-search to the price-run
/// (O(log n)) then a scan of that small same-price run — instead of an O(n) id scan. The order's
/// `price` is always known at the call site (the loaded Order / the matched level). `buy_side`
/// picks the sort direction (buys DESC, sells ASC). Returns None if absent (price not in the list,
/// or no entry in the price-run carries `order_id`).
pub(super) fn find_entry_by_price_id(
    entries: &std::collections::VecDeque<OrderEntry>,
    order_id: &[u8; 32],
    price: u64,
    buy_side: bool,
) -> Option<usize> {
    let search = || -> Option<usize> {
        let probe = if buy_side {
            entries.binary_search_by(|e| price.cmp(&e.price)) // DESC: monotone Less→Eq→Greater
        } else {
            entries.binary_search_by(|e| e.price.cmp(&price)) // ASC
        };
        let anchor = probe.ok()?;
        // Walk to the start of the equal-price run, then scan it for the order_id (run is tiny:
        // per-price occupancy p50=1 / p90=3).
        let mut start = anchor;
        while start > 0 && entries[start - 1].price == price {
            start -= 1;
        }
        let mut k = start;
        while k < entries.len() && entries[k].price == price {
            if &entries[k].order_id == order_id {
                return Some(k);
            }
            k += 1;
        }
        None
    };
    let result = search();
    // Guard rail: the binary search converts "list happens to be price-sorted" from a perf property
    // into a CORRECTNESS precondition (the old linear id-scan was order-agnostic). order_ids are
    // unique per side, so the binary-search index MUST equal the linear-scan index — assert it in
    // debug builds (zero release cost). This turns any future sortedness regression (buys DESC /
    // sells ASC broken by some later change) into a loud test failure here instead of a silent
    // "entry not found" revert at cancel / fill time.
    debug_assert_eq!(
        result,
        entries.iter().position(|e| &e.order_id == order_id),
        "find_entry_by_price_id disagrees with linear id scan — per-account order list is not \
         price-sorted (buys DESC / sells ASC invariant broken?)"
    );
    result
}

/// #B: `remove_order_entry` for the hot cancel path — O(log n) via [`find_entry_by_price_id`]
/// instead of the O(n) id scan. Byte-identical result to `remove_order_entry` (removes the same
/// unique entry; order_ids are unique per side).
fn remove_order_entry_by_price(
    entries: &mut std::collections::VecDeque<OrderEntry>,
    order_id: &[u8; 32],
    price: u64,
    buy_side: bool,
    side_label: &str,
) -> Result<OrderEntry, PerpError> {
    let idx = find_entry_by_price_id(entries, order_id, price, buy_side).ok_or_else(|| {
        perp_invariant_err(format!(
            "{side_label} entry for order {:?} not found during cancel",
            order_id
        ))
    })?;
    Ok(entries
        .remove(idx)
        .expect("index from find_entry_by_price_id is in bounds"))
}

fn execute_limit_order<H: PerpHost>(
    context: &mut H,
    account: Address,
    order_id: [u8; 32],
    market_id: u64,
    price: u64,
    quantity: u64,
    client_order_id: [u8; 16],
    order: ValidatedOrder,
    taker_order: &mut Order,
    pending_placed: &mut Option<PendingOrderPlaced>,
) -> Result<(), PerpError> {
    match order.tif {
        TimeInForce::PostOnly => {
            // No match runs → the do-not-cross BBO is still current at rest; thread it in.
            let bbo = ensure_post_only_does_not_cross(context, market_id, order.side, price)?;
            rest_in_book(
                context,
                account,
                &order_id,
                market_id,
                order.side,
                price,
                quantity,
                order.tif,
                client_order_id,
                &order.market,
                Some(bbo),
                pending_placed,
            )
        }
        TimeInForce::Gtc => {
            let remaining = match_order(
                context,
                account,
                &order_id,
                market_id,
                order.side,
                price,
                quantity,
                order.order_type,
                order.tif,
                &order.market,
                false,
                true, // GTC: rest the remainder → pre-validate its margin atomically with fills
                taker_order,
                pending_placed,
            )?;
            if remaining > 0 {
                rest_in_book(
                    context,
                    account,
                    &order_id,
                    market_id,
                    order.side,
                    price,
                    remaining,
                    order.tif,
                    client_order_id,
                    &order.market,
                    // GTC: matching ran → read the (post-match) BBO inside rest_in_book.
                    None,
                    pending_placed,
                )?;
            }
            Ok(())
        }
        TimeInForce::Ioc => {
            let remaining = match_order(
                context,
                account,
                &order_id,
                market_id,
                order.side,
                price,
                quantity,
                order.order_type,
                order.tif,
                &order.market,
                false,
                false, // IOC: unmatched remainder is dropped, never rested
                taker_order,
                pending_placed,
            )?;
            cancel_unfilled_remainder(taker_order, remaining);
            Ok(())
        }
        TimeInForce::Fok => {
            check_fok_feasibility(
                context,
                market_id,
                order.side,
                price,
                quantity,
                order.order_type,
                &order.market,
            )?;
            let remaining = match_order(
                context,
                account,
                &order_id,
                market_id,
                order.side,
                price,
                quantity,
                order.order_type,
                order.tif,
                &order.market,
                false,
                false, // FOK: fully filled or rejected — never rests
                taker_order,
                pending_placed,
            )?;
            ensure_fok_filled(remaining)
        }
    }
}

fn execute_market_order<H: PerpHost>(
    context: &mut H,
    account: Address,
    order_id: [u8; 32],
    market_id: u64,
    price: u64,
    quantity: u64,
    order: ValidatedOrder,
    taker_order: &mut Order,
    pending_placed: &mut Option<PendingOrderPlaced>,
) -> Result<(), PerpError> {
    if order.tif == TimeInForce::Fok {
        check_fok_feasibility(
            context,
            market_id,
            order.side,
            price,
            quantity,
            order.order_type,
            &order.market,
        )?;
    }
    let remaining = match_order(
        context,
        account,
        &order_id,
        market_id,
        order.side,
        price,
        quantity,
        order.order_type,
        order.tif,
        &order.market,
        false,
        false, // market order: never rests
        taker_order,
        pending_placed,
    )?;
    if order.tif == TimeInForce::Fok {
        ensure_fok_filled(remaining)
    } else {
        cancel_unfilled_remainder(taker_order, remaining);
        Ok(())
    }
}

fn cancel_order_core<H: PerpHost>(
    account: Address,
    order_id: [u8; 32],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let order = storage::load_order(context, &order_id)?
        .ok_or_else(|| perp_err("cancelOrder: order not found"))?;

    if order.owner != account.0 .0 {
        return Err(perp_err("cancelOrder: not owner"));
    }
    if !matches!(
        order.status,
        OrderStatus::Open | OrderStatus::PartiallyFilled
    ) {
        return Err(perp_err("cancelOrder: order not cancellable"));
    }

    let market_id = order.market_id;
    let market = storage::load_market_ref(context, market_id)?
        .ok_or_else(|| perp_err("cancelOrder: unknown market"))?;
    execute_order_cancellation(
        context,
        account,
        market_id,
        order_id,
        order,
        &market,
        // Explicit cancel: no matching ran in this call, so the BBO cache is live.
        remove_from_book_after_cancel,
    )?;
    Ok(Bytes::new())
}

// ── Matching engine ───────────────────────────────────────────────────────────

/// Core matching loop.  Returns the unfilled quantity after matching.
pub(super) fn match_order<H: PerpHost>(
    context: &mut H,
    taker_addr: Address,
    taker_order_id: &[u8; 32],
    market_id: u64,
    side: Side,
    limit_price: u64,
    quantity: u64,
    order_type: OrderType,
    tif: TimeInForce,
    market: &crate::types::Market,
    // When true, this is a liquidation close: the taker fee is waived and the number of distinct
    // maker accounts is capped so balance after-image gas has a fixed pre-write upper bound.
    liquidation_close: bool,
    // When true (GTC), the caller will rest the unmatched remainder — so the rest's margin is
    // pre-validated atomically with the fills (commit-only #23 atomic-reject).
    rest_remainder: bool,
    // The taker's Order threaded in memory (commit-only #23): NOT yet persisted — the caller
    // performs the single final save after every genuine reject has passed, so a rejected
    // placement leaves no phantom order (and a signed order's signature is not burned).
    taker_order: &mut Order,
    // The place path's buffered `OrderPlaced`, flushed at the APPLY below so it precedes this
    // order's own Trade/PositionChanged/OrderCancelled events. `&mut None` for callers that emit
    // their own OrderPlaced (the liquidation close).
    pending_placed: &mut Option<PendingOrderPlaced>,
) -> Result<u64, PerpError> {
    let mut remaining = quantity;
    let mut last_trade_price = None;
    let mut taker_settlement =
        TakerSettlement::load(context, taker_addr, market_id, liquidation_close)?;
    // commit-only #23 L1: per-user working copies for this match. Each touched maker is loaded
    // once (funding settled at first touch, exactly where the first per-maker settle did it) and
    // saved once at the flush below — same write-key set and net values as the old per-fill saves.
    let mut registry = settlement::MatchRegistry::new();
    // The taker order is mutated in memory per fill; the CALLER persists the final state once.
    // Nothing reads it from storage mid-match: the registry touches only the makers, finalize
    // touches the taker's position/account, and the taker order is not in any book queue yet.

    // Fill-time price band: a fill may never execute farther than ±band from the CURRENT
    // mark, regardless of order type. Because each side of the book is sorted, this is a
    // clean early break — the first out-of-band level ends matching and everything past it
    // (necessarily farther) is skipped. This is the sole off-mark guard (there is no
    // placement-time band): it sees post-placement mark drift, lets harmless deep passive
    // orders rest, and gives market orders implicit slippage protection. mark_price is a
    // field of the Market already loaded — no separate storage read.
    let mark = market.mark_price;
    let (mark_upper, mark_lower) =
        crate::math::mark_band_bounds(mark, market.price_band_bps);

    match side {
        Side::Buy => {
            // Match against asks (sorted ASC: lowest ask first).
            let ask_prices = storage::load_ask_prices_ref(context, market_id)?;
            let old_best_ask = ask_prices.first().copied().unwrap_or(0);
            let mut ask_levels_cleared = false;
            let mut removed_asks: Vec<u64> = Vec::new();
            'outer: for ask_price in ask_prices.iter().copied() {
                // Fill-time band (lower): an ask below mark-band is an off-mark price
                // (e.g. a closing maker dumping cheap); skip it — higher, in-band asks may
                // still match. It stays resting until the mark legitimately reaches it.
                if (ask_price as u128) < mark_lower {
                    continue;
                }
                // For limit buy: only match if ask_price <= our limit.
                if order_type == OrderType::Limit && ask_price > limit_price {
                    break;
                }
                // Fill-time band (upper): asks above mark+band end matching (ascending).
                if ask_price as u128 > mark_upper {
                    break;
                }
                // ONE probe gets both the FIFO ids and the live count (Obs-1 merge).
                let blob = storage::load_ask_level_arc(context, market_id, ask_price)?;
                let count_old = blob.count;
                let queue = &blob.ids;
                let mut new_queue: Vec<[u8; 32]> = Vec::new();
                // Live makers that LEAVE this level during the walk (fully filled / K9-rejected).
                // The post-walk live count is `count_old - level_removed`; stale ids the walk sweeps
                // (already gone from the book) are NOT counted here — they were decremented when
                // they left. See `finalize_level_count` below.
                let mut level_removed = 0u64;
                let mut qi = 0;

                while qi < queue.len() {
                    if remaining == 0 {
                        new_queue.extend(queue[qi..].iter().copied());
                        let count_new = count_old.saturating_sub(level_removed);
                        if count_new == 0 {
                            // Level logically empty (any ids left in new_queue are stale) — drop the
                            // price; SaveLevel with count 0 deletes the blob (clears stale ids).
                            registry.push_event(settlement::MatchEvent::RemovePrice {
                                is_bid: false,
                                price: ask_price,
                            });
                            removed_asks.push(ask_price);
                            ask_levels_cleared = true;
                            registry.push_event(settlement::MatchEvent::SaveLevel {
                                is_bid: false,
                                price: ask_price,
                                queue: Vec::new(),
                                count: 0,
                            });
                        } else {
                            // Live orders remain in the untouched tail — keep it verbatim (any stale
                            // ids ride along and are swept on the next walk).
                            registry.push_event(settlement::MatchEvent::SaveLevel {
                                is_bid: false,
                                price: ask_price,
                                queue: new_queue,
                                count: count_new,
                            });
                        }
                        break 'outer;
                    }
                    let maker_id = queue[qi];
                    qi += 1;

                    let mut maker_order = match storage::load_order(context, &maker_id)? {
                        Some(o)
                            if matches!(
                                o.status,
                                OrderStatus::Open | OrderStatus::PartiallyFilled
                            ) =>
                        {
                            o
                        }
                        // lazy-queue sweep: a cancelled/filled maker was deleted (delete-on-terminal)
                        // but its id lingers in the FIFO — drop it (not re-queued), no count change
                        // (it was decremented when it left the book).
                        _ => continue,
                    };
                    let available = maker_order.quantity - maker_order.filled;
                    let fill_qty = remaining.min(available);

                    let maker_addr = Address::from(maker_order.owner);
                    if liquidation_close
                        && !registry.can_touch_user(maker_addr, MAX_LIQUIDATION_MAKER_ACCOUNTS)
                    {
                        new_queue.push(maker_id);
                        new_queue.extend(queue[qi..].iter().copied());
                        let count_new = count_old.saturating_sub(level_removed);
                        registry.defer_maker_level(false, ask_price, new_queue, count_new)?;
                        break 'outer;
                    }
                    // Maker open-solvency guard (K9): settle the maker first. If filling
                    // it would open its position below maintenance at mark, cancel the
                    // maker order (drop it from this level by not re-queuing) and skip —
                    // the taker's `remaining` is untouched so it keeps matching. Funding
                    // is settled+persisted inside either way.
                    let maker_fee = match settlement::settle_maker_fill_registry(
                        context,
                        &mut registry,
                        maker_addr,
                        &maker_id,
                        market_id,
                        ask_price,
                        fill_qty,
                        Side::Buy,
                        market,
                    )? {
                        MakerFillOutcome::Filled { maker_fee } => maker_fee,
                        MakerFillOutcome::RejectedInsolvent => {
                            settlement::cancel_rejected_maker_registry(
                                context,
                                &mut registry,
                                maker_addr,
                                market_id,
                                Side::Sell,
                                &maker_id,
                                &mut maker_order,
                                market,
                            )?;
                            // A live maker left the level (cancelled): count it. Dropped from the
                            // queue (not re-queued); if it was the last live order the count hits 0
                            // and the level is removed below.
                            level_removed += 1;
                            continue;
                        }
                    };
                    taker_settlement.record_fill(ask_price, fill_qty, Side::Buy, market)?;
                    let fill_notional = calc_value(
                        ask_price,
                        fill_qty,
                        market.base_decimals,
                        market.price_decimals,
                    )?;
                    let taker_fee =
                        calc_trading_fee(fill_notional, taker_settlement.taker_fee_bps())?;
                    last_trade_price = Some(ask_price);
                    registry.push_event(settlement::MatchEvent::Trade {
                        market_id,
                        taker_order_id: *taker_order_id,
                        maker_order_id: maker_id,
                        taker: taker_addr,
                        maker: Address::from(maker_order.owner),
                        price: ask_price,
                        quantity: fill_qty,
                        taker_side: Side::Buy,
                        taker_fee,
                        maker_fee,
                    });

                    // Update the maker order in place — the registry settle does not touch the
                    // maker Order struct, so the value loaded above is still current.
                    maker_order.filled += fill_qty;
                    maker_order.status = if maker_order.filled >= maker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    // delete-on-terminal: a fully-filled maker leaves the map (and the level, via
                    // level_removed); a partial fill stays and is re-queued below.
                    if maker_order.status == OrderStatus::Filled {
                        registry
                            .push_event(settlement::MatchEvent::DeleteOrder { order_id: maker_id });
                        level_removed += 1;
                    } else {
                        registry.push_event(settlement::MatchEvent::SaveOrder {
                            order_id: maker_id,
                            order: maker_order.clone(),
                        });
                    }

                    // Accumulate into the hoisted taker order (saved once after the loop).
                    taker_order.filled += fill_qty;
                    taker_order.status = if taker_order.filled >= taker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };

                    remaining -= fill_qty;
                    if maker_order.status == OrderStatus::PartiallyFilled {
                        new_queue.push(maker_id);
                    }
                }

                // Normal level end (taker still had capacity → consumed every live maker here).
                let count_new = count_old.saturating_sub(level_removed);
                if count_new == 0 {
                    registry.push_event(settlement::MatchEvent::RemovePrice {
                        is_bid: false,
                        price: ask_price,
                    });
                    removed_asks.push(ask_price);
                    ask_levels_cleared = true;
                    registry.push_event(settlement::MatchEvent::SaveLevel {
                        is_bid: false,
                        price: ask_price,
                        queue: Vec::new(),
                        count: 0,
                    });
                } else {
                    registry.push_event(settlement::MatchEvent::SaveLevel {
                        is_bid: false,
                        price: ask_price,
                        queue: new_queue,
                        count: count_new,
                    });
                }
            }
            if ask_levels_cleared {
                // New best ask from the walk's own knowledge: the price-index snapshot minus the
                // levels this match emptied — identical to what refresh_best_ask reads after the
                // (deferred) removals are applied.
                let best_ask = ask_prices
                    .iter()
                    .copied()
                    .find(|p| !removed_asks.contains(p))
                    .unwrap_or(0);
                registry.push_event(settlement::MatchEvent::SaveBest {
                    is_bid: false,
                    price: best_ask,
                });
                if best_ask != old_best_ask {
                    let best_bid = storage::load_best_bid(context, market_id)?;
                    registry
                        .push_event(settlement::MatchEvent::MidPriceSample { best_bid, best_ask });
                }
            }
        }
        Side::Sell => {
            // Match against bids (sorted DESC: highest bid first).
            let bid_prices = storage::load_bid_prices_ref(context, market_id)?;
            let old_best_bid = bid_prices.last().copied().unwrap_or(0); // bids: best = max
            let mut bid_levels_cleared = false;
            let mut removed_bids: Vec<u64> = Vec::new();
            'outer: for bid_price in bid_prices.iter().rev().copied() {
                // Fill-time band (upper): a bid above mark+band is an off-mark price
                // (e.g. a closing maker buying rich); skip it — lower, in-band bids may
                // still match. It stays resting until the mark legitimately reaches it.
                if (bid_price as u128) > mark_upper {
                    continue;
                }
                // For limit sell: only match if bid_price >= our limit.
                if order_type == OrderType::Limit && bid_price < limit_price {
                    break;
                }
                // Fill-time band (lower): bids below mark-band end matching (descending).
                if (bid_price as u128) < mark_lower {
                    break;
                }
                // ONE probe gets both the FIFO ids and the live count (Obs-1 merge).
                let blob = storage::load_bid_level_arc(context, market_id, bid_price)?;
                let count_old = blob.count;
                let queue = &blob.ids;
                let mut new_queue: Vec<[u8; 32]> = Vec::new();
                // See the mirror on the Buy side.
                let mut level_removed = 0u64;
                let mut qi = 0;

                while qi < queue.len() {
                    if remaining == 0 {
                        new_queue.extend(queue[qi..].iter().copied());
                        let count_new = count_old.saturating_sub(level_removed);
                        if count_new == 0 {
                            registry.push_event(settlement::MatchEvent::RemovePrice {
                                is_bid: true,
                                price: bid_price,
                            });
                            removed_bids.push(bid_price);
                            bid_levels_cleared = true;
                            registry.push_event(settlement::MatchEvent::SaveLevel {
                                is_bid: true,
                                price: bid_price,
                                queue: Vec::new(),
                                count: 0,
                            });
                        } else {
                            registry.push_event(settlement::MatchEvent::SaveLevel {
                                is_bid: true,
                                price: bid_price,
                                queue: new_queue,
                                count: count_new,
                            });
                        }
                        break 'outer;
                    }
                    let maker_id = queue[qi];
                    qi += 1;

                    let mut maker_order = match storage::load_order(context, &maker_id)? {
                        Some(o)
                            if matches!(
                                o.status,
                                OrderStatus::Open | OrderStatus::PartiallyFilled
                            ) =>
                        {
                            o
                        }
                        // lazy-queue sweep (see the Buy mirror): stale id → drop, no count change.
                        _ => continue,
                    };
                    let available = maker_order.quantity - maker_order.filled;
                    let fill_qty = remaining.min(available);

                    let maker_addr = Address::from(maker_order.owner);
                    if liquidation_close
                        && !registry.can_touch_user(maker_addr, MAX_LIQUIDATION_MAKER_ACCOUNTS)
                    {
                        new_queue.push(maker_id);
                        new_queue.extend(queue[qi..].iter().copied());
                        let count_new = count_old.saturating_sub(level_removed);
                        registry.defer_maker_level(true, bid_price, new_queue, count_new)?;
                        break 'outer;
                    }
                    // Maker open-solvency guard (K9) — see the mirror on the Buy side.
                    let maker_fee = match settlement::settle_maker_fill_registry(
                        context,
                        &mut registry,
                        maker_addr,
                        &maker_id,
                        market_id,
                        bid_price,
                        fill_qty,
                        Side::Sell,
                        market,
                    )? {
                        MakerFillOutcome::Filled { maker_fee } => maker_fee,
                        MakerFillOutcome::RejectedInsolvent => {
                            settlement::cancel_rejected_maker_registry(
                                context,
                                &mut registry,
                                maker_addr,
                                market_id,
                                Side::Buy,
                                &maker_id,
                                &mut maker_order,
                                market,
                            )?;
                            // A live maker left the level (see the Buy mirror): count it.
                            level_removed += 1;
                            continue;
                        }
                    };
                    taker_settlement.record_fill(bid_price, fill_qty, Side::Sell, market)?;
                    let fill_notional = calc_value(
                        bid_price,
                        fill_qty,
                        market.base_decimals,
                        market.price_decimals,
                    )?;
                    let taker_fee =
                        calc_trading_fee(fill_notional, taker_settlement.taker_fee_bps())?;
                    last_trade_price = Some(bid_price);
                    registry.push_event(settlement::MatchEvent::Trade {
                        market_id,
                        taker_order_id: *taker_order_id,
                        maker_order_id: maker_id,
                        taker: taker_addr,
                        maker: Address::from(maker_order.owner),
                        price: bid_price,
                        quantity: fill_qty,
                        taker_side: Side::Sell,
                        taker_fee,
                        maker_fee,
                    });

                    maker_order.filled += fill_qty;
                    maker_order.status = if maker_order.filled >= maker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    // delete-on-terminal (see the Buy mirror).
                    if maker_order.status == OrderStatus::Filled {
                        registry
                            .push_event(settlement::MatchEvent::DeleteOrder { order_id: maker_id });
                        level_removed += 1;
                    } else {
                        registry.push_event(settlement::MatchEvent::SaveOrder {
                            order_id: maker_id,
                            order: maker_order.clone(),
                        });
                    }

                    taker_order.filled += fill_qty;
                    taker_order.status = if taker_order.filled >= taker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };

                    remaining -= fill_qty;
                    if maker_order.status == OrderStatus::PartiallyFilled {
                        new_queue.push(maker_id);
                    }
                }

                let count_new = count_old.saturating_sub(level_removed);
                if count_new == 0 {
                    registry.push_event(settlement::MatchEvent::RemovePrice {
                        is_bid: true,
                        price: bid_price,
                    });
                    removed_bids.push(bid_price);
                    bid_levels_cleared = true;
                    registry.push_event(settlement::MatchEvent::SaveLevel {
                        is_bid: true,
                        price: bid_price,
                        queue: Vec::new(),
                        count: 0,
                    });
                } else {
                    registry.push_event(settlement::MatchEvent::SaveLevel {
                        is_bid: true,
                        price: bid_price,
                        queue: new_queue,
                        count: count_new,
                    });
                }
            }
            if bid_levels_cleared {
                // New best bid = max non-removed price in the snapshot (mirrors refresh_best_bid
                // reading the index after the deferred removals).
                let best_bid = bid_prices
                    .iter()
                    .rev()
                    .copied()
                    .find(|p| !removed_bids.contains(p))
                    .unwrap_or(0);
                registry.push_event(settlement::MatchEvent::SaveBest {
                    is_bid: true,
                    price: best_bid,
                });
                if best_bid != old_best_bid {
                    let best_ask = storage::load_best_ask(context, market_id)?;
                    registry
                        .push_event(settlement::MatchEvent::MidPriceSample { best_bid, best_ask });
                }
            }
        }
    }

    // ── commit-only #23 L2b: the walk above performed ZERO storage writes (all effects live in
    // the registry copies + ordered events), so every genuine reject here leaves state untouched.

    // 1. FOK: unfillable → reject with zero writes (previously the whole match committed and the
    //    caller's post-hoc check reverted it via undo).
    if tif == TimeInForce::Fok && remaining > 0 {
        return Err(perp_err("placeOrder: FOK order cannot be fully filled"));
    }

    // 2. Taker settlement compute: K9 open-into-insolvency / wallet-cover / checked-arithmetic
    //    rejects — all pre-write. The taker joins the registry (self-match reuses the evolved
    //    copies) and its fill effects are flushed with everyone else's below. When the caller will
    //    REST the remainder (GTC), pass the rest requirement so the fills+rest margin is validated
    //    atomically here (else the fills commit and rest_in_book could revert, leaking them).
    let rest_req = if rest_remainder && remaining > 0 {
        Some(settlement::RestReq {
            price: limit_price,
            qty: remaining,
        })
    } else {
        None
    };
    let taker_plan =
        taker_settlement.finalize_compute(context, &mut registry, side, market, rest_req)?;

    // ── APPLY (no genuine rejects past this point) ──
    // Flush the buffered OrderPlaced FIRST so it precedes this order's own Trade /
    // PositionChanged events, which the flush below emits (log order preserved).
    //
    // UNCONDITIONAL, and provably so: `finalize_compute` validates the GTC rest even when the fill
    // set is EMPTY, so every genuine reject of this placement — PostOnly-cross, FOK-unfillable, the
    // taker K9 / wallet-cover / fills+rest margin rejects, and now the zero-fill rest-margin reject
    // that used to surface only later in `rest_in_book` — has already fired above, pre-flush and
    // write-clean. `rest_in_book` re-checks the same formula on the same post-flush state, so its
    // reject can no longer be reached from here. What is left past this point is invariant /
    // arithmetic guards, plus `finalize_apply`'s two residuals: the wallet-cover check (an
    // unreachable invariant — the compute phase simulated the same cancel loop) and
    // `credit_fee_recipient`'s "fee recipient not initialised", which needs an UNSET admin and is
    // therefore unreachable on any live chain (a market cannot be added without a non-zero admin,
    // and neither `initAdmin` nor `transferAdmin` can set one back to zero). Nothing a user can
    // provoke rejects after this line, so "emitted ⟺ accepted" holds with no exception.
    emit_pending_order_placed(context, pending_placed);
    registry.flush(context, market_id)?;
    if let Some(plan) = taker_plan {
        settlement::finalize_apply(context, plan, side, market)?;
    }
    if let Some(price) = last_trade_price {
        storage::save_last_traded_price(context, market_id, price)?;
    }
    Ok(remaining)
}

// ── Resting in book ───────────────────────────────────────────────────────────

/// Place a limit order that did not (fully) match into the order book and lock margin.
///
/// The margin model uses `max(buy_reserved, sell_reserved)` so only the dominant side
/// actually locks capital.  Adding an order on the weaker side only increases the wallet
/// deduction when it surpasses the other side's reservation.
/// #A oracle (debug builds only): assert the maintained per-side aggregates on `pos` equal a fresh
/// recompute from the actual order lists. A divergence means a mutation site failed to keep the
/// totals in sync — caught loudly in tests / the correctness gate, compiled out in release.
#[inline]
fn debug_assert_totals(
    buy: impl Iterator<Item = OrderEntry>,
    sell: impl Iterator<Item = OrderEntry>,
    pos: &crate::types::PerpPosition,
    base_decimals: u32,
    price_decimals: u32,
) {
    #[cfg(debug_assertions)]
    {
        let (bq, bn) = crate::math::sum_side_totals(buy, base_decimals, price_decimals)
            .expect("sum buy totals");
        let (sq, sn) = crate::math::sum_side_totals(sell, base_decimals, price_decimals)
            .expect("sum sell totals");
        debug_assert_eq!(
            (bq, bn, sq, sn),
            (
                pos.total_buy_qty,
                pos.total_buy_notional,
                pos.total_sell_qty,
                pos.total_sell_notional
            ),
            "#A maintained reservation totals diverged from the order lists"
        );
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = (buy, sell, pos, base_decimals, price_decimals);
    }
}

/// Consuming a Deque-or-slice as an OrderEntry iterator for the reservation calls / oracle.
#[inline]
fn entries_iter(
    v: &std::collections::VecDeque<OrderEntry>,
) -> impl Iterator<Item = OrderEntry> + Clone + '_ {
    v.iter().copied()
}

fn rest_in_book<H: PerpHost>(
    context: &mut H,
    user: Address,
    order_id: &[u8; 32],
    market_id: u64,
    side: Side,
    price: u64,
    qty: u64,
    tif: TimeInForce,
    client_order_id: [u8; 16],
    market: &crate::types::Market,
    // 2b resolve-once: (best_bid, best_ask). `Some` = the caller already read the BBO (PostOnly
    // threads its do-not-cross read — no match ran, so it is still current). `None` = read it here
    // once. rest runs AFTER matching (GTC), and matching only moves the OPPOSITE side from the one
    // we rest on, so a rest-time read yields both bests current — no staleness.
    bbo: Option<(u64, u64)>,
    // The place path's buffered `OrderPlaced`, flushed at the top of the APPLY block below (so it
    // lands before this order's `OrderRested`, and only once the margin rejects have passed).
    // Already-`None` on the GTC path when the match flush emitted it.
    pending_placed: &mut Option<PendingOrderPlaced>,
) -> Result<(), PerpError> {
    let mut pos = storage::load_position(context, user, market_id)?;
    // Resting escrows nothing, so the account is READ-ONLY here: the fee rate (folded into the
    // account blob) for the book entry, and the cross wallet for the derived admission gate. An
    // `_ref` read — no owned clone, and no `save_account` at the end, so no spurious
    // `AccountBalanceChanged` for a call that moves no money.
    let (maker_fee_bps, wallet) = {
        let account = storage::load_account_ref(context, user)?;
        (account.maker_fee_bps, account.perp_wallet_balance)
    };
    // ONE BBO resolve for both the best-update check and the mid-price sample (was up to two
    // separate load_best_bid/load_best_ask reads per arm).
    let (best_bid, best_ask) = match bbo {
        Some(b) => b,
        None => {
            let h = storage::load_market_hot(context, market_id)?;
            (h.best_bid, h.best_ask)
        }
    };

    match side {
        Side::Buy => {
            // commit-only #23 CLONE-FREE probe: the hypothetical "buy-list ⊕ new_entry" is never
            // materialised — the derived requirement reads only the per-side AGGREGATES, so the
            // whole probe is two `checked_add`s. A reject below therefore leaves the overlay
            // untouched (validate-then-apply: check first, insert after).
            let buy_ref = storage::load_buy_orders_ref(context, user, market_id)?;
            let new_entry = OrderEntry {
                order_id: *order_id,
                price,
                amount: qty,
                maker_fee_bps,
            };
            let (bd, pd) = (market.base_decimals, market.price_decimals);
            #[cfg(debug_assertions)]
            {
                // The aggregates ARE `Bid`/`Ask` and now feed the admission gate directly, so keep
                // the raw-list fold as a live oracle on them (debug-only: the two list loads exist
                // for this check alone).
                let sell_entries = storage::load_sell_orders_ref(context, user, market_id)?;
                debug_assert_totals(
                    entries_iter(&buy_ref),
                    entries_iter(&sell_entries),
                    &pos,
                    bd,
                    pd,
                );
            }
            // `Bid` grows by this order's notional at its LIMIT price — the same per-order-floored
            // `calc_value` term the fold would contribute, so the aggregate stays exactly Binance's
            // `bidNotional`.
            let entry_notional = crate::math::calc_value(price, qty, bd, pd)?;
            let new_tbq = pos
                .total_buy_qty
                .checked_add(qty)
                .ok_or_else(|| perp_err("placeOrder: total buy qty overflow"))?;
            let new_tbn = pos
                .total_buy_notional
                .checked_add(entry_notional)
                .ok_or_else(|| perp_err("placeOrder: total buy notional overflow"))?;

            // ── Derived-ooIM admission gate ──
            // Requirement = `ooIM(after) − ooIM(before)` for THIS market; available =
            // `perp_wallet_balance − Σ ooIM(before)` across the user's whole market index, with
            // `pos` overriding storage for this market (it is not written until below). Nothing is
            // debited: resting an order moves no money, it only raises the requirement.
            // A non-positive delta — a pure-reduce order — is free at any balance.
            let mut after = pos.clone();
            after.total_buy_qty = new_tbq;
            after.total_buy_notional = new_tbn;
            let delta = crate::margin_view::derived_requirement_delta(market, &pos, &after)?;
            let available = crate::margin_view::derived_available_balance_with(
                context,
                user,
                Some(wallet),
                Some((market_id, &pos)),
            )?;
            if !crate::margin_view::derived_can_afford(available, delta) {
                return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
            }
            // Commit the maintained buy aggregates (op accepted).
            pos = after;

            // ── APPLY (all rejects passed) ── NOW do the real insert: in-place on a warm list
            // (zero clone), or one materialize-clone on a cold first-touch (unavoidable — it IS
            // the write of a previously-committed list). partition_point re-derives the same idx.
            emit_pending_order_placed(context, pending_placed);
            drop(buy_ref);
            storage::mutate_buy_orders(context, user, market_id, |list| {
                let i = list.partition_point(|e| e.price > price);
                list.insert(i, new_entry);
            })?;
            storage::insert_bid_price(context, market_id, price)?;
            // push_bid_order also bumps the level's live count (folded into the level blob).
            storage::push_bid_order(context, market_id, price, *order_id)?;

            // Keep best_bid cache up to date.
            if best_bid == 0 || price > best_bid {
                storage::save_best_bid(context, market_id, price)?;
                // best_ask from the single resolve above (post-match for GTC).
                record_mid_price_sample_for_best_quote_change(context, market_id, price, best_ask)?;
            }
        }
        Side::Sell => {
            // commit-only #23 CLONE-FREE probe (mirror of the buy arm).
            let sell_ref = storage::load_sell_orders_ref(context, user, market_id)?;
            let new_entry = OrderEntry {
                order_id: *order_id,
                price,
                amount: qty,
                maker_fee_bps,
            };
            let (bd, pd) = (market.base_decimals, market.price_decimals);
            #[cfg(debug_assertions)]
            {
                let buy_entries = storage::load_buy_orders_ref(context, user, market_id)?;
                debug_assert_totals(
                    entries_iter(&buy_entries),
                    entries_iter(&sell_ref),
                    &pos,
                    bd,
                    pd,
                );
            }
            let entry_notional = crate::math::calc_value(price, qty, bd, pd)?;
            let new_tsq = pos
                .total_sell_qty
                .checked_add(qty)
                .ok_or_else(|| perp_err("placeOrder: total sell qty overflow"))?;
            let new_tsn = pos
                .total_sell_notional
                .checked_add(entry_notional)
                .ok_or_else(|| perp_err("placeOrder: total sell notional overflow"))?;

            // ── Derived-ooIM admission gate ── (see the buy arm)
            let mut after = pos.clone();
            after.total_sell_qty = new_tsq;
            after.total_sell_notional = new_tsn;
            let delta = crate::margin_view::derived_requirement_delta(market, &pos, &after)?;
            let available = crate::margin_view::derived_available_balance_with(
                context,
                user,
                Some(wallet),
                Some((market_id, &pos)),
            )?;
            if !crate::margin_view::derived_can_afford(available, delta) {
                return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
            }
            pos = after;

            // ── APPLY (all rejects passed) ── real insert: in-place (warm) / one materialize (cold).
            emit_pending_order_placed(context, pending_placed);
            drop(sell_ref);
            storage::mutate_sell_orders(context, user, market_id, |list| {
                let i = list.partition_point(|e| e.price < price);
                list.insert(i, new_entry);
            })?;
            storage::insert_ask_price(context, market_id, price)?;
            // push_ask_order also bumps the level's live count (folded into the level blob).
            storage::push_ask_order(context, market_id, price, *order_id)?;

            // Keep best_ask cache up to date.
            if best_ask == 0 || price < best_ask {
                storage::save_best_ask(context, market_id, price)?;
                // best_bid from the single resolve above (post-match for GTC).
                record_mid_price_sample_for_best_quote_change(context, market_id, best_bid, price)?;
            }
        }
    }

    // Resting only touches the per-side aggregates, so this skips save_position's old-position
    // re-read + `amount` zero-crossing registry hooks (dead work here) and moves the position in
    // by value instead of cloning it. The account is NOT written: resting moves no money.
    storage::save_position_reservation_only(context, user, market_id, pos)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderRested {
            user,
            marketId: market_id,
            orderId: FixedBytes(*order_id),
            side: side as u8,
            price,
            quantity: qty,
            tif: tif as u8,
            clientOrderId: FixedBytes(client_order_id),
        }
        .to_log_data(),
    });

    Ok(())
}

// ── Cancel helpers ─────────────────────────────────────────────────────────────

/// Atomically executes all four steps of an order cancellation:
/// remove from book → release reserved margin → delete the order record → emit log.
///
/// Both the explicit user-initiated cancel path and the auto-cancel-for-margin
/// path in settlement use this function so the invariant "these steps always
/// happen together" is enforced in one place.
///
/// `remove` is the book-removal step to run first — the caller passes the variant
/// matching its BBO-cache freshness: [`remove_from_book_after_cancel`] from the
/// explicit cancel path (cache live → may skip the BBO refresh), or
/// [`remove_from_book_during_match`] from the settlement auto-cancel paths (cache
/// stale mid-matching → must always refresh).
pub(super) fn execute_order_cancellation<H: PerpHost, F>(
    context: &mut H,
    user: Address,
    market_id: u64,
    order_id: [u8; 32],
    order: Order,
    market: &crate::types::Market,
    remove: F,
) -> Result<(), PerpError>
where
    F: FnOnce(&mut H, u64, Side, u64, &[u8; 32]) -> Result<(), PerpError>,
{
    remove(context, market_id, order.side, order.price, &order_id)?;
    release_margin_for_cancelled_order(context, user, market_id, order.side, &order_id, market)?;
    // delete-on-terminal: the cancelled/expired order is removed from the map. Its id may linger in
    // the level FIFO (lazy-queue) until a match walk sweeps it; `remove` already decremented the
    // level's live count. The Cancelled/Expired distinction (previously only the saved status; the
    // event has always been OrderCancelled) is dropped with the record — history is disposable.
    storage::delete_order(context, &order_id)?;
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderCancelled {
            user,
            orderId: FixedBytes(order_id),
            marketId: market_id,
        }
        .to_log_data(),
    });
    Ok(())
}

/// Detach `order_id` from its price level: drop it from the level's FIFO queue and,
/// if that empties the level, remove the price from the side's price list. Updates
/// the level queue + price list but does NOT touch the best_bid/best_ask cache.
///
/// Returns `(level_emptied, old_best)` where `old_best` is the side's cached best
/// captured BEFORE any mutation (so callers can decide how to refresh it).
fn detach_order_from_level<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    price: u64,
    _order_id: &[u8; 32],
) -> Result<(bool, u64), PerpError> {
    // lazy-queue: `order_id` is intentionally NOT removed from the FIFO queue — that O(depth) scan
    // is replaced by an O(1) decrement of the level's LIVE count. The stale id is swept when the
    // next match walk reaches it (load_order → None → skip). When the count hits 0 the level is
    // logically empty: drop the price from the index and clear the (now all-stale) queue in one
    // shot, so single-order-per-level churn (count 1→0 each cancel) never accumulates stale ids.
    let (old_best, emptied) = match side {
        Side::Buy => {
            let old_best = storage::load_best_bid(context, market_id)?;
            // decr_level_count clears the FIFO ids when the count hits 0 (blob → delete); we only
            // need to drop the price from the index.
            let emptied = storage::decr_level_count(context, market_id, Side::Buy, price, 1)? == 0;
            if emptied {
                storage::remove_bid_price(context, market_id, price)?;
            }
            (old_best, emptied)
        }
        Side::Sell => {
            let old_best = storage::load_best_ask(context, market_id)?;
            let emptied = storage::decr_level_count(context, market_id, Side::Sell, price, 1)? == 0;
            if emptied {
                storage::remove_ask_price(context, market_id, price)?;
            }
            (old_best, emptied)
        }
    };
    Ok((emptied, old_best))
}

/// Recompute the side's best from its (already-mutated) price list and, if it moved
/// off `old_best`, record a mid-price sample. Shared by both removal entry points.
fn refresh_best_and_sample<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    old_best: u64,
) -> Result<(), PerpError> {
    match side {
        Side::Buy => {
            let best_bid = storage::refresh_best_bid(context, market_id)?;
            if best_bid != old_best {
                let best_ask = storage::load_best_ask(context, market_id)?;
                record_mid_price_sample_for_best_quote_change(
                    context, market_id, best_bid, best_ask,
                )?;
            }
        }
        Side::Sell => {
            let best_ask = storage::refresh_best_ask(context, market_id)?;
            if best_ask != old_best {
                let best_bid = storage::load_best_bid(context, market_id)?;
                record_mid_price_sample_for_best_quote_change(
                    context, market_id, best_bid, best_ask,
                )?;
            }
        }
    }
    Ok(())
}

/// Remove an order from the book on the **explicit cancel path**, where the
/// best_bid/best_ask cache is live (no matching ran earlier in this call).
///
/// The best only moves when the TOP level empties, which — with a current cache —
/// is provable from `price` vs the cached best, so an interior removal skips the
/// refresh entirely (price-list reload + recompute + cache re-store + best_*_key
/// commitment membership + mid-price sample are all pure waste there). Orientation
/// is side-aware (bids sort DESC, asks ASC):
///   - Buy:  price == best_bid → refresh; price <  best_bid → skip; price >  best_bid → invariant
///   - Sell: price == best_ask → refresh; price >  best_ask → skip; price <  best_ask → invariant
/// A removal "beyond" the cached best is impossible with a live cache, so it trips
/// an invariant error (guards against a stale cache reaching this path).
pub(super) fn remove_from_book_after_cancel<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    price: u64,
    order_id: &[u8; 32],
) -> Result<(), PerpError> {
    let (emptied, old_best) = detach_order_from_level(context, market_id, side, price, order_id)?;
    if !emptied {
        return Ok(());
    }
    let top_emptied = match side {
        Side::Buy => {
            if price > old_best {
                return Err(perp_invariant_err(format!(
                    "cancel: bid level {price} above cached best_bid {old_best} \
                     (stale BBO cache on the cancel path?)"
                )));
            }
            price == old_best
        }
        Side::Sell => {
            if old_best == 0 || price < old_best {
                return Err(perp_invariant_err(format!(
                    "cancel: ask level {price} below cached best_ask {old_best} \
                     (stale BBO cache on the cancel path?)"
                )));
            }
            price == old_best
        }
    };
    if top_emptied {
        refresh_best_and_sample(context, market_id, side, old_best)?;
    }
    Ok(())
}

/// Remove an order from the book on the **mid-matching auto-cancel path** (maker
/// deficit / taker margin-cover inside match_order's sweep), where the
/// best_bid/best_ask cache is deliberately stale — match_order defers its single
/// refresh to after the sweep. The price-vs-cache test is untrustworthy here, so
/// always recompute the best from the price list to keep the cached value correct.
pub(super) fn remove_from_book_during_match<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    price: u64,
    order_id: &[u8; 32],
) -> Result<(), PerpError> {
    let (emptied, old_best) = detach_order_from_level(context, market_id, side, price, order_id)?;
    if emptied {
        refresh_best_and_sample(context, market_id, side, old_best)?;
    }
    Ok(())
}

/// Detach a cancelled order from the owner's per-market entry list and shrink the maintained
/// per-side aggregates by exactly its contribution.
///
/// Formerly this also RELEASED the order's escrowed margin back to the wallet. There is no escrow:
/// a cancel moves **no money at all**. What it does is lower `Bid` (or `Ask`), which lowers this
/// market's derived `ooIM` and therefore RAISES the account's available balance — the same effect,
/// with nothing changing hands. The function is kept (under its call sites' name) because the
/// "book removal, entry removal and aggregate update always happen together" invariant is still
/// the thing worth enforcing in one place.
pub(super) fn release_margin_for_cancelled_order<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    side: Side,
    order_id: &[u8; 32],
    market: &crate::types::Market,
) -> Result<(), PerpError> {
    let mut pos = storage::load_position(context, user, market_id)?;
    let (bd, pd) = (market.base_decimals, market.price_decimals);

    // #B: the cancelled order's price (O(1) warm load) lets remove_order_entry_by_price binary-search
    // to it (O(log n)) instead of an O(n) id scan over the account's list.
    let price = storage::load_order_ref(context, order_id)?
        .map(|o| o.price)
        .ok_or_else(|| perp_invariant_err("cancel: order missing for price lookup"))?;

    // Real removal FIRST (in-place on a warm list / one materialize on a cold first-touch) — returns
    // the cancelled entry (and reproduces remove_order_entry's not-found invariant verbatim). Only
    // the cancelled side's key is written. Cancel has no reject, so this is a pure apply.
    let cancelled_entry = match side {
        Side::Buy => storage::mutate_buy_orders(context, user, market_id, |list| {
            remove_order_entry_by_price(list, order_id, price, true, "buy")
        })??,
        Side::Sell => storage::mutate_sell_orders(context, user, market_id, |list| {
            remove_order_entry_by_price(list, order_id, price, false, "sell")
        })??,
    };

    // Subtract exactly the per-order-floored term this entry contributed, so the aggregate stays
    // byte-identical to a fresh fold over the remaining list (asserted below in debug).
    let entry_notional =
        crate::math::calc_value(cancelled_entry.price, cancelled_entry.amount, bd, pd)?;
    remove_entry_from_side_aggregates(&mut pos, side, cancelled_entry.amount, entry_notional)?;
    #[cfg(debug_assertions)]
    {
        let buy_ref = storage::load_buy_orders_ref(context, user, market_id)?;
        let sell_ref = storage::load_sell_orders_ref(context, user, market_id)?;
        debug_assert_totals(
            entries_iter(&buy_ref),
            entries_iter(&sell_ref),
            &pos,
            bd,
            pd,
        );
    }
    storage::save_position(context, user, market_id, &pos)?;
    Ok(())
}

/// Shrink one side's `(Σ qty, Σ notional)` aggregate by a departing entry's contribution.
/// An underflow means the aggregate and the list have desynchronised — an invariant break, not a
/// user-facing reject.
fn remove_entry_from_side_aggregates(
    pos: &mut crate::types::PerpPosition,
    side: Side,
    amount: u64,
    notional: u64,
) -> Result<(), PerpError> {
    let (qty_field, notional_field, label) = match side {
        Side::Buy => (&mut pos.total_buy_qty, &mut pos.total_buy_notional, "buy"),
        Side::Sell => (
            &mut pos.total_sell_qty,
            &mut pos.total_sell_notional,
            "sell",
        ),
    };
    *qty_field = qty_field
        .checked_sub(amount)
        .ok_or_else(|| perp_invariant_err(format!("cancel: total {label} qty underflow")))?;
    *notional_field = notional_field
        .checked_sub(notional)
        .ok_or_else(|| perp_invariant_err(format!("cancel: total {label} notional underflow")))?;
    Ok(())
}

/// PURE core of [`release_margin_for_cancelled_order`] (commit-only #23, tranche-4): removes the
/// entry from the working-copy list and shrinks the per-side aggregates — over in-memory copies
/// only, NO storage access. The match compute phase runs this to simulate the taker's LIFO
/// cover-cancels (and plan them) before any write.
///
/// Moves no money (there is no escrow to release); its effect on affordability is entirely via the
/// smaller `Bid`/`Ask` it leaves behind. The book entry's `amount` is the authoritative remaining
/// quantity (kept current by `reduce_order_entry_core`); the order's `filled` can lag it during the
/// same matching round, so the aggregate is shrunk by the entry, not by `order.quantity - filled`.
pub(super) fn release_margin_core(
    pos: &mut crate::types::PerpPosition,
    buy_entries: &mut std::collections::VecDeque<OrderEntry>,
    sell_entries: &mut std::collections::VecDeque<OrderEntry>,
    side: Side,
    order_id: &[u8; 32],
    market: &crate::types::Market,
) -> Result<(), PerpError> {
    let removed = {
        let (entries, label) = match side {
            Side::Buy => (&mut *buy_entries, "buy"),
            Side::Sell => (&mut *sell_entries, "sell"),
        };
        remove_order_entry(entries, order_id, label)?
    };
    let notional = crate::math::calc_value(
        removed.price,
        removed.amount,
        market.base_decimals,
        market.price_decimals,
    )?;
    remove_entry_from_side_aggregates(pos, side, removed.amount, notional)
}

// ── FOK / PostOnly pre-checks ─────────────────────────────────────────────────

/// Check whether the book can fully fill a FOK order.  Returns error if not.
fn check_fok_feasibility<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    limit_price: u64,
    quantity: u64,
    order_type: OrderType,
    market: &crate::types::Market,
) -> Result<(), PerpError> {
    let mut available: u64 = 0;
    // Count only IN-BAND liquidity: match_order will not fill past the band, so FOK
    // feasibility must apply the same bound or it would pass a FOK that then can't fully
    // fill. (mark_price is a field of the Market already loaded.)
    let mark = market.mark_price;
    let (mark_upper, mark_lower) =
        crate::math::mark_band_bounds(mark, market.price_band_bps);
    match side {
        Side::Buy => {
            let ask_prices = storage::load_ask_prices_ref(context, market_id)?;
            'outer: for ask_price in ask_prices.iter().copied() {
                if (ask_price as u128) < mark_lower {
                    continue;
                }
                if order_type == OrderType::Limit && ask_price > limit_price {
                    break;
                }
                if ask_price as u128 > mark_upper {
                    break;
                }
                let queue = storage::load_ask_level_arc(context, market_id, ask_price)?;
                for maker_id in queue.ids.iter() {
                    if let Some(o) = storage::load_order_ref(context, maker_id)? {
                        if matches!(o.status, OrderStatus::Open | OrderStatus::PartiallyFilled) {
                            available += o.quantity - o.filled;
                            if available >= quantity {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
        Side::Sell => {
            let bid_prices = storage::load_bid_prices_ref(context, market_id)?;
            'outer: for bid_price in bid_prices.iter().rev().copied() {
                if (bid_price as u128) > mark_upper {
                    continue;
                }
                if order_type == OrderType::Limit && bid_price < limit_price {
                    break;
                }
                if (bid_price as u128) < mark_lower {
                    break;
                }
                let queue = storage::load_bid_level_arc(context, market_id, bid_price)?;
                for maker_id in queue.ids.iter() {
                    if let Some(o) = storage::load_order_ref(context, maker_id)? {
                        if matches!(o.status, OrderStatus::Open | OrderStatus::PartiallyFilled) {
                            available += o.quantity - o.filled;
                            if available >= quantity {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
    }
    if available < quantity {
        Err(perp_err("placeOrder: FOK order cannot be fully filled"))
    } else {
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
