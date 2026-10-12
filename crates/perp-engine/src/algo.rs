//! Conditional (algo) orders — stop-loss / take-profit.
//!
//! # Why this is its own selector family and not flags on `placeOrder`
//!
//! Binance moved the whole conditional family OFF `/fapi/v1/order` onto the Algo Order API:
//! `STOP`, `TAKE_PROFIT`, `STOP_MARKET` and `TRAILING_STOP_MARKET` all answer `-4120` on the old
//! endpoint ("Please use the Algo Order API endpoints instead"), measured four-for-four on a live
//! account. The precompile analogue of "a different endpoint" is a different SELECTOR.
//!
//! # The shape of a conditional order's life
//!
//! ```text
//! placeAlgoOrder  ->  lives ONLY in the per-market trigger index
//!                     not in the book, no price level, no queue position,
//!                     no open-order requirement, no margin  (Binance: "no margin
//!                     check when placing conditional orders")
//!        |
//!     trigger fires on the MARK
//!        v
//!                 ->  becomes an ORDINARY order and is routed through the ordinary
//!                     placement path, where reduce-only admission, the fill-time price
//!                     band and the fee schedule all apply exactly as they always do
//! ```
//!
//! Triggering itself is NOT in this module — it belongs to the price-driven sweep next to the band
//! GC and the liquidation sweep, and lands separately.
//!
//! # Phase scope
//!
//! Only reduce-only / closePosition conditionals are accepted. Binance performs no margin check
//! before trigger, which leaves "triggered but cannot afford to open" as a case its own
//! documentation does not describe; a reduce-only conditional has a zero open-order requirement
//! before AND after trigger, so the case cannot arise here at all.

use crate::host::PerpHost;
use crate::interface::IPerpDex::{
    self, cancelAlgoOrderCall, cancelAlgoOrderSignedCall, getAlgoOrderCall, getOpenAlgoOrdersCall,
    placeAlgoOrderCall, placeAlgoOrderSignedCall,
};
use crate::types::{
    AlgoOrderType, CancelReason, Order, OrderStatus, OrderType, Side, TimeInForce, WorkingType,
    ALGO_TYPE_CONDITIONAL,
};
use crate::{errors::perp_err, storage, PerpError, PERP_DEX_ADDRESS};
use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use primitives::{Address, Bytes, FixedBytes, Log};

/// Untriggered conditional orders one user may hold in one market.
///
/// Not cosmetic: liquidation and ADL both call `cancel_all_orders_for_market`, which must clear
/// conditional orders too (they are not in the book, so walking the book would miss them), and
/// that runs INSIDE an already-capped sweep. An uncapped per-user list would put unbounded work
/// on the liquidation path — the same shape as every other cap in this engine.
///
/// Binance's limit is 200 per ACCOUNT across all symbols; this is per (user, market), which is
/// the unit the cancel-all walk actually pays for.
pub const MAX_ALGO_ORDERS_PER_MARKET: usize = 32;

const FLAG_REDUCE_ONLY: u8 = 1 << 0;
const FLAG_CLOSE_POSITION: u8 = 1 << 1;
const KNOWN_FLAGS: u8 = FLAG_REDUCE_ONLY | FLAG_CLOSE_POSITION;

/// Everything `placeAlgoOrder` and its signed twin agree on before anything is written.
struct ValidatedAlgo {
    side: Side,
    kind: AlgoOrderType,
    trigger_price: u64,
    trigger_up: bool,
    price: u64,
    quantity: u64,
    tif: TimeInForce,
    reduce_only: bool,
    close_position: bool,
}

