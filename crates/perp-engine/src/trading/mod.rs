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

mod batch_ops;
pub use batch_ops::*;

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

mod resting;
pub(crate) use resting::*;

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
