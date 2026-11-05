//! Trading engine: place/cancel/query orders with on-chain order-book matching.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use crate::host::PerpHost;
use primitives::{keccak256, Address, Bytes, FixedBytes, Log};

pub mod algo;
mod liquidation_close;
pub mod reduce_only;
mod settlement;

pub(crate) use liquidation_close::execute_liquidation_market_order;
pub(crate) use settlement::{
    apply_position_fill, OpeningMarginFunding, CROSS_MAINTENANCE_REJECT, ISOLATED_SILO_CASH_REJECT,
};
use settlement::{MakerFillOutcome, TakerSettlement};

/// Maximum distinct maker accounts a single liquidation close may touch.
///
/// Any unfilled quantity after reaching the cap follows the existing residual/ADL path. This keeps
/// the balance after-image count bounded independently of order-book size.
pub(crate) const MAX_LIQUIDATION_MAKER_ACCOUNTS: usize = 128;

use crate::{
    account::signing::{
        check_api_key_expiry, check_recv_window, gc_seen_buckets_best_effort, put, verify_ed25519,
        write_signed_header,
    },
        batch::{self, PerpBatchTag},
    errors::{perp_err, perp_invariant_err},
    interface::IPerpDex::{
        self, batchCancelOrdersCall, batchCancelOrdersSignedCall, batchPlaceOrdersCall,
        batchPlaceOrdersSignedCall, cancelOrderCall, cancelOrderSignedCall, getBookLevelCall,
        getBookPricesCall, getMarketFeeTotalCall, getOpenOrdersCall, getOpenOrdersReturn,
        getOrderCall, getOrderReturn, placeOrderCall, placeOrderSignedCall, PlaceItem,
    },
    math::calc_value,
    risk::record_mid_price_sample_for_best_quote_change,
    storage,
    types::{
        CancelReason, ExpiryReason, Order, OrderEntry, OrderKind, OrderStatus, OrderType,
        Side, StpMode, TimeInForce,
    },
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
        args.flags,
        PlacementOrigin::Submitted,
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
///
/// The signature is spent by SUBMISSION, not by success: a rejected placement has spent it just the
/// same, so it cannot be resubmitted once conditions change. See the note at the burn site.
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

    // Canonical fixed-layout message. ⚠️ No total is quoted here on purpose: the array literal
    // below IS the length and the closing `debug_assert_eq!` enforces it, so a number in this
    // comment would be a second source of truth that nothing checks. It went stale three times —
    // once per field added — before it was removed.
    //   "perpdex_v1_order" || chainId(8) || account(20) || marketId(8) || side(1)
    //   || price(8) || quantity(8) || orderType(1) || tif(1) || clientOrderId(16)
    //   || timestamp(8) || recvWindow(8) || keyId(1) || flags(1)
    // `flags` is signed for the reason on the ABI: a signature that did not cover it could be
    // resubmitted with the reduce-only bit flipped either way.
    let mut msg = [0u8; 105];
    let mut c = write_signed_header(&mut msg, b"perpdex_v1_order", context.chain_id());
    c = put(&mut msg, c, args.account.as_slice());
    c = put(&mut msg, c, &args.marketId.to_be_bytes());
    c = put(&mut msg, c, &[args.side]);
    c = put(&mut msg, c, &args.price.to_be_bytes());
    c = put(&mut msg, c, &args.quantity.to_be_bytes());
    c = put(&mut msg, c, &[args.orderType, args.tif]);
    c = put(&mut msg, c, &args.clientOrderId.0);
    c = put(&mut msg, c, &args.timestamp.to_be_bytes());
    c = put(&mut msg, c, &args.recvWindow.to_be_bytes());
    c = put(&mut msg, c, &[args.keyId, args.flags]);
    debug_assert_eq!(c, msg.len(), "placeOrderSigned message layout");

    verify_ed25519(&pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(&format!("placeOrderSigned: {e}")))?;

    // orderId = keccak256(signature); the SAME hash keys the seen-signature replay set.
    let order_id: [u8; 32] = keccak256(args.signature.as_ref()).0;
    if storage::is_signature_seen(context, &order_id)? {
        return Err(perp_err(
            "placeOrderSigned: duplicate signature (already submitted)",
        ));
    }

    // ── last pre-write fault has passed; the first write happens here ──
    //
    // # The signature is burned UNCONDITIONALLY, BEFORE the placement runs
    //
    // Same rule as the two signed batch entrypoints. A signed one-shot authorization is consumed
    // by SUBMISSION, not by success — exactly how an Ethereum nonce is spent by a failed
    // transaction. Commit-only (#23) is what makes that implementable: the marker survives the
    // frame revert a rejected placement triggers (`journal/inner.rs`: "perp overlay writes are NOT
    // rolled back by frame reverts"), so the burn outlives the reject it exists to outlive.
    //
    // Burning only on success left a REJECTED signature replayable for the rest of its recv window
    // (≤65s), which is reachable whenever the rejection was TRANSIENT — a FOK that could not be
    // filled, a PostOnly that would have crossed, an insufficient-`available` reject all flip
    // within seconds as the book moves or margin frees. An observer then landed the order at a
    // moment the owner believed it had failed. It also made a relayer's idempotent re-send
    // genuinely re-execute instead of bouncing off this guard.
    //
    // That ordering was inherited, not chosen: before delete-on-terminal the ORDER MAP was the
    // replay witness and a rejected placement wrote no order, so the id stayed free. The side
    // effect was carried over verbatim when the guard moved into its own namespace.
    //
    // ⚠️ Two invariants hold this together, and both are load-bearing:
    //   * the burn stays AFTER `verify_ed25519` — burning before authentication would let anyone
    //     write replay markers from arbitrary bytes;
    //   * the reject below is tagged with the MEASURED burn write count, so the commit-only #23
    //     guard exempts exactly these writes and still fires on anything `place_order_core` wrote
    //     before its own reject. Do not hardcode the count: the GC sweep's write count varies.
    //
    // The GC is best-effort — see `gc_seen_buckets_best_effort`: a fallible write running after a
    // committed burn, so surfacing its `Err` would revert a call whose signature is already spent.
    let block_ts: u64 = context.timestamp();
    let writes_before_burn = context.perp_write_count();
    storage::mark_signature_seen(context, &order_id, args.timestamp)?;
    gc_seen_buckets_best_effort(context, block_ts)?;
    let burn_writes = context.perp_write_count().saturating_sub(writes_before_burn);

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
        args.flags,
        PlacementOrigin::Submitted,
        context,
    )
    .map_err(|e| e.after_retained_writes(burn_writes))?;

    Ok(Bytes::from(placeOrderSignedCall::abi_encode_returns(
        &FixedBytes(order_id),
    )))
}