#[allow(clippy::too_many_arguments)]
fn validate(
    algo_type: u8,
    side: u8,
    order_type: u8,
    trigger_price: u64,
    price: u64,
    quantity: u64,
    tif: u8,
    working_type: u8,
    flags: u8,
    market: &crate::types::Market,
) -> Result<ValidatedAlgo, PerpError> {
    if algo_type != ALGO_TYPE_CONDITIONAL {
        return Err(perp_err("placeAlgoOrder: algoType must be CONDITIONAL"));
    }
    // ⚠️ Rejected rather than substituted. Binance's DEFAULT working type is CONTRACT_PRICE, so a
    // client ported from there which omits the field is relying on last-trade semantics; quietly
    // triggering it off the mark instead would be the worst of both worlds.
    if working_type != WorkingType::MarkPrice as u8 {
        return Err(perp_err(
            "placeAlgoOrder: workingType must be MARK_PRICE (CONTRACT_PRICE is not supported)",
        ));
    }
    let side = Side::from_u8(side).ok_or_else(|| perp_err("placeAlgoOrder: invalid side"))?;
    let kind = AlgoOrderType::from_u8(order_type).ok_or_else(|| {
        perp_err("placeAlgoOrder: invalid orderType (TRAILING_STOP_MARKET is not supported)")
    })?;
    if trigger_price == 0 {
        return Err(perp_err("placeAlgoOrder: triggerPrice must be non-zero"));
    }
    if trigger_price % market.tick_size != 0 {
        return Err(perp_err("placeAlgoOrder: triggerPrice must be a multiple of tickSize"));
    }
    if flags & !KNOWN_FLAGS != 0 {
        return Err(perp_err("placeAlgoOrder: unknown flag bits"));
    }
    let reduce_only = flags & FLAG_REDUCE_ONLY != 0;
    let close_position = flags & FLAG_CLOSE_POSITION != 0;

    // Phase scope — see the module docs for why this restriction removes a whole failure class
    // rather than merely deferring it.
    if !reduce_only && !close_position {
        return Err(perp_err(
            "placeAlgoOrder: this build accepts only reduceOnly or closePosition conditionals",
        ));
    }

    let is_market = kind.is_market();
    if close_position {
        if !is_market {
            return Err(perp_err(
                "placeAlgoOrder: closePosition requires STOP_MARKET or TAKE_PROFIT_MARKET",
            ));
        }
        if quantity != 0 {
            return Err(perp_err("placeAlgoOrder: closePosition and quantity are exclusive"));
        }
    } else if quantity == 0 {
        return Err(perp_err("placeAlgoOrder: quantity must be non-zero"));
    }

    if is_market {
        if price != 0 {
            return Err(perp_err("placeAlgoOrder: price must be 0 for a *_MARKET type"));
        }
        if tif != 0 {
            return Err(perp_err("placeAlgoOrder: tif is unused for a *_MARKET type, send 0"));
        }
    } else {
        if price == 0 {
            return Err(perp_err("placeAlgoOrder: price is required for a limit type"));
        }
        if price % market.tick_size != 0 {
            return Err(perp_err("placeAlgoOrder: price must be a multiple of tickSize"));
        }
    }
    let tif = TimeInForce::from_u8(tif).ok_or_else(|| perp_err("placeAlgoOrder: invalid tif"))?;

    if quantity != 0 {
        if quantity % market.step_size != 0 {
            return Err(perp_err("placeAlgoOrder: quantity must be a multiple of stepSize"));
        }
        if quantity > market.max_quantity {
            return Err(perp_err("placeAlgoOrder: quantity exceeds maxQuantity"));
        }
        // ⚠️ No minQuantity check: a reduce-only order is exempt from the minimum, measured on
        // Binance (R16) — a single step against a 5-unit floor was accepted. The whole point is to
        // be able to close a dust remainder.
    }

    Ok(ValidatedAlgo {
        side,
        kind,
        trigger_price,
        trigger_up: kind.trigger_up(side),
        price,
        quantity,
        tif,
        reduce_only,
        close_position,
    })
}

