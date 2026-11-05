use super::*;

// ── Batch cancel (Phase 1) ───────────────────────────────────────────────────
//
// The shell lives in `crate::batch`: pre-decode length reading, the up-front gas bound, the
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
// Same shell as batch cancel (`crate::batch`), same abort-forward semantics, and
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
pub(super) fn commit_batch_order_nonce<H: PerpHost>(
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