/// `cancelOrderSigned(address account, bytes32 orderId, uint64 timestamp, bytes signature)`
///
/// Replay protection is the seen-signature set, same as every other signed entrypoint: the
/// signature is spent by SUBMISSION, so a resubmission is refused before `cancel_order_core` runs.
///
/// It used to rely on terminal-status idempotence alone — "an already-cancelled order cannot be
/// cancelled again". That held, but it was the ONE signed path whose safety came from a property
/// of the target rather than from the signature, so it read differently from its four siblings and
/// broke for a different reason than they would. It also left a genuine gap: a cancel signed for
/// an order that does not exist YET is rejected, and the signature stayed live, so the same
/// signature could cancel that order the moment it appeared. Explicit burn closes it and makes all
/// five paths one shape.
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

    // Canonical fixed-layout message; the array literal and the closing assert carry the length.
    //   "perpdex_v1_cancel" || chainId(8) || account(20) || orderId(32) || marketId(8)
    //   || timestamp(8) || recvWindow(8) || keyId(1)
    // marketId is part of the signed message for ABI compatibility but otherwise ignored.
    let mut msg = [0u8; 102];
    let mut c = write_signed_header(&mut msg, b"perpdex_v1_cancel", context.chain_id());
    c = put(&mut msg, c, args.account.as_slice());
    c = put(&mut msg, c, args.orderId.as_slice());
    c = put(&mut msg, c, &args.marketId.to_be_bytes());
    c = put(&mut msg, c, &args.timestamp.to_be_bytes());
    c = put(&mut msg, c, &args.recvWindow.to_be_bytes());
    c = put(&mut msg, c, &[args.keyId]);
    debug_assert_eq!(c, msg.len(), "cancelOrderSigned message layout");

    verify_ed25519(&pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(&format!("cancelOrderSigned: {e}")))?;

    // Burned unconditionally, after verification and before the cancel — see the long note at
    // `run_place_order_signed`'s burn site for why submission (not success) spends a signed
    // authorization, and why the reject carries the MEASURED burn write count.
    //
    // Keyed on `keccak256(signature)` like the others. Note this is NOT the orderId here: the
    // cancel takes its target as an argument rather than deriving it, so the two namespaces are
    // unrelated and the `PFX_SEEN_SIG` prefix keeps them from aliasing.
    let sig_hash: [u8; 32] = keccak256(args.signature.as_ref()).0;
    if storage::is_signature_seen(context, &sig_hash)? {
        return Err(perp_err(
            "cancelOrderSigned: duplicate signature (already submitted)",
        ));
    }

    // ── last pre-write fault has passed; the first write happens here ──
    let block_ts: u64 = context.timestamp();
    let writes_before_burn = context.perp_write_count();
    storage::mark_signature_seen(context, &sig_hash, args.timestamp)?;
    gc_seen_buckets_best_effort(context, block_ts)?;
    let burn_writes = context.perp_write_count().saturating_sub(writes_before_burn);

    cancel_order_core(args.account, args.orderId.0, context)
        .map_err(|e| e.after_retained_writes(burn_writes))
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
        context.chain_id(),
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