/// Shared tail: write the record, index it, emit. `algo_id` is already derived and, on the signed
/// path, its signature already burned.
fn place_core<H: PerpHost>(
    context: &mut H,
    account: Address,
    market_id: u64,
    algo_id: [u8; 32],
    v: ValidatedAlgo,
    client_algo_id: FixedBytes<16>,
    flags: u8,
) -> Result<Bytes, PerpError> {
    let mut live = storage::load_user_algo_orders(context, account, market_id)?;
    if live.len() >= MAX_ALGO_ORDERS_PER_MARKET {
        return Err(perp_err(
            "placeAlgoOrder: too many open conditional orders in this market",
        ));
    }

    let order = Order {
        owner: account.0 .0,
        market_id,
        side: v.side,
        price: v.price,
        quantity: v.quantity,
        filled: 0,
        // The ORDINARY type the trigger will produce — a conditional order is not a third kind of
        // order, it is one of these two with a condition in front of it.
        order_type: if v.kind.is_market() {
            OrderType::Market
        } else {
            OrderType::Limit
        },
        tif: v.tif,
        status: OrderStatus::Open,
        reduce_only: v.reduce_only,
        trigger_price: v.trigger_price,
        trigger_up: v.trigger_up,
        close_position: v.close_position,
    };
    storage::save_order(context, &algo_id, &order)?;
    storage::algo_index_insert(context, market_id, v.trigger_up, v.trigger_price, algo_id)?;
    live.push(algo_id);
    storage::save_user_algo_orders(context, account, market_id, &live)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AlgoOrderPlaced {
            user: account,
            marketId: market_id,
            algoId: FixedBytes(algo_id),
            side: v.side as u8,
            orderType: v.kind as u8,
            triggerPrice: v.trigger_price,
            price: v.price,
            quantity: v.quantity,
            tif: v.tif as u8,
            clientAlgoId: client_algo_id,
            flags,
        }
        .to_log_data(),
    });
    Ok(Bytes::from(FixedBytes::<32>(algo_id).to_vec()))
}

/// Remove an untriggered conditional order from both indexes and delete it. Idempotent in the
/// index, so cancel / cancel-all / the sweep may all reach the same id.
pub(crate) fn remove_algo_order<H: PerpHost>(
    context: &mut H,
    account: Address,
    market_id: u64,
    algo_id: &[u8; 32],
    order: &Order,
    reason: CancelReason,
) -> Result<(), PerpError> {
    storage::algo_index_remove(
        context,
        market_id,
        order.trigger_up,
        order.trigger_price,
        algo_id,
    )?;
    let mut live = storage::load_user_algo_orders(context, account, market_id)?;
    live.retain(|id| id != algo_id);
    storage::save_user_algo_orders(context, account, market_id, &live)?;
    storage::delete_order(context, algo_id)?;
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AlgoOrderCancelled {
            user: account,
            marketId: market_id,
            algoId: FixedBytes(*algo_id),
            reason: reason as u8,
        }
        .to_log_data(),
    });
    Ok(())
}

/// `placeAlgoOrder(...)`
pub fn run_place_algo_order<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = placeAlgoOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("placeAlgoOrder: invalid calldata"))?;
    let market = storage::load_market_ref(context, a.marketId)?
        .ok_or_else(|| perp_err("placeAlgoOrder: unknown market"))?;
    if !market.active {
        return Err(perp_err("placeAlgoOrder: market is not active"));
    }
    let v = validate(
        a.algoType,
        a.side,
        a.orderType,
        a.triggerPrice,
        a.price,
        a.quantity,
        a.tif,
        a.workingType,
        a.flags,
        &market,
    )?;
    let (algo_id, bumped) = crate::trading::peek_order_id(context, caller)?;
    crate::trading::commit_order_nonce(context, caller, bumped)?;
    place_core(context, caller, a.marketId, algo_id, v, a.clientAlgoId, a.flags)
}

/// `cancelAlgoOrder(bytes32 algoId, uint64 marketId)`
pub fn run_cancel_algo_order<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = cancelAlgoOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("cancelAlgoOrder: invalid calldata"))?;
    cancel_core(context, caller, a.algoId.0, a.marketId)?;
    Ok(Bytes::new())
}

fn cancel_core<H: PerpHost>(
    context: &mut H,
    account: Address,
    algo_id: [u8; 32],
    market_id: u64,
) -> Result<(), PerpError> {
    let order = storage::load_order(context, &algo_id)?
        .ok_or_else(|| perp_err("cancelAlgoOrder: not found"))?;
    if Address::from(order.owner) != account {
        return Err(perp_err("cancelAlgoOrder: not the owner"));
    }
    if order.trigger_price == 0 {
        return Err(perp_err(
            "cancelAlgoOrder: not a conditional order (already triggered — use cancelOrder)",
        ));
    }
    if order.market_id != market_id {
        return Err(perp_err("cancelAlgoOrder: marketId mismatch"));
    }
    remove_algo_order(context, account, market_id, &algo_id, &order, CancelReason::User)
}

