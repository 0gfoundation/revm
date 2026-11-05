use super::*;

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