/// Canonical batch-cancel digest preimage — `HEADER` below is the header width, and the closing
/// `debug_assert_eq!` ties the total to it, so neither size is restated here:
/// `"perpdex_v1_batch_cancel" || chainId(8) || account(20) || keyId(1) || timestamp(8)
///  || recvWindow(8) || N(4) || N x orderId(32)`, all integers big-endian.
///
/// `N` is the DECODED id count, so a tampered length cannot be made to verify.
pub(crate) fn batch_cancel_message(
    chain_id: u64,
    account: Address,
    key_id: u8,
    timestamp: u64,
    recv_window: u64,
    ids: &[FixedBytes<32>],
) -> Vec<u8> {
    const TAG: &[u8; 23] = b"perpdex_v1_batch_cancel";
    const HEADER: usize = 23 + 8 + 20 + 1 + 8 + 8 + 4; // + chainId(8)
    let mut msg = Vec::with_capacity(HEADER + ids.len() * 32);
    msg.extend_from_slice(TAG);
    msg.extend_from_slice(&chain_id.to_be_bytes());
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
        context.chain_id(),
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
        item.flags,
        PlacementOrigin::Submitted,
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

/// Canonical batch-place digest preimage — `HEADER` and `ITEM` below are the widths, and the
/// closing `debug_assert_eq!` ties the total to them, so neither is restated here:
/// `"perpdex_v1_batch_order" || chainId(8) || account(20) || keyId(1) || timestamp(8)
///  || recvWindow(8) || N(4)
///  || N x [ marketId(8) || side(1) || price(8) || quantity(8) || orderType(1) || tif(1)
///           || clientOrderId(16) || flags(1) ]`, all integers big-endian.
///
/// `N` is the DECODED item count, so neither the length nor any field of any item can be tampered
/// with. The digest packs the items TIGHTLY — it is NOT the ABI encoding, which pads every field
/// to a word (`batch::PLACE_ITEM_ENCODED_LEN`). That contrast is the point of saying so; the two
/// numbers live on `ITEM` and that constant.
pub(crate) fn batch_place_message(
    chain_id: u64,
    account: Address,
    key_id: u8,
    timestamp: u64,
    recv_window: u64,
    orders: &[PlaceItem],
) -> Vec<u8> {
    const TAG: &[u8; 22] = b"perpdex_v1_batch_order";
    const HEADER: usize = 22 + 8 + 20 + 1 + 8 + 8 + 4; // + chainId(8)
    /// marketId(8) + side(1) + price(8) + quantity(8) + orderType(1) + tif(1) + clientOrderId(16)
    /// + flags(1)
    const ITEM: usize = 44;
    let mut msg = Vec::with_capacity(HEADER + orders.len() * ITEM);
    msg.extend_from_slice(TAG);
    msg.extend_from_slice(&chain_id.to_be_bytes());
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
        msg.push(o.flags);
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
            flags: encode_order_flags(order.reduce_only, order.stp),
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
    let mut flags = Vec::with_capacity(total);

    for entry in buy_entries.iter() {
        order_ids.push(FixedBytes(entry.order_id));
        sides.push(Side::Buy as u8);
        prices.push(entry.price);
        remaining_quantities.push(entry.amount);
        // Off the ENTRY (it mirrors both modifiers), so this view never loads an order.
        flags.push(encode_order_flags(entry.reduce_only, entry.stp));
    }
    for entry in sell_entries.iter() {
        order_ids.push(FixedBytes(entry.order_id));
        sides.push(Side::Sell as u8);
        prices.push(entry.price);
        remaining_quantities.push(entry.amount);
        // Off the ENTRY (it mirrors both modifiers), so this view never loads an order.
        flags.push(encode_order_flags(entry.reduce_only, entry.stp));
    }

    Ok(Bytes::from(getOpenOrdersCall::abi_encode_returns(
        &getOpenOrdersReturn {
            orderIds: order_ids,
            sides,
            prices,
            remainingQuantities: remaining_quantities,
            flags,
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

mod order;
pub(crate) use order::*;

mod matching;
use matching::*;

// ── Resting in book ───────────────────────────────────────────────────────────

/// Place a limit order that did not (fully) match into the order book and lock margin.
///
/// The margin model uses `max(buy_reserved, sell_reserved)` so only the dominant side
/// actually locks capital.  Adding an order on the weaker side only increases the wallet
/// deduction when it surpasses the other side's reservation.
/// #A oracle (debug builds only): assert the maintained per-side aggregates on `pos` equal a fresh
/// recompute from the actual order lists. A divergence means a mutation site failed to keep the
/// totals in sync — caught loudly in tests / the correctness gate, compiled out in release.
///
/// The fold is `math::sum_side_totals`, which values each entry at its FROZEN
/// `assuming_price` — the same basis every maintenance site adds and subtracts. Folding at the LIMIT
/// price here would fire spuriously on any sell that was marked up at placement.
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

/// One of PHASE 2's four aggregate adds — the mechanical restatement of a sum PHASE 1 already
/// checked, on the same two integers.
///
/// `perp_invariant_err` and not `perp_err`: this runs BEHIND the write barrier, so reaching it at
/// all is the write-then-error shape commit-only #23 forbids. It is out of reach by construction —
/// PHASE 1 performed the identical `checked_add` on the identical values (the `debug_assert_eq!`
/// against the basis is what holds "identical" to account), so an overflow here means the basis
/// diverged from the flushed state, which is a BUG and not a user's arithmetic.
#[inline]
fn add_decided_aggregate(current: u64, addend: u64, field: &str) -> Result<u64, PerpError> {
    current.checked_add(addend).ok_or_else(|| {
        perp_invariant_err(format!(
            "rest apply: total {field} overflowed a sum the decision phase had already checked"
        ))
    })
}

/// Consuming a Deque-or-slice as an OrderEntry iterator for the reservation calls / oracle.
#[inline]
fn entries_iter(
    v: &std::collections::VecDeque<OrderEntry>,
) -> impl Iterator<Item = OrderEntry> + Clone + '_ {
    v.iter().copied()
}

/// Whether `rest_in_book` actually rested the order, or refused it as a too-good new best.
///
/// Both refusals are decided in PHASE 1, above the write barrier, so both are clean whole-order
/// rejects for every caller ([`raise_rest_refusal`]). Returning them rather than raising them in
/// place is still the right shape: it keeps the two refusal points inside the arms from having to
/// know whether a barrier is pending, and it keeps the funnel single.
///
/// ⚠️ A new variant must be decided in PHASE 1 too. Returning one from PHASE 2 would put a reject
/// behind the barrier and re-open the val0 leak.
#[must_use]
enum RestOutcome {
    Rested,
    RefusedTooGoodBest,
    /// The rest's marginal `ooIM` (plus, for a cross order, its Open Loss) does not fit in the
    /// available balance.
    ///
    /// This used to be raised in place (two `return Err`s, one per side arm) while a crossing GTC's
    /// fills were already flushed, which was the val0 leak at block 1,098,719. It was then made a
    /// remainder EXPIRY, which stopped the leak but broke the order-lifecycle contract (an
    /// `OrderCancelled` with no `OrderRested` before it) and diverged from Binance, which rejects
    /// the whole order (`-2019`). Now the barrier sits below the decision, so it is simply a reject.
    RefusedUnaffordable {
        /// The order's position is CROSSED in force: refused with the `-2019` string.
        cross: bool,
    },
    /// The order can open, its account holds cross exposure, and `E − OpenLoss < Σ_C MM` — J1 at
    /// rest (R2). Raised with the taker J1's string (`CROSS_MAINTENANCE_REJECT`, `-2019`).
    RefusedUnderCrossMaintenance,
}

const TOO_GOOD_BEST_REJECT: &str = "placeOrder: a new best quote must be inside the price band";
/// The rest-margin refusal. Also the reject the pre-walk early-out raises, deliberately: it is the
/// same cause, so a client cannot tell "refused before the walk" from "refused after it".
pub(super) const INSUFFICIENT_MARGIN_REJECT: &str =
    "placeOrder: insufficient perp wallet for margin";
/// [`INSUFFICIENT_MARGIN_REJECT`] for an order on a position CROSSED in force — the same cause, the
/// available balance `A` not covering the order's cost (a cross open's initial margin at the
/// fills gate; `Δ ooIM` plus its Open Loss at the rest gate; the pre-walk bound) — with Binance's
/// code, `-2019`, like the two other cross margin refusals of the place path (J1, J6). An
/// ISOLATED order keeps the uncoded string byte for byte. Batch reason `InsufficientMargin` (29),
/// like its isolated twin.
///
/// ABI CHANGE (cross P2): a new revert string, reachable only on a position CROSSED in force.
pub(crate) const CROSS_INSUFFICIENT_MARGIN_REJECT: &str =
    "placeOrder: -2019 Margin is insufficient. The available balance does not cover the order";

/// The place path's insufficient-margin refusal for an order on a position CROSSED in force
/// (`cross`) or not.
pub(super) fn insufficient_margin_reject(cross: bool) -> &'static str {
    if cross {
        CROSS_INSUFFICIENT_MARGIN_REJECT
    } else {
        INSUFFICIENT_MARGIN_REJECT
    }
}

/// The quantity of a `qty` order on `side` that can only OPEN: what is left once the position it
/// offsets has been closed — net of the same-side orders already resting against it, which are
/// first in line to close it. `0` for a pure reduce. Conservative by construction: an order that
/// shares the closing room with earlier same-side orders is treated as opening as soon as theirs
/// would fill that room.
fn opening_part_of_order(pos: &crate::types::PerpPosition, side: Side, qty: u64) -> u64 {
    let (offset, resting) = match side {
        Side::Buy if pos.amount < 0 => (pos.amount.unsigned_abs(), pos.total_buy_qty),
        Side::Sell if pos.amount > 0 => (pos.amount.unsigned_abs(), pos.total_sell_qty),
        _ => (0, 0),
    };
    qty.saturating_sub(offset.saturating_sub(resting))
}

/// Binance's **Open Loss** (F8) of a resting order on a position CROSSED in force:
/// `q_open × max(0, price − mark)` for a buy, `q_open × max(0, mark − price)` for a sell, with
/// `q_open` the order's OPENING part ([`opening_part_of_order`]) and `mark` the stored mark at
/// placement. The whole Binance order cost is `IM + Open Loss`; the IM half is the order's `ooIM`
/// (a sell's at `max(T, limit) ≥ mark`, so it carries no open loss of its own).
///
/// `0` for a position ISOLATED in force (its maker fill is held to K9 instead), for a pure reduce
/// (Binance gates no reducing order; a reduce booked at a bad price is the close-through-bankruptcy
/// risk every close carries), for an order on the good side of the mark, and at a mark of `0`.
///
/// It is the cost at PLACEMENT. A mark that later moves through a resting order's price turns it
/// into an open loss the account did not pay for — Binance's own behaviour, and not a lever: no
/// user moves the mark. The liquidation sweep (P3) is the backstop for it.
fn cross_rest_open_loss(
    market: &crate::types::Market,
    pos: &crate::types::PerpPosition,
    side: Side,
    price: u64,
    qty: u64,
) -> Result<u64, PerpError> {
    if crate::margin::margin_type::effective_margin_type(pos) != crate::types::MarginType::Crossed {
        return Ok(0);
    }
    let mark = market.mark_price;
    let loss_per_unit = match side {
        Side::Buy => price.saturating_sub(mark),
        Side::Sell => mark.saturating_sub(price),
    };
    if mark == 0 || loss_per_unit == 0 {
        return Ok(0);
    }
    let q_open = opening_part_of_order(pos, side, qty);
    calc_value(loss_per_unit, q_open, market.base_decimals, market.price_decimals)
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
    // Threaded rather than re-read: the `OrderEntry` this builds is what the admission predicate and
    // all three eviction triggers walk, so `reduce_only` has to be ON the entry; `stp` rides along
    // so `getOpenOrders` can echo it off the entry, and `OrderRested` echoes both.
    flags: OrderFlags,
    market: &crate::types::Market,
    // 2b resolve-once: (best_bid, best_ask). `Some` = the caller already read the BBO (PostOnly
    // threads its do-not-cross read — no match ran, so it is still current). `None` = read it here.
    bbo: Option<(u64, u64)>,
    // ── THE DEFERRED MATCH, if any ──
    // `Some` = a crossing GTC whose match is DECIDED BUT UNWRITTEN. PHASE 1 below reads its
    // [`settlement::RestBasis`] instead of the store, decides, and only then does PHASE 1's tail
    // flush it. Dropping it (a refusal) discards the whole match, which is exactly what a clean
    // reject wants — see [`MatchOutcome`]. `None` = PostOnly, where no match ran.
    mut deferred: Option<MatchOutcome>,
    // The place path's buffered `OrderPlaced`. On the deferred path `MatchOutcome::apply` flushes it
    // at the barrier (so it precedes the match's own Trade/PositionChanged rows, unchanged); on the
    // PostOnly path the tail below flushes it, before this order's `OrderRested`. Either way it is
    // emitted only once every refusal has passed.
    pending_placed: &mut Option<PendingOrderPlaced>,
) -> Result<RestOutcome, PerpError> {
    // ══════════════════════════════════════════════════════════════════════════════════════════
    // PHASE 1 — DECIDE. Zero storage writes, here or inside `deferred`.
    // ══════════════════════════════════════════════════════════════════════════════════════════
    let (base_decimals, price_decimals) = (market.base_decimals, market.price_decimals);
    // The position + wallet the rest is admitted against. On the deferred path these are the values
    // the barrier is ABOUT to write (post-fill, post-cover, post-debit) rather than the values the
    // store still holds — that is the whole point of `RestBasis`, and it is why this decision needs
    // no second opinion from the settlement side. `None` = the taker was never touched by the match
    // (or there was no match), so the store already IS the post-flush state.
    let basis = deferred.as_mut().and_then(|d| d.rest_basis.take());
    // Resting escrows nothing, so the account is READ-ONLY on this path: the fee rate for the book
    // entry, and (in the no-basis case) the cross wallet for the derived admission gate. An `_ref`
    // read — no owned clone, and no `save_account` at the end, so nothing marks this user for an
    // `AccountBalanceChanged` snapshot (see `storage::mark_account_snapshot_dirty`: a pure placement
    // publishes nothing, by measurement).
    //
    // The FEE RATE is read from the store on BOTH paths, deliberately: it is not a settlement
    // output. Only `setUserFeeRates` moves it, the flush cannot, so the pre-barrier read is the same
    // integer the post-barrier one would give.
    let maker_fee_ppm = storage::load_account_ref(context, user)?.maker_fee_ppm;
    let (pos_basis, wallet, own_side_cancelled, own_side_list) = match basis {
        Some(b) => (b.pos, b.wallet, b.own_side_cancelled, b.own_side_list),
        None => (
            storage::load_position(context, user, market_id)?,
            storage::load_account_ref(context, user)?.perp_wallet_balance,
            false,
            // A reduce-only order overtakes nothing (see the pass at the end of PHASE 2).
            if flags.reduce_only {
                None
            } else {
                stored_side_list_with_reduce_only(context, user, market_id, side)?
            },
        ),
    };
    // ONE BBO resolve for the band gate. On the deferred path this is the PRE-barrier BBO; see
    // `may_become_best` for why that is sound, and PHASE 2 for the post-barrier re-read the two
    // cache writes use.
    let (best_bid, best_ask) = match bbo {
        Some(b) => b,
        None => {
            let hot = storage::load_market_hot(context, market_id)?;
            (hot.best_bid, hot.best_ask)
        }
    };

    // A quote that BECOMES the best must sit inside the fill-time band. Gating on "becomes the BBO"
    // rather than on "is out of band" is what makes this precise instead of blanket: an out-of-band
    // level is harmful only while it is the best — that is when it poisons the PostOnly cross check
    // (which reads the cached best with no band term) and when it feeds the mid-price sample that
    // flows into the mark median. A level sitting behind an in-band one is invisible to both, so
    // deep passive orders keep resting, which the fill-time band deliberately allows.
    //
    // This is the PLACEMENT half of a pair; the capped sweep in `run_update_index_price` is the
    // DRIFT half (a mark move can strand a book that was in band when placed). Neither subsumes
    // the other.
    //
    // It applies to EVERY resting order, not just PostOnly. A crossing GTC stops at the band and
    // rests its remainder at its own limit price, so `best_bid = 200` with the mark at 100 is
    // reachable without ever touching the PostOnly path — and a best_bid that high rejects nearly
    // every PostOnly sell. Gating PostOnly alone would leave that half open.
    //
    // ── Why the gate tests `may_become_best` and PHASE 2 recomputes `becomes_best` ──────────────
    //
    // The gate is a DECISION and must therefore be taken pre-barrier, off the BBO the store holds
    // now. The two cache writes it guards are WRITES and must therefore be taken post-barrier, off
    // the BBO the flush leaves. Those two BBOs can differ, so the flag cannot be shared any more —
    // what is shared instead is an implication: `becomes_best(post) ⟹ may_become_best(pre)`, so the
    // band is never skipped for an order that really does become the best.
    //
    // The pre-BBO is exact on our OWN side except for one narrow case:
    // * the match consumes the OPPOSITE side only, so it cannot move our side's best at all;
    // * `finalize_apply`'s cover loop, and the flush's reduce-only pass, cancel the taker's OWN
    //   same-side orders, which CAN lower a bid / raise an ask and hand the best to this very
    //   order.
    //
    // Hence the `own_side_cancelled` disjunct: when the barrier will really cancel one, treat the
    // order as possibly-best and band-check it. That over-checks (never under-checks) in a corner
    // that is already all but unreachable — a crossing order's remainder sits at or past the touch
    // it just crossed, so it is the new best on its own side anyway.
    let may_become_best = own_side_cancelled
        || match side {
            Side::Buy => best_bid == 0 || price > best_bid,
            Side::Sell => best_ask == 0 || price < best_ask,
        };
    if may_become_best {
        // ONE-SIDED, matching the sweep's scoping and the measured exchange behaviour. The harms are
        // asymmetric: a new best that is too GOOD to be true — an ask below `mark_lower`, a bid above
        // `mark_upper` — both mis-rejects the opposite side's PostOnly orders (the cross check rejects
        // on `best_ask <= price` / `best_bid >= price`, so a too-low ask or too-high bid rejects MORE)
        // and pollutes the mid-price sample. A new best on the FAR side only pollutes the sample, and
        // it is the ordinary "thin book, deep quote" case: a live probe on the reference exchange
        // placed a sell at 1.389x mark and it was accepted, so rejecting it here would diverge for no
        // gain and would break the deep passive orders the fill-time band deliberately allows.
        let (mark_upper, mark_lower) =
            crate::math::mark_band_bounds(market.mark_price, market.price_band_bps);
        let p = price as u128;
        let too_good = match side {
            Side::Sell => p < mark_lower,
            Side::Buy => p > mark_upper,
        };
        if too_good {
            // Write-clean: nothing above this point mutates, and `deferred` is dropped unflushed on
            // the way out. A clean whole-order reject.
            return Ok(RestOutcome::RefusedTooGoodBest);
        }
    }

    // ── THE FREEZE ── The one place a SHORT order's `T` is resolved, ever, and it happens HERE:
    // pre-barrier, i.e. before `save_last_traded_price`. A SHORT order's Assuming Price is
    // `max(T, limit)` with `T = max(ROUND_UP(lastTraded × 1.0015), mark)`, and it is stored on the
    // entry: from here on the order's margin term is `calc_value(assuming_price, amount)` and no
    // later read re-derives it (R12 — 90 frames, the reported value never moved; `H_live` refused by
    // 1939 quanta).
    //
    // ⚠️ THIS IS THE VAL0 BUG'S ROOT, AND ITS FIX. There used to be a second resolution of `T` (the
    // pre-check in `finalize_compute`) with `save_last_traded_price` running BETWEEN the two. A
    // crossing SELL always drags `last_traded` to at least its own limit price, so the second read
    // was always ≥ the first and the pre-check was systematically optimistic — it vouched for a rest
    // the real check then refused, after the fills had committed. With the write deferred past this
    // point there is exactly ONE evaluation, so the divergence is not merely fixed, it has nowhere
    // left to live. Do not add a second call site.
    //
    // Binance additionally ESCROWS the uplift (measured, R10) and releases it at fill, which is what
    // funds a flipping fill's new leg. We hold no escrow bucket at all, so for us the uplift
    // manifests purely as a stricter admission — which still leaves more wallet present at fill
    // time. It is NOT cosmetic: without it the `ooIM = 0` band (`Ask ≈ 2|N|`, a sell that would flip
    // the position) is free AND fillable here, while on Binance the two are mutually exclusive (a
    // sell must sit near the touch to fill, and near the touch the markup bites). See
    // `binance-flip-and-admission.md` §1.6c/§3.4.
    //
    // ⚠️ `assuming_price` is a MARGIN BASIS ONLY. The insert position, the book level, the fill price
    // and the fee all key on `price`.
    let assuming_price = match side {
        // A LONG order's Assuming Price IS its own limit price — MEASURED, no markup on the buy side
        // (R11's `bidNotional == q_B × P_b` digit-for-digit). Setting the field rather than leaving
        // it side-conditional is what lets ONE rule (`Σ calc_value(assuming_price, amount)`) serve
        // both aggregates.
        Side::Buy => price,
        Side::Sell => price.max(crate::margin::view::assuming_price_floor(
            context, market_id, market,
        )?),
    };
    let new_entry = OrderEntry {
        order_id: *order_id,
        price,
        amount: qty,
        maker_fee_ppm,
        assuming_price,
        reduce_only: flags.reduce_only,
        stp: flags.stp,
    };
    // `Bid`/`Ask` grow by this order's notional at its FROZEN Assuming Price — the same
    // per-order-floored `calc_value` term the raw-list fold would contribute, so the aggregates stay
    // exactly Binance's `bidNotional`/`askNotional`.
    let entry_notional = new_entry.margin_notional(base_decimals, price_decimals)?;

    // ── Derived-ooIM admission gate (POST-STATE form, lazy delta) ──
    //
    // commit-only #23 CLONE-FREE probe: the hypothetical "own-side list ⊕ new_entry" is never
    // materialised — the derived requirement reads only the per-side AGGREGATES, so the whole probe
    // is two `checked_add`s. Nothing is debited either: resting an order moves no money, it only
    // raises the requirement. The OTHER side still matters, through the `max()` inside `ooIM`.
    //
    // ONE arm for both sides. The buy and sell paths used to be ~165 lines of verbatim duplication
    // apiece — same probe, same gate, same four-case table, same ⚠️ notes — differing only in WHICH
    // aggregate pair moves and in the Assuming Price resolved above. An asymmetry between them would
    // have been silent (most tests drive one side), so the duplication is gone rather than mirrored.
    //
    // # Mapping this onto the doc's predicate — one algebraic step, and the step is MEASURED
    //
    // The measured admission predicate is the DELTA form,
    // 「接受 ⟺ 该单带来的 IM 增量 ≤ availableBalance」, i.e. `Δ ooIM ≤ available(before)`. What is
    // evaluated below is `available(after) ≥ 0`. R13 measured the identity that connects them —
    // `availableBalance + totalOpenOrderInitialMargin == totalCrossWalletBalance`, 73/73
    // observations, residual `0E-8`, while BOTH terms moved (`misc/binance-flip-and-admission.md`
    // §3.14) — so `available = wallet − Σ_m ooIM_m` and
    //
    //     available(after) = wallet − (Σ_{m ≠ this} ooIM_m + ooIM_this(after))
    //                      = available(before) − Δ ooIM
    //     ⇒   Δ ooIM ≤ available(before)   ⟺   available(after) ≥ 0
    //
    // That identity is exact here, not approximate: `ooIM_m` is a pure function of market `m`'s own
    // `(N_m, Bid_m, Ask_m, L_m)` (`margin_view::position_derived_margin` → `math::open_order_margin`,
    // which takes nothing else), so every `m ≠ this` term is literally the same integer in both sums
    // within one call — there is NO cross-market coupling to make the cancellation lossy. Both sides
    // also price THIS market at the same mark: the threaded `market` and the one the Σ walk re-reads
    // are the same blob, since the place path never writes `Market`.
    //
    // # The case table this is equivalent over
    //
    //   Δ ooIM │ available(after) │ delta form (was)              │ this form
    //   ───────┼──────────────────┼───────────────────────────────┼────────────────────────
    //     ≤ 0  │      ≥ 0         │ accept (`Δ ≤ 0` escape)       │ accept (1st branch)
    //     ≤ 0  │      < 0         │ accept (`Δ ≤ 0` escape)       │ accept (escape kept)
    //     > 0  │      ≥ 0         │ accept (`avail(before) ≥ Δ`)  │ accept (1st branch)
    //     > 0  │      < 0         │ REJECT                        │ REJECT
    //
    // # ⚠️ The `Δ ≤ 0` escape is the B1 INVARIANT — do not delete it
    //
    // Rows 1 and 3 are the whole reason the delta is LAZY: they are the overwhelming majority and
    // they need only `available(after)`. Row 2 is the ONLY row that needs the delta, and only its
    // SIGN. It is **reachable and it matters**: a user rests affordable orders, the mark then moves
    // against them so `Σ ooIM > wallet` and `available < 0` with no action of theirs (R8: an
    // identically-priced probe accepted, then refused 4 seconds later after the mark fell
    // `4.42 USD`), and they now want to rest a RISK-REDUCING order. Its `Δ ooIM ≤ 0`, but
    // `available(after)` is still negative. Dropping the escape would refuse precisely the order
    // that de-risks them and send them to liquidation instead. This is `derived_can_afford`'s "a
    // non-positive requirement is always affordable", restated at the one site that still has to ask.
    //
    // (At THIS site `Δ ooIM` is never negative unless the rest OVERTAKES reduce-only orders —
    // `max(|N + Bid|, |N − Ask|)` is non-decreasing in each of `Bid`/`Ask` at fixed `N`, `PIM` does
    // not move, and `round_up_div`/`saturating_sub` preserve that. An order that evicts reduce-only
    // orders lowers its own side by their terms, and can come out negative: then the escape is
    // exactly B1 — the placement as a whole lowers the requirement.)
    //
    // Cost: in the accepting case this market's `ooIM` is evaluated ONCE (inside the walk, via the
    // override) instead of three times.
    //
    // # Why the LEAN fold — `Σ ooIM` alone, not the ten-scalar roll-up
    //
    // The gate needs one number, `available(after)`, and that is all this walk produces. A previous
    // pass widened it to `margin_view::index_account_scalars` so the placement could publish an
    // `AccountBalanceChanged` off the same walk; that event is gone (a pure placement is deliberately
    // silent — R14 measurement + the official trigger sentence, see
    // `storage::mark_account_snapshot_dirty`), so the widening has no consumer again and the five
    // accumulators it folded would be discarded arithmetic on the hottest path.
    //
    // # The orders this rest OVERTAKES — measured without them
    //
    // The third reduce-only eviction trigger (the pass at the end of PHASE 2): a NORMAL order
    // resting nearer the touch grows the "ahead" term of the owner's reduce-only orders behind it,
    // and the pass evicts as many as the prefix condition needs. It runs AFTER this decision, so
    // the decision replays it here — on the post-barrier list with the new entry inserted where
    // PHASE 2 inserts it — and the gate is measured without the orders it evicts. On Binance a
    // reduce-only order carries no open-order margin and is expired once it can no longer reduce,
    // so the order whose rest kills one is not liable for it. Measured with them, the gate was
    // too strict: short 5 with a reduce-only buy 5 @ $90, a normal buy 16 @ $100 resting ahead of
    // it was priced at `Bid = $2,050` (`ooIM = $1,050` at leverage 1) where `$1,600` is what rests
    // (`ooIM = $600`).
    //
    // `pos_pre` is the basis without them; `None` = nothing evicted (the common case — no
    // reduce-only order on this side, so no list was even cloned). The order's OWN terms (its Open
    // Loss, its opening part) are measured against it; the delta runs from the true before
    // (`pos_basis`) to the true after (`pos_after`).
    let pos_pre = match own_side_list.filter(|_| !flags.reduce_only) {
        Some(mut list) => {
            let i = match side {
                Side::Buy => list.partition_point(|e| e.price > price),
                Side::Sell => list.partition_point(|e| e.price < price),
            };
            list.insert(i, new_entry);
            let mut pre = pos_basis.clone();
            let mut other = std::collections::VecDeque::new();
            let (buy, sell) = match side {
                Side::Buy => (&mut list, &mut other),
                Side::Sell => (&mut other, &mut list),
            };
            replay_reduce_only_restore(&mut pre, buy, sell, side, market)?.then_some(pre)
        }
        None => None,
    };
    let pre = pos_pre.as_ref().unwrap_or(&pos_basis);
    let mut pos_after = pre.clone();
    {
        let (qty_field, notional_field, label) = match side {
            Side::Buy => (
                &mut pos_after.total_buy_qty,
                &mut pos_after.total_buy_notional,
                "buy",
            ),
            Side::Sell => (
                &mut pos_after.total_sell_qty,
                &mut pos_after.total_sell_notional,
                "sell",
            ),
        };
        *qty_field = qty_field
            .checked_add(qty)
            .ok_or_else(|| perp_err(format!("placeOrder: total {label} qty overflow")))?;
        *notional_field = notional_field
            .checked_add(entry_notional)
            .ok_or_else(|| perp_err(format!("placeOrder: total {label} notional overflow")))?;
    }
    // ── A CROSS order's Open Loss (Binance F8: `Cost = IM + Open Loss`) ──
    //
    // `0` for an order on an ISOLATED position (its maker fill is held to K9 on the trial silo,
    // which refuses an open into insolvency) and for the part of an order that only reduces. For
    // the OPENING part of an order on a position CROSSED in force it is what the fill will book
    // against the account's equity the moment it happens, priced against the mark: nothing checks a
    // cross maker fill (owner decision Q9 / D13 — and K9 does not apply to a margin-free position),
    // so it must be paid for HERE, at placement, or a sell resting below the mark (a buy above it)
    // is admitted on its initial margin alone and fills into an open loss the account never had —
    // the counterparty, possibly the user's own sub-account, collecting it as profit. See
    // [`cross_rest_open_loss`].
    let open_loss = cross_rest_open_loss(market, pre, side, price, qty)?;
    // `pos_after` overrides storage for this market: it is not written until PHASE 2. ONE fold:
    // its `A` is the gate below (`derived_available_balance_with` is exactly this fold's `a`), its
    // `E` / `Σ_C MM` the maintenance check after it (R2) — no load beyond the walk the gate made.
    let fold = crate::margin::view::cross_fold_checked(
        context,
        user,
        Some(wallet),
        Some((market_id, &pos_after)),
        None,
    )?;
    let available_after = fold.a;
    // `A_after − OpenLoss ≥ 0`, i.e. `Δ ooIM + OpenLoss ≤ A(before)` — the measured delta form with
    // the order's whole Binance cost. The B1 escape is unchanged: an order whose cost is not
    // positive (`Δ ooIM ≤ 0` and no open loss) rests at any balance.
    if available_after < open_loss as i128 {
        let delta = crate::margin::view::derived_requirement_delta(market, &pos_basis, &pos_after)?;
        if delta > 0 || open_loss > 0 {
            // Write-clean (validate-then-apply: the probe above materialised nothing, and
            // `deferred` is dropped unflushed). A clean whole-order reject.
            let cross = crate::margin::margin_type::effective_margin_type(&pos_basis)
                == crate::types::MarginType::Crossed;
            return Ok(RestOutcome::RefusedUnaffordable { cross });
        }
    }
    // ── R2: J1 AT REST — an order that can open must find the account at cross maintenance ──────
    //
    // The gate above is `A`'s, so an account that already sits UNDER cross maintenance while
    // `A ≥ 0` (a leg whose `MM > PIM` after tier drift) could still rest an opening order and —
    // no maker fill being checked (Q9) — have it filled, which the taker's J1 refuses. So the
    // resting order is held to the liquidation predicate itself, on the same fold:
    // `E − OpenLoss ≥ Σ_C MM`. Resting moves neither `E` nor `Σ_C MM` (only `ooIM`), so these are
    // the post-rest values; the Open Loss is what the fill would book against `E` at the mark.
    //
    // WHICH orders: the ones that can OPEN (`opening_part_of_order > 0`) of an account holding
    // cross exposure (`has_cross`) — a cross order, and an isolated one too (its fill would move
    // silo cash out of the cross wallet). Never a reduce-only order (it cannot open, whatever the
    // conservative opening part says about the orders ahead of it — a fired stop included) nor a
    // pure reduce, which keep B1. An account with no cross position never reaches it, so every
    // isolated outcome is what it was.
    if fold.has_cross
        && !flags.reduce_only
        && opening_part_of_order(pre, side, qty) > 0
        && fold.e - (open_loss as i128) < fold.cross_mm
    {
        // Write-clean, like the refusals above.
        return Ok(RestOutcome::RefusedUnderCrossMaintenance);
    }

    // ══════════════════════════════════════════════════════════════════════════════════════════
    // ── THE WRITE BARRIER ── every refusal has passed; from here the placement is ACCEPTED.
    // ══════════════════════════════════════════════════════════════════════════════════════════
    let flushed = deferred.is_some();
    if let Some(d) = deferred {
        d.apply(context, side, market, pending_placed)?;
    }

    // ══════════════════════════════════════════════════════════════════════════════════════════
    // PHASE 2 — APPLY. Mechanical: it writes what PHASE 1 decided and decides nothing itself.
    // ══════════════════════════════════════════════════════════════════════════════════════════

    // Post-barrier BBO for the two CACHE writes only. Re-resolved when something was flushed,
    // because the match consumed the opposite side and a cover cancel may have moved our own.
    let (best_bid, best_ask) = if flushed {
        let hot = storage::load_market_hot(context, market_id)?;
        (hot.best_bid, hot.best_ask)
    } else {
        (best_bid, best_ask)
    };
    let becomes_best = match side {
        Side::Buy => best_bid == 0 || price > best_bid,
        Side::Sell => best_ask == 0 || price < best_ask,
    };
    debug_assert!(
        !becomes_best || may_become_best,
        "an order that BECAME the best was not band-checked — `may_become_best` must be a \
         superset of the post-barrier `becomes_best` (see the note on the band gate)"
    );

    // The store is now the state PHASE 1 measured, so the aggregates are re-derived from it rather
    // than carried over from `pos_after`: the add is the same two `checked_add`s on the same two
    // integers, and doing it here means PHASE 2 can never clobber a field the flush wrote.
    let mut pos = storage::load_position(context, user, market_id)?;
    debug_assert_eq!(
        (
            pos.total_buy_qty,
            pos.total_buy_notional,
            pos.total_sell_qty,
            pos.total_sell_notional
        ),
        (
            pos_basis.total_buy_qty,
            pos_basis.total_buy_notional,
            pos_basis.total_sell_qty,
            pos_basis.total_sell_notional
        ),
        "the rest basis diverged from what the flush actually wrote — the PHASE 1 decision was \
         taken on the wrong state (`settlement::RestBasis`)"
    );
    #[cfg(debug_assertions)]
    {
        // The aggregates ARE `Bid`/`Ask` and feed the admission gate directly, so keep the raw-list
        // fold as a live oracle on them. Both lists are read POST-barrier, against the POST-barrier
        // position, so a flush that moved one without the other fails here. (Debug-only: the two
        // list loads exist for this check alone.)
        let buy_entries = storage::load_buy_orders_ref(context, user, market_id)?;
        let sell_entries = storage::load_sell_orders_ref(context, user, market_id)?;
        debug_assert_totals(
            entries_iter(&buy_entries),
            entries_iter(&sell_entries),
            &pos,
            base_decimals,
            price_decimals,
        );
    }
    match side {
        Side::Buy => {
            pos.total_buy_qty = add_decided_aggregate(pos.total_buy_qty, qty, "buy qty")?;
            pos.total_buy_notional =
                add_decided_aggregate(pos.total_buy_notional, entry_notional, "buy notional")?;
        }
        Side::Sell => {
            pos.total_sell_qty = add_decided_aggregate(pos.total_sell_qty, qty, "sell qty")?;
            pos.total_sell_notional =
                add_decided_aggregate(pos.total_sell_notional, entry_notional, "sell notional")?;
        }
    }

    // The real insert: in-place on a warm list (zero clone), or one materialize-clone on a cold
    // first-touch (unavoidable — it IS the write of a previously-committed list). `partition_point`
    // re-derives the same index the probe would have.
    match side {
        Side::Buy => {
            storage::mutate_buy_orders(context, user, market_id, |list| {
                let i = list.partition_point(|e| e.price > price);
                list.insert(i, new_entry);
            })?;
            storage::insert_bid_price(context, market_id, price)?;
            // push_bid_order also bumps the level's live count (folded into the level blob).
            storage::push_bid_order(context, market_id, price, *order_id)?;

            // Keep best_bid cache up to date.
            if becomes_best {
                storage::save_best_bid(context, market_id, price)?;
                // best_ask from the POST-barrier resolve above — the match consumed that side.
                record_mid_price_sample_for_best_quote_change(context, market_id, price, best_ask)?;
            }
        }
        Side::Sell => {
            storage::mutate_sell_orders(context, user, market_id, |list| {
                let i = list.partition_point(|e| e.price < price);
                list.insert(i, new_entry);
            })?;
            storage::insert_ask_price(context, market_id, price)?;
            // push_ask_order also bumps the level's live count (folded into the level blob).
            storage::push_ask_order(context, market_id, price, *order_id)?;

            // Keep best_ask cache up to date.
            if becomes_best {
                storage::save_best_ask(context, market_id, price)?;
                // best_bid from the POST-barrier resolve above — the match consumed that side.
                record_mid_price_sample_for_best_quote_change(context, market_id, best_bid, price)?;
            }
        }
    }

    // Resting only touches the per-side aggregates, so this skips save_position's old-position
    // re-read + `amount` zero-crossing registry hooks (dead work here) and writes just the four
    // aggregates onto the stored position, in place.
    //
    // It is ALSO what keeps a pure placement silent on the `@account` stream: an aggregates-only
    // write does not mark the user for an `AccountBalanceChanged` snapshot, while `save_position`
    // would. Resting moves no money and moves no position state — the two things Binance's
    // `ACCOUNT_UPDATE` reports — so it publishes nothing. See the trigger table and the R14 /
    // official-doc citation on `storage::mark_account_snapshot_dirty`.
    storage::save_position_reservation_only(
        context,
        user,
        market_id,
        storage::SideAggregates::of(&pos),
    )?;

    emit_pending_order_placed(context, pending_placed);
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
            // The FROZEN margin basis, straight off the entry this call just inserted — the same
            // integer `new_entry.margin_notional` was folded into `total_buy/sell_notional` with,
            // so `quantity × assumingPrice` reproduces this order's contribution to
            // `getPositionRisk`'s `bidNotional`/`askNotional` with no re-derivation. It is
            // published because it is un-derivable off-chain: for a sell it is `max(T, limit)` with
            // `T` resolved ONCE, above, and never again (see THE FREEZE).
            assumingPrice: assuming_price,
            flags: flags.encode(),
        }
        .to_log_data(),
    });

    // ── reduce-only: this order may have OVERTAKEN the owner's reduce-only orders ────────────
    //
    // The third way the prefix condition breaks, and the one that is not about the position at all:
    // a NORMAL order nearer the touch fills FIRST, so it grows the "ahead" term of every
    // reduce-only order behind it. Nothing refuses the normal order — it carries no reduce-only
    // predicate of its own, and §1.6's R20 measured Binance accepting it and killing the
    // reduce-only order instead. Normal orders are first-class; reduce-only ones are not.
    //
    // Skipped for a reduce-only order resting: `admit` already priced it against this same
    // condition, so re-running the pass here could only evict the order that just passed.
    //
    // The whole cost when the owner holds no reduce-only order on this side — the overwhelmingly
    // common case — is one scan of the list this call already had warm. The heavy part (loading the
    // position, folding the prefix) is behind that check.
    //
    // PHASE 1 replayed this pass (`pos_pre`) and decided on its outcome; this is the real one, run
    // whenever it could evict — not only when the replay did — so the prefix condition never
    // depends on the replay's plumbing. The assertion below holds the two to each other.
    if !flags.reduce_only {
        crate::trading::reduce_only::restore(
            context,
            user,
            market,
            side,
            crate::types::CancelReason::ReduceOnlyOvertaken,
        )?;
    }
    #[cfg(debug_assertions)]
    {
        // PHASE 2 wrote what PHASE 1 decided on: the rest's own terms, less exactly the reduce-only
        // orders the replay evicted.
        let settled = storage::load_position_ref(context, user, market_id)?;
        debug_assert_eq!(
            (
                settled.total_buy_qty,
                settled.total_buy_notional,
                settled.total_sell_qty,
                settled.total_sell_notional
            ),
            (
                pos_after.total_buy_qty,
                pos_after.total_buy_notional,
                pos_after.total_sell_qty,
                pos_after.total_sell_notional
            ),
            "the overtake pass evicted other orders than PHASE 1 replayed — the rest was admitted \
             on the wrong state (`replay_reduce_only_restore`)"
        );
    }

    Ok(RestOutcome::Rested)
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
///
/// `reason` is the attribution stamped on the `OrderCancelled` log. It is a required argument, not
/// a default, precisely because the two existing callers here are a USER cancel and a mid-match
/// protocol kill and the event used to be indistinguishable between them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_order_cancellation<H: PerpHost, F>(
    context: &mut H,
    user: Address,
    market_id: u64,
    order_id: [u8; 32],
    order: Order,
    market: &crate::types::Market,
    remove: F,
    reason: CancelReason,
) -> Result<(), PerpError>
where
    F: FnOnce(&mut H, u64, Side, u64, &[u8; 32]) -> Result<(), PerpError>,
{
    remove(context, market_id, order.side, order.price, &order_id)?;
    release_open_order_margin(context, user, market_id, order.side, &order_id, market)?;
    // delete-on-terminal: the cancelled/expired order is removed from the map. Its id may linger in
    // the level FIFO (lazy-queue) until a match walk sweeps it; `remove` already decremented the
    // level's live count. The Cancelled/Expired distinction (previously only the saved status; the
    // event has always been OrderCancelled) is dropped with the record — history is disposable, and
    // `reason` below now carries the part of it a client actually needs.
    storage::delete_order(context, &order_id)?;
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderCancelled {
            user,
            orderId: FixedBytes(order_id),
            marketId: market_id,
            reason: reason as u8,
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
pub(crate) fn remove_from_book_after_cancel<H: PerpHost>(
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
/// ⚠️ The name is one level above the mechanics, deliberately: what this function TOUCHES is
/// `Bid`/`Ask` (`total_buy_notional` / `total_sell_notional` and their qty siblings). `ooIM` is
/// DERIVED from those downstream and is never stored, so nothing here reads or writes it — the
/// open-order margin falls as a *consequence*.
///
/// And "release" does not mean money moves. It never does here: there is no escrow, so a cancel
/// moves **no money at all** (the placement side has no `reserve_*` counterpart either — it is two
/// inline `checked_add`s). Lowering `Bid`/`Ask` lowers this market's derived `ooIM` and therefore
/// RAISES the account's available balance — the same effect a refund would have, with nothing
/// changing hands. Do not reintroduce a wallet credit here.
///
/// The function exists as one place because the invariant worth enforcing is that book removal,
/// entry removal and the aggregate update **always happen together**.
pub(super) fn release_open_order_margin<H: PerpHost>(
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
    // the cancelled entry (or raises the not-found invariant). Only
    // the cancelled side's key is written. Cancel has no reject, so this is a pure apply.
    let cancelled_entry = match side {
        Side::Buy => storage::mutate_buy_orders(context, user, market_id, |list| {
            remove_order_entry_by_price(list, order_id, price, true, "buy")
        })??,
        Side::Sell => storage::mutate_sell_orders(context, user, market_id, |list| {
            remove_order_entry_by_price(list, order_id, price, false, "sell")
        })??,
    };

    // Subtract exactly the per-order-floored term this entry contributed — at its FROZEN
    // `assuming_price`, the same basis placement added, so the aggregate stays byte-identical to a
    // fresh fold over the remaining list (asserted below in debug). Using the limit price here would
    // leave a marked-up sell's uplift stranded in the aggregate forever.
    let entry_notional = cancelled_entry.margin_notional(bd, pd)?;
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
    // Aggregates-only write (`pos.amount` is untouched here — the whole function only shrinks
    // `Bid`/`Ask`), so this takes the reservation-only route rather than `save_position`. Two
    // consequences, both wanted: `save_position`'s `amount` zero-crossing hooks are skipped as dead
    // work, and — the reason it matters here — a pure CANCEL does not mark the user for an
    // `AccountBalanceChanged` snapshot. A cancel moves no money and no position state, which is
    // exactly Binance's measured behaviour ("Unfilled orders or cancelled orders will not make the
    // event `ACCOUNT_UPDATE` pushed"); see `storage::mark_account_snapshot_dirty`.
    storage::save_position_reservation_only(
        context,
        user,
        market_id,
        storage::SideAggregates::of(&pos),
    )?;
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

/// PURE core of [`release_open_order_margin`] (commit-only #23, tranche-4): removes the
/// entry from the working-copy list and shrinks the per-side aggregates — over in-memory copies
/// only, NO storage access. The match compute phase runs this to simulate the taker's LIFO
/// cover-cancels (and plan them) before any write.
///
/// Moves no money (there is no escrow to release); its effect on affordability is entirely via the
/// smaller `Bid`/`Ask` it leaves behind. The book entry's `amount` is the authoritative remaining
/// quantity (kept current by `reduce_order_entry_core`); the order's `filled` can lag it during the
/// same matching round, so the aggregate is shrunk by the entry, not by `order.quantity - filled`.
///
/// `price` is the order's LIMIT price (the list's sort key — never the frozen `assuming_price`). Every
/// caller has it in hand (the cover loop reads it off the entry it picked, an in-match maker cancel
/// off the loaded maker `Order`), so the entry is found by binary search to its price run, O(log n),
/// rather than by an O(n) id scan of the account's whole list.
pub(super) fn release_open_order_margin_core(
    pos: &mut crate::types::PerpPosition,
    buy_entries: &mut std::collections::VecDeque<OrderEntry>,
    sell_entries: &mut std::collections::VecDeque<OrderEntry>,
    side: Side,
    order_id: &[u8; 32],
    price: u64,
    market: &crate::types::Market,
) -> Result<(), PerpError> {
    let removed = match side {
        Side::Buy => remove_order_entry_by_price(buy_entries, order_id, price, true, "buy")?,
        Side::Sell => remove_order_entry_by_price(sell_entries, order_id, price, false, "sell")?,
    };
    let notional = removed.margin_notional(market.base_decimals, market.price_decimals)?;
    remove_entry_from_side_aggregates(pos, side, removed.amount, notional)
}

/// [`crate::trading::reduce_only::restore`] on `side`, replayed on in-memory copies: evicts from
/// the copies
/// exactly the entries the real pass would cancel against `pos`, shrinking `pos`'s aggregates by
/// each, and returns whether it evicted anything. NO storage access.
///
/// It is how an admission decision sees the state its own acceptance leaves. The real pass runs
/// AFTER the decision (behind the write barrier), and every order it evicts there is an order that
/// can no longer reduce — on Binance such an order carries no open-order margin and is expired, so
/// an order whose acceptance kills it must not be charged for it. Callers:
/// * `settlement::post_flush_snapshot` — the match flush's pass on the taker
///   (`ReduceOnlyPositionShrank` / `ReduceOnlyWouldOpen`), replayed before the fills gate, the
///   cover simulation and the [`settlement::RestBasis`] are measured;
/// * [`rest_in_book`] — the overtake pass a NORMAL order's rest triggers (`ReduceOnlyOvertaken`).
///
/// The decision is the real pass's own ([`crate::trading::reduce_only::next_eviction`]) and the
/// removal is
/// the cover simulation's ([`release_open_order_margin_core`]), so neither can drift from what the
/// barrier then writes; the debug assertions after the barrier hold the two to account.
pub(super) fn replay_reduce_only_restore(
    pos: &mut crate::types::PerpPosition,
    buy_entries: &mut std::collections::VecDeque<OrderEntry>,
    sell_entries: &mut std::collections::VecDeque<OrderEntry>,
    side: Side,
    market: &crate::types::Market,
) -> Result<bool, PerpError> {
    let mut evicted = false;
    loop {
        let list = match side {
            Side::Buy => &*buy_entries,
            Side::Sell => &*sell_entries,
        };
        let Some(eviction) = crate::trading::reduce_only::next_eviction(pos, list, side) else {
            return Ok(evicted);
        };
        let victim = eviction.entry;
        release_open_order_margin_core(
            pos,
            buy_entries,
            sell_entries,
            side,
            &victim.order_id,
            victim.price,
            market,
        )?;
        evicted = true;
    }
}

/// The owner's `side` list off the STORE, only when it holds a reduce-only entry — the store-path
/// twin of `settlement::RestBasis::own_side_list`, for a rest no match touched. A `_ref` read of
/// the list PHASE 2 inserts into anyway; cloned only when there is something to overtake.
fn stored_side_list_with_reduce_only<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    side: Side,
) -> Result<Option<std::collections::VecDeque<OrderEntry>>, PerpError> {
    let list = match side {
        Side::Buy => storage::load_buy_orders_ref(context, user, market_id)?,
        Side::Sell => storage::load_sell_orders_ref(context, user, market_id)?,
    };
    Ok(list.iter().any(|e| e.reduce_only).then(|| (*list).clone()))
}

// ── FOK / PostOnly pre-checks ─────────────────────────────────────────────────

/// Check whether the book can fully fill a FOK order.  Returns error if not.
///
/// FOK is a LIMIT-only time-in-force ([`OrderKind`] has no market variant that carries one), so the
/// walk always honours `limit_price` — the `order_type` parameter this used to take could only ever
/// be `Limit`, and the dead `Market` half of it was the `Market + FOK` product that no longer
/// exists.
fn check_fok_feasibility<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    limit_price: u64,
    quantity: u64,
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
                if ask_price > limit_price {
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
                if bid_price < limit_price {
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
pub(crate) mod tests;