/// `getAlgoOrder(bytes32 algoId)`
pub fn run_get_algo_order<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = getAlgoOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAlgoOrder: invalid calldata"))?;
    let o = storage::load_order(context, &a.algoId.0)?
        .ok_or_else(|| perp_err("getAlgoOrder: not found"))?;
    if o.trigger_price == 0 {
        return Err(perp_err("getAlgoOrder: not a conditional order"));
    }
    let kind = if o.order_type == OrderType::Market {
        if o.trigger_up == matches!(o.side, Side::Buy) {
            AlgoOrderType::StopMarket
        } else {
            AlgoOrderType::TakeProfitMarket
        }
    } else if o.trigger_up == matches!(o.side, Side::Buy) {
        AlgoOrderType::Stop
    } else {
        AlgoOrderType::TakeProfit
    };
    Ok(Bytes::from(getAlgoOrderCall::abi_encode_returns(
        &crate::interface::IPerpDex::getAlgoOrderReturn {
            owner: Address::from(o.owner),
            marketId: o.market_id,
            side: o.side as u8,
            orderType: kind as u8,
            triggerPrice: o.trigger_price,
            price: o.price,
            quantity: o.quantity,
            tif: o.tif as u8,
            status: o.status as u8,
            reduceOnly: o.reduce_only,
            closePosition: o.close_position,
        },
    )))
}

/// `getOpenAlgoOrders(address user, uint64 marketId)`
pub fn run_get_open_algo_orders<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = getOpenAlgoOrdersCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getOpenAlgoOrders: invalid calldata"))?;
    let ids = storage::load_user_algo_orders(context, a.user, a.marketId)?;
    let out: Vec<FixedBytes<32>> = ids.into_iter().map(FixedBytes).collect();
    Ok(Bytes::from(getOpenAlgoOrdersCall::abi_encode_returns(&out)))
}

// ── Signed (API-key) variants ─────────────────────────────────────────────────────────────────
//
// The path real traders use: sign locally with a registered ed25519 key, let anyone relay and pay
// the gas. Binance's analogue is `X-MBX-APIKEY` + an HMAC over the query string; `keyId` names the
// key slot and the signature covers a fixed-layout canonical message including the CHAIN ID, so a
// signature minted for one chain cannot be replayed onto another.
//
// Validation order is the same on all signed entrypoints and is not arbitrary:
//   load_api_key -> check_recv_window -> check_api_key_expiry -> verify_ed25519 -> burn -> core

/// `placeAlgoOrderSigned(...)`
pub fn run_place_algo_order_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = placeAlgoOrderSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("placeAlgoOrderSigned: invalid calldata"))?;

    let api_key = storage::load_api_key(context, a.account, a.keyId)?
        .ok_or_else(|| perp_err("placeAlgoOrderSigned: no api key registered for account"))?;
    crate::trading::check_recv_window(context, a.timestamp, a.recvWindow)
        .map_err(|e| perp_err(&format!("placeAlgoOrderSigned: {e}")))?;
    crate::trading::check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("placeAlgoOrderSigned: {e}")))?;

    // Canonical fixed-layout message. ⚠️ No byte total is quoted: the array literal IS the length
    // and the closing `debug_assert_eq!` enforces it. A number in a comment here went stale three
    // times on the sibling entrypoints before it was removed.
    //   "perpdex_v1_algo" || chainId(8) || account(20) || algoType(1) || marketId(8) || side(1)
    //   || orderType(1) || triggerPrice(8) || price(8) || quantity(8) || tif(1) || workingType(1)
    //   || clientAlgoId(16) || flags(1) || timestamp(8) || recvWindow(8) || keyId(1)
    let mut msg = [0u8; 114];
    let mut c = crate::trading::write_signed_header(&mut msg, b"perpdex_v1_algo", context.chain_id());
    c = crate::trading::put(&mut msg, c, a.account.as_slice());
    c = crate::trading::put(&mut msg, c, &[a.algoType]);
    c = crate::trading::put(&mut msg, c, &a.marketId.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[a.side, a.orderType]);
    c = crate::trading::put(&mut msg, c, &a.triggerPrice.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &a.price.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &a.quantity.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[a.tif, a.workingType]);
    c = crate::trading::put(&mut msg, c, &a.clientAlgoId.0);
    c = crate::trading::put(&mut msg, c, &[a.flags]);
    c = crate::trading::put(&mut msg, c, &a.timestamp.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &a.recvWindow.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[a.keyId]);
    debug_assert_eq!(c, msg.len(), "placeAlgoOrderSigned message layout");

    crate::trading::verify_ed25519(&api_key.pubkey, &msg, &a.signature)
        .map_err(|e| perp_err(&format!("placeAlgoOrderSigned: {e}")))?;

    // algoId = keccak256(signature); the SAME hash keys the seen-signature replay set, exactly as
    // on `placeOrderSigned`.
    let algo_id: [u8; 32] = primitives::keccak256(a.signature.as_ref()).0;
    if storage::is_signature_seen(context, &algo_id)? {
        return Err(perp_err(
            "placeAlgoOrderSigned: duplicate signature (already submitted)",
        ));
    }

    // Validate BEFORE the burn, so a malformed request costs no write at all. Everything after
    // this point either succeeds or is a sanctioned reject-after-write.
    let market = storage::load_market_ref(context, a.marketId)?
        .ok_or_else(|| perp_err("placeAlgoOrderSigned: unknown market"))?;
    if !market.active {
        return Err(perp_err("placeAlgoOrderSigned: market is not active"));
    }
    let v = validate(
        a.algoType,
        a.side,
        a.orderType,
        a.triggerPrice,
        a.price,
        a.quantity,
        a.tif,
        a.workingType,
        a.flags,
        &market,
    )
    .map_err(|e| perp_err(&format!("placeAlgoOrderSigned: {e}")))?;

    // ── the signature is burned UNCONDITIONALLY, BEFORE placement ──
    // Same rule as every other signed entrypoint: a one-shot authorization is consumed by
    // SUBMISSION, not by success, the way an Ethereum nonce is spent by a failed transaction.
    // Burning only on success leaves a REJECTED signature replayable for the rest of its recv
    // window, which is reachable whenever the rejection was transient.
    let writes_before_burn = context.perp_write_count();
    storage::mark_signature_seen(context, &algo_id, a.timestamp)?;
    // Best-effort by design: this runs AFTER a committed burn, so surfacing its `Err` would
    // revert a call whose signature is already spent.
    let _ = crate::trading::gc_seen_buckets_best_effort(context, a.timestamp);
    let burn_writes = context.perp_write_count() - writes_before_burn;

    place_core(context, a.account, a.marketId, algo_id, v, a.clientAlgoId, a.flags).map_err(|e| {
        match e {
            PerpError::Reject(msg) => PerpError::RejectAfterRetainedWrite {
                message: msg,
                sanctioned_writes: burn_writes,
            },
            other => other,
        }
    })
}

/// `cancelAlgoOrderSigned(...)`
pub fn run_cancel_algo_order_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = cancelAlgoOrderSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("cancelAlgoOrderSigned: invalid calldata"))?;

    let api_key = storage::load_api_key(context, a.account, a.keyId)?
        .ok_or_else(|| perp_err("cancelAlgoOrderSigned: no api key registered for account"))?;
    crate::trading::check_recv_window(context, a.timestamp, a.recvWindow)
        .map_err(|e| perp_err(&format!("cancelAlgoOrderSigned: {e}")))?;
    crate::trading::check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("cancelAlgoOrderSigned: {e}")))?;

    //   "perpdex_v1_algocancel" || chainId(8) || account(20) || algoId(32) || marketId(8)
    //   || timestamp(8) || recvWindow(8) || keyId(1)
    let mut msg = [0u8; 106];
    let mut c =
        crate::trading::write_signed_header(&mut msg, b"perpdex_v1_algocancel", context.chain_id());
    c = crate::trading::put(&mut msg, c, a.account.as_slice());
    c = crate::trading::put(&mut msg, c, &a.algoId.0);
    c = crate::trading::put(&mut msg, c, &a.marketId.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &a.timestamp.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &a.recvWindow.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[a.keyId]);
    debug_assert_eq!(c, msg.len(), "cancelAlgoOrderSigned message layout");

    crate::trading::verify_ed25519(&api_key.pubkey, &msg, &a.signature)
        .map_err(|e| perp_err(&format!("cancelAlgoOrderSigned: {e}")))?;

    // No seen-signature burn here, for the same reason `cancelOrderSigned` has none: replay
    // protection is implicit, because a cancelled conditional order is DELETED and the second
    // attempt finds nothing to cancel.
    cancel_core(context, a.account, a.algoId.0, a.marketId)?;
    Ok(Bytes::new())
}
