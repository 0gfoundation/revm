//! Trading engine: place/cancel/query orders with on-chain order-book matching.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use context::{ContextTr, JournalTr};
use primitives::{keccak256, Address, Bytes, FixedBytes, Log};

use context::Block as BlockTr;
use ed25519_dalek::{Signature, VerifyingKey};

mod liquidation;
mod settlement;

pub(crate) use liquidation::{
    execute_liquidation_market_order, settle_liquidation_residual_at_mark_price,
};
use settlement::{settle_maker_fill, TakerSettlement};

use crate::{
    perp_dex::{
        errors::{perp_err, perp_invariant_err},
        interface::IPerpDex::{
            self, cancelOrderCall, cancelOrderSignedCall, getBookLevelCall, getBookPricesCall,
            getMarketFeeTotalCall, getOpenOrdersCall, getOpenOrdersReturn, getOrderCall,
            getOrderReturn, placeOrderCall, placeOrderSignedCall,
        },
        math::{
            calc_maker_fee_for_order_qty_with_bps, calc_reservation_notionals, calc_trading_fee,
            calc_value,
        },
        risk::record_mid_price_sample_for_best_quote_change,
        storage,
        types::{ApiKey, Order, OrderEntry, OrderStatus, OrderType, Side, TimeInForce},
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};

/// Maker fee using the user's *current* fee rate (placement path). Cancel/fill
/// paths instead use the rate snapshotted on the order entry, via
/// `math::calc_maker_fee_for_order_qty_with_bps`.
fn calc_maker_fee_for_order_qty<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    price: u64,
    qty: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<u64, PrecompileError> {
    let rates = storage::load_user_fee_rates(context, user)?;
    calc_maker_fee_for_order_qty_with_bps(price, qty, rates.maker_fee_bps, market)
}

// ── Public entry-points ───────────────────────────────────────────────────────

/// `placeOrder(uint64 marketId, uint8 side, uint64 price, uint64 quantity, uint8 orderType, uint8 tif) returns (bytes32 orderId)`
pub fn run_place_order<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = placeOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("placeOrder: invalid calldata"))?;

    let order_id = next_order_id(context, caller)?;
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
    Ok(Bytes::from(placeOrderCall::abi_encode_returns(
        &FixedBytes(order_id),
    )))
}

/// `placeOrderSigned(address account, ..., uint64 timestamp, bytes signature) returns (bytes32 orderId)`
///
/// orderId = keccak256(signature). Replay protection is implicit: a second submission of the
/// same signature produces the same orderId, which already exists in storage, and is rejected.
pub fn run_place_order_signed<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
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

    // Derive orderId from signature: same sig, same id, duplicate check is the replay guard.
    let order_id: [u8; 32] = keccak256(args.signature.as_ref()).0;
    if storage::load_order(context, &order_id)?.is_some() {
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
    Ok(Bytes::from(placeOrderSignedCall::abi_encode_returns(
        &FixedBytes(order_id),
    )))
}

/// `cancelOrderSigned(address account, bytes32 orderId, uint64 timestamp, bytes signature)`
///
/// Replay protection is implicit: cancelling an already-cancelled order is rejected by
/// cancel_order_core ("order not cancellable").
pub fn run_cancel_order_signed<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
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
pub fn run_cancel_order<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = cancelOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("cancelOrder: invalid calldata"))?;
    cancel_order_core(caller, args.orderId.0, context)
}

/// `getOrder(bytes32 orderId, uint64 marketId) returns (address owner, uint64 marketId, uint8 side, uint64 price, uint64 quantity, uint64 filled, uint8 status)`
///
/// The input `marketId` is accepted for ABI compatibility but ignored — the
/// order is looked up globally by `orderId`.
pub fn run_get_order<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getOrder: invalid calldata"))?;
    let order_id: [u8; 32] = args.orderId.0;

    let order = storage::load_order(context, &order_id)?
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
pub fn run_get_open_orders<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getOpenOrdersCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getOpenOrders: invalid calldata"))?;

    let buy_entries = storage::load_buy_orders(context, args.user, args.marketId)?;
    let sell_entries = storage::load_sell_orders(context, args.user, args.marketId)?;
    let total = buy_entries.len() + sell_entries.len();

    let mut order_ids = Vec::with_capacity(total);
    let mut sides = Vec::with_capacity(total);
    let mut prices = Vec::with_capacity(total);
    let mut remaining_quantities = Vec::with_capacity(total);

    for entry in buy_entries {
        order_ids.push(FixedBytes(entry.order_id));
        sides.push(Side::Buy as u8);
        prices.push(entry.price);
        remaining_quantities.push(entry.amount);
    }
    for entry in sell_entries {
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
pub fn run_get_market_fee_total<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getMarketFeeTotalCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarketFeeTotal: invalid calldata"))?;
    let total = storage::load_market_fee_total(context, args.marketId)?;
    Ok(Bytes::from(getMarketFeeTotalCall::abi_encode_returns(
        &total,
    )))
}

/// `getBookPrices(uint64 marketId, uint8 side) returns (uint64[] prices)`
pub fn run_get_book_prices<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getBookPricesCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getBookPrices: invalid calldata"))?;

    let side = Side::from_u8(args.side).ok_or_else(|| perp_err("getBookPrices: invalid side"))?;
    let prices = match side {
        Side::Buy => storage::load_bid_prices(context, args.marketId)?,
        Side::Sell => storage::load_ask_prices(context, args.marketId)?,
    };

    Ok(Bytes::from(getBookPricesCall::abi_encode_returns(&prices)))
}

/// `getBookLevel(uint64 marketId, uint8 side, uint64 price) returns (bytes32[] orderIds)`
pub fn run_get_book_level<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getBookLevelCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getBookLevel: invalid calldata"))?;

    let side = Side::from_u8(args.side).ok_or_else(|| perp_err("getBookLevel: invalid side"))?;
    let queue = match side {
        Side::Buy => storage::load_bid_level(context, args.marketId, args.price)?,
        Side::Sell => storage::load_ask_level(context, args.marketId, args.price)?,
    };
    let order_ids = queue.into_iter().map(FixedBytes).collect();

    Ok(Bytes::from(getBookLevelCall::abi_encode_returns(
        &order_ids,
    )))
}

// ── Timestamp / recvWindow helpers ────────────────────────────────────────────

const MAX_RECV_WINDOW: u64 = 60; // seconds
const CLOCK_SKEW_ALLOWANCE: u64 = 5; // seconds of future tolerance

pub(crate) fn check_api_key_expiry<CTX: ContextTr>(
    context: &mut CTX,
    key: &ApiKey,
) -> Result<(), &'static str> {
    if key.expiry == 0 {
        return Ok(());
    }
    let block_ts: u64 = context.block().timestamp().saturating_to();
    if block_ts >= key.expiry {
        return Err("api key has expired");
    }
    Ok(())
}

pub(crate) fn check_recv_window<CTX: ContextTr>(
    context: &mut CTX,
    timestamp: u64,
    recv_window: u64,
) -> Result<(), &'static str> {
    let block_ts: u64 = context.block().timestamp().saturating_to();
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
pub(super) fn next_order_id<CTX: ContextTr>(
    context: &mut CTX,
    account: Address,
) -> Result<[u8; 32], PrecompileError> {
    let nonce = storage::load_user_nonce(context, account)?;
    let mut buf = [0u8; 28];
    buf[..20].copy_from_slice(account.as_slice());
    buf[20..28].copy_from_slice(&nonce.to_be_bytes());
    storage::save_user_nonce(context, account, nonce + 1)?;
    Ok(keccak256(&buf).0)
}

struct ValidatedOrder {
    market: crate::perp_dex::types::Market,
    side: Side,
    order_type: OrderType,
    tif: TimeInForce,
}

fn place_order_core<CTX: ContextTr>(
    account: Address,
    order_id: [u8; 32],
    market_id: u64,
    side_u8: u8,
    price: u64,
    quantity: u64,
    order_type_u8: u8,
    tif_u8: u8,
    client_order_id: [u8; 16],
    context: &mut CTX,
) -> Result<(), PrecompileError> {
    let validated = validate_place_order(
        context,
        market_id,
        side_u8,
        price,
        quantity,
        order_type_u8,
        tif_u8,
    )?;

    persist_new_order(
        context,
        account,
        &order_id,
        market_id,
        price,
        quantity,
        client_order_id,
        &validated,
    )?;

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
        ),
        OrderType::Market => execute_market_order(
            context, account, order_id, market_id, price, quantity, validated,
        ),
    }
}

fn validate_place_order<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    side_u8: u8,
    price: u64,
    quantity: u64,
    order_type_u8: u8,
    tif_u8: u8,
) -> Result<ValidatedOrder, PrecompileError> {
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
    }

    Ok(ValidatedOrder {
        market,
        side,
        order_type,
        tif,
    })
}

fn persist_new_order<CTX: ContextTr>(
    context: &mut CTX,
    account: Address,
    order_id: &[u8; 32],
    market_id: u64,
    price: u64,
    quantity: u64,
    client_order_id: [u8; 16],
    order: &ValidatedOrder,
) -> Result<(), PrecompileError> {
    let order = Order {
        owner: account.0 .0,
        market_id,
        side: order.side,
        price,
        quantity,
        filled: 0,
        order_type: order.order_type,
        tif: order.tif,
        status: OrderStatus::Open,
    };
    storage::save_order(context, order_id, &order)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderPlaced {
            user: account,
            marketId: market_id,
            orderId: FixedBytes(*order_id),
            side: order.side as u8,
            price,
            quantity,
            orderType: order.order_type as u8,
            tif: order.tif as u8,
            clientOrderId: FixedBytes(client_order_id),
        }
        .to_log_data(),
    });

    Ok(())
}

fn cancel_unfilled_remainder<CTX: ContextTr>(
    context: &mut CTX,
    order_id: &[u8; 32],
    remaining: u64,
) -> Result<(), PrecompileError> {
    if remaining == 0 {
        return Ok(());
    }

    if let Some(mut o) = storage::load_order(context, order_id)? {
        // Binance-style: TIF expiry (IOC/market not fully filled) → Expired,
        // not Cancelled. Cancelled is reserved for user-initiated cancels.
        o.status = OrderStatus::Expired;
        storage::save_order(context, order_id, &o)?;
    }

    Ok(())
}

fn ensure_fok_filled(remaining: u64) -> Result<(), PrecompileError> {
    if remaining == 0 {
        Ok(())
    } else {
        Err(perp_err("placeOrder: FOK order cannot be fully filled"))
    }
}

fn ensure_post_only_does_not_cross<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    side: Side,
    price: u64,
) -> Result<(), PrecompileError> {
    match side {
        Side::Buy => {
            let best_ask = storage::load_best_ask(context, market_id)?;
            if best_ask != 0 && best_ask <= price {
                return Err(perp_err("placeOrder: PostOnly order would match"));
            }
        }
        Side::Sell => {
            let best_bid = storage::load_best_bid(context, market_id)?;
            if best_bid != 0 && best_bid >= price {
                return Err(perp_err("placeOrder: PostOnly order would match"));
            }
        }
    }

    Ok(())
}

fn remove_order_entry(
    entries: &mut Vec<OrderEntry>,
    order_id: &[u8; 32],
    side_label: &str,
) -> Result<OrderEntry, PrecompileError> {
    let idx = entries
        .iter()
        .position(|e| &e.order_id == order_id)
        .ok_or_else(|| {
            perp_invariant_err(format!(
                "{side_label} entry for order {:?} not found during cancel",
                order_id
            ))
        })?;
    Ok(entries.remove(idx))
}

fn execute_limit_order<CTX: ContextTr>(
    context: &mut CTX,
    account: Address,
    order_id: [u8; 32],
    market_id: u64,
    price: u64,
    quantity: u64,
    client_order_id: [u8; 16],
    order: ValidatedOrder,
) -> Result<(), PrecompileError> {
    match order.tif {
        TimeInForce::PostOnly => {
            ensure_post_only_does_not_cross(context, market_id, order.side, price)?;
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
            )?;
            cancel_unfilled_remainder(context, &order_id, remaining)
        }
        TimeInForce::Fok => {
            check_fok_feasibility(
                context,
                market_id,
                order.side,
                price,
                quantity,
                order.order_type,
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
            )?;
            ensure_fok_filled(remaining)
        }
    }
}

fn execute_market_order<CTX: ContextTr>(
    context: &mut CTX,
    account: Address,
    order_id: [u8; 32],
    market_id: u64,
    price: u64,
    quantity: u64,
    order: ValidatedOrder,
) -> Result<(), PrecompileError> {
    if order.tif == TimeInForce::Fok {
        check_fok_feasibility(
            context,
            market_id,
            order.side,
            price,
            quantity,
            order.order_type,
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
    )?;
    if order.tif == TimeInForce::Fok {
        ensure_fok_filled(remaining)
    } else {
        cancel_unfilled_remainder(context, &order_id, remaining)
    }
}

fn cancel_order_core<CTX: ContextTr>(
    account: Address,
    order_id: [u8; 32],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let mut order = storage::load_order(context, &order_id)?
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
    let market = storage::load_market(context, market_id)?
        .ok_or_else(|| perp_err("cancelOrder: unknown market"))?;
    execute_order_cancellation(
        context,
        account,
        market_id,
        order_id,
        order,
        OrderStatus::Cancelled,
        &market,
        // Explicit cancel: no matching ran in this call, so the BBO cache is live.
        remove_from_book_after_cancel,
    )?;
    Ok(Bytes::new())
}

// ── Matching engine ───────────────────────────────────────────────────────────

/// Core matching loop.  Returns the unfilled quantity after matching.
pub(super) fn match_order<CTX: ContextTr>(
    context: &mut CTX,
    taker_addr: Address,
    taker_order_id: &[u8; 32],
    market_id: u64,
    side: Side,
    limit_price: u64,
    quantity: u64,
    order_type: OrderType,
    _tif: TimeInForce,
    market: &crate::perp_dex::types::Market,
) -> Result<u64, PrecompileError> {
    let mut remaining = quantity;
    let mut last_trade_price = None;
    let mut taker_settlement = TakerSettlement::load(context, taker_addr, market_id)?;

    match side {
        Side::Buy => {
            // Match against asks (sorted ASC: lowest ask first).
            let ask_prices = storage::load_ask_prices(context, market_id)?;
            let old_best_ask = ask_prices.first().copied().unwrap_or(0);
            let mut ask_levels_cleared = false;
            'outer: for ask_price in ask_prices {
                // For limit buy: only match if ask_price <= our limit.
                if order_type == OrderType::Limit && ask_price > limit_price {
                    break;
                }
                let queue = storage::load_ask_level(context, market_id, ask_price)?;
                let mut new_queue: Vec<[u8; 32]> = Vec::new();
                let mut expired_during_level: Vec<[u8; 32]> = Vec::new();
                let mut qi = 0;

                while qi < queue.len() {
                    if remaining == 0 {
                        new_queue.extend(
                            queue[qi..]
                                .iter()
                                .copied()
                                .filter(|id| !expired_during_level.contains(id)),
                        );
                        if new_queue.is_empty() {
                            storage::remove_ask_price(context, market_id, ask_price)?;
                            ask_levels_cleared = true;
                        }
                        storage::save_ask_level(context, market_id, ask_price, &new_queue)?;
                        break 'outer;
                    }
                    let maker_id = queue[qi];
                    qi += 1;

                    let maker_order = match storage::load_order(context, &maker_id)? {
                        Some(o)
                            if matches!(
                                o.status,
                                OrderStatus::Open | OrderStatus::PartiallyFilled
                            ) =>
                        {
                            o
                        }
                        Some(o)
                            if o.status == OrderStatus::Expired
                                && expired_during_level.contains(&maker_id) =>
                        {
                            continue;
                        }
                        Some(o) => {
                            return Err(perp_invariant_err(format!(
                                "ask queue contains order {:?} with terminal status {:?}",
                                maker_id, o.status
                            )))
                        }
                        None => {
                            return Err(perp_invariant_err(format!(
                                "ask queue references order {:?} not found in storage",
                                maker_id
                            )))
                        }
                    };
                    let available = maker_order.quantity - maker_order.filled;
                    let fill_qty = remaining.min(available);

                    taker_settlement.record_fill(ask_price, fill_qty, Side::Buy, market)?;
                    let maker_fill = settle_maker_fill(
                        context,
                        Address::from(maker_order.owner),
                        &maker_id,
                        market_id,
                        ask_price,
                        fill_qty,
                        Side::Buy,
                        market,
                    )?;
                    expired_during_level.extend(maker_fill.expired_order_ids.iter().copied());
                    new_queue.retain(|id| !expired_during_level.contains(id));
                    let fill_notional = calc_value(
                        ask_price,
                        fill_qty,
                        market.base_decimals,
                        market.price_decimals,
                    )?;
                    let taker_fee =
                        calc_trading_fee(fill_notional, taker_settlement.taker_fee_bps())?;
                    last_trade_price = Some(ask_price);
                    emit_trade(
                        context,
                        TradeEvent {
                            market_id,
                            taker_order_id,
                            maker_order_id: &maker_id,
                            taker: taker_addr,
                            maker: Address::from(maker_order.owner),
                            price: ask_price,
                            quantity: fill_qty,
                            taker_side: Side::Buy,
                            taker_fee,
                            maker_fee: maker_fill.maker_fee,
                        },
                    )?;

                    // Update maker order.
                    let mut updated_maker =
                        storage::load_order(context, &maker_id)?.ok_or_else(|| {
                            perp_invariant_err(format!(
                                "maker order {:?} missing after maker settlement",
                                maker_id
                            ))
                        })?;
                    let maker_expired_during_settlement = expired_during_level.contains(&maker_id)
                        || updated_maker.status == OrderStatus::Expired;
                    updated_maker.filled += fill_qty;
                    updated_maker.status = if updated_maker.filled >= updated_maker.quantity {
                        OrderStatus::Filled
                    } else if maker_expired_during_settlement {
                        OrderStatus::Expired
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    storage::save_order(context, &maker_id, &updated_maker)?;

                    // Update taker order.
                    let mut taker_order = storage::load_order(context, taker_order_id)?
                        .ok_or_else(|| {
                            perp_invariant_err("taker order missing after maker settlement")
                        })?;
                    taker_order.filled += fill_qty;
                    taker_order.status = if taker_order.filled >= taker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    storage::save_order(context, taker_order_id, &taker_order)?;

                    remaining -= fill_qty;
                    if updated_maker.status == OrderStatus::PartiallyFilled {
                        new_queue.push(maker_id);
                    }
                }

                if new_queue.is_empty() {
                    storage::remove_ask_price(context, market_id, ask_price)?;
                    ask_levels_cleared = true;
                }
                storage::save_ask_level(context, market_id, ask_price, &new_queue)?;
            }
            if ask_levels_cleared {
                let best_ask = storage::refresh_best_ask(context, market_id)?;
                if best_ask != old_best_ask {
                    let best_bid = storage::load_best_bid(context, market_id)?;
                    record_mid_price_sample_for_best_quote_change(
                        context, market_id, best_bid, best_ask,
                    )?;
                }
            }
        }
        Side::Sell => {
            // Match against bids (sorted DESC: highest bid first).
            let bid_prices = storage::load_bid_prices(context, market_id)?;
            let old_best_bid = bid_prices.first().copied().unwrap_or(0);
            let mut bid_levels_cleared = false;
            'outer: for bid_price in bid_prices {
                // For limit sell: only match if bid_price >= our limit.
                if order_type == OrderType::Limit && bid_price < limit_price {
                    break;
                }
                let queue = storage::load_bid_level(context, market_id, bid_price)?;
                let mut new_queue: Vec<[u8; 32]> = Vec::new();
                let mut expired_during_level: Vec<[u8; 32]> = Vec::new();
                let mut qi = 0;

                while qi < queue.len() {
                    if remaining == 0 {
                        new_queue.extend(
                            queue[qi..]
                                .iter()
                                .copied()
                                .filter(|id| !expired_during_level.contains(id)),
                        );
                        if new_queue.is_empty() {
                            storage::remove_bid_price(context, market_id, bid_price)?;
                            bid_levels_cleared = true;
                        }
                        storage::save_bid_level(context, market_id, bid_price, &new_queue)?;
                        break 'outer;
                    }
                    let maker_id = queue[qi];
                    qi += 1;

                    let maker_order = match storage::load_order(context, &maker_id)? {
                        Some(o)
                            if matches!(
                                o.status,
                                OrderStatus::Open | OrderStatus::PartiallyFilled
                            ) =>
                        {
                            o
                        }
                        Some(o)
                            if o.status == OrderStatus::Expired
                                && expired_during_level.contains(&maker_id) =>
                        {
                            continue;
                        }
                        Some(o) => {
                            return Err(perp_invariant_err(format!(
                                "bid queue contains order {:?} with terminal status {:?}",
                                maker_id, o.status
                            )))
                        }
                        None => {
                            return Err(perp_invariant_err(format!(
                                "bid queue references order {:?} not found in storage",
                                maker_id
                            )))
                        }
                    };
                    let available = maker_order.quantity - maker_order.filled;
                    let fill_qty = remaining.min(available);

                    taker_settlement.record_fill(bid_price, fill_qty, Side::Sell, market)?;
                    let maker_fill = settle_maker_fill(
                        context,
                        Address::from(maker_order.owner),
                        &maker_id,
                        market_id,
                        bid_price,
                        fill_qty,
                        Side::Sell,
                        market,
                    )?;
                    expired_during_level.extend(maker_fill.expired_order_ids.iter().copied());
                    new_queue.retain(|id| !expired_during_level.contains(id));
                    let fill_notional = calc_value(
                        bid_price,
                        fill_qty,
                        market.base_decimals,
                        market.price_decimals,
                    )?;
                    let taker_fee =
                        calc_trading_fee(fill_notional, taker_settlement.taker_fee_bps())?;
                    last_trade_price = Some(bid_price);
                    emit_trade(
                        context,
                        TradeEvent {
                            market_id,
                            taker_order_id,
                            maker_order_id: &maker_id,
                            taker: taker_addr,
                            maker: Address::from(maker_order.owner),
                            price: bid_price,
                            quantity: fill_qty,
                            taker_side: Side::Sell,
                            taker_fee,
                            maker_fee: maker_fill.maker_fee,
                        },
                    )?;

                    let mut updated_maker =
                        storage::load_order(context, &maker_id)?.ok_or_else(|| {
                            perp_invariant_err(format!(
                                "maker order {:?} missing after maker settlement",
                                maker_id
                            ))
                        })?;
                    let maker_expired_during_settlement = expired_during_level.contains(&maker_id)
                        || updated_maker.status == OrderStatus::Expired;
                    updated_maker.filled += fill_qty;
                    updated_maker.status = if updated_maker.filled >= updated_maker.quantity {
                        OrderStatus::Filled
                    } else if maker_expired_during_settlement {
                        OrderStatus::Expired
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    storage::save_order(context, &maker_id, &updated_maker)?;

                    let mut taker_order = storage::load_order(context, taker_order_id)?
                        .ok_or_else(|| {
                            perp_invariant_err("taker order missing after maker settlement")
                        })?;
                    taker_order.filled += fill_qty;
                    taker_order.status = if taker_order.filled >= taker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    storage::save_order(context, taker_order_id, &taker_order)?;

                    remaining -= fill_qty;
                    if updated_maker.status == OrderStatus::PartiallyFilled {
                        new_queue.push(maker_id);
                    }
                }

                if new_queue.is_empty() {
                    storage::remove_bid_price(context, market_id, bid_price)?;
                    bid_levels_cleared = true;
                }
                storage::save_bid_level(context, market_id, bid_price, &new_queue)?;
            }
            if bid_levels_cleared {
                let best_bid = storage::refresh_best_bid(context, market_id)?;
                if best_bid != old_best_bid {
                    let best_ask = storage::load_best_ask(context, market_id)?;
                    record_mid_price_sample_for_best_quote_change(
                        context, market_id, best_bid, best_ask,
                    )?;
                }
            }
        }
    }

    taker_settlement.finalize(context, side, market)?;
    if let Some(price) = last_trade_price {
        storage::save_last_traded_price(context, market_id, price)?;
    }
    Ok(remaining)
}

struct TradeEvent<'a> {
    market_id: u64,
    taker_order_id: &'a [u8; 32],
    maker_order_id: &'a [u8; 32],
    taker: Address,
    maker: Address,
    price: u64,
    quantity: u64,
    taker_side: Side,
    taker_fee: u64,
    maker_fee: u64,
}

fn emit_trade<CTX: ContextTr>(
    context: &mut CTX,
    trade: TradeEvent<'_>,
) -> Result<(), PrecompileError> {
    let trade_id = storage::next_trade_id(context, trade.market_id)?;
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Trade {
            marketId: trade.market_id,
            tradeId: trade_id,
            takerOrderId: FixedBytes(*trade.taker_order_id),
            makerOrderId: FixedBytes(*trade.maker_order_id),
            taker: trade.taker,
            maker: trade.maker,
            price: trade.price,
            quantity: trade.quantity,
            takerSide: trade.taker_side as u8,
            takerFee: trade.taker_fee,
            makerFee: trade.maker_fee,
        }
        .to_log_data(),
    });
    Ok(())
}

// ── Resting in book ───────────────────────────────────────────────────────────

/// Place a limit order that did not (fully) match into the order book and lock margin.
///
/// The margin model uses `max(buy_reserved, sell_reserved)` so only the dominant side
/// actually locks capital.  Adding an order on the weaker side only increases the wallet
/// deduction when it surpasses the other side's reservation.
fn rest_in_book<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    order_id: &[u8; 32],
    market_id: u64,
    side: Side,
    price: u64,
    qty: u64,
    tif: TimeInForce,
    client_order_id: [u8; 16],
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let mut pos = storage::load_position(context, user, market_id)?;
    let mut account = storage::load_account(context, user)?;
    let maker_fee_bps = storage::load_user_fee_rates(context, user)?.maker_fee_bps;

    match side {
        Side::Buy => {
            // Insert the new entry into the user's buy-order list (sorted price DESC).
            let mut entries = storage::load_buy_orders(context, user, market_id)?;
            let idx = entries.partition_point(|e| e.price > price);
            entries.insert(
                idx,
                OrderEntry {
                    order_id: *order_id,
                    price,
                    amount: qty,
                    maker_fee_bps,
                },
            );

            // Recompute the flip-aware reservation from both sides of the book;
            // the reservation fields are written by the single set_reservations
            // source of truth below.
            let sell_entries = storage::load_sell_orders(context, user, market_id)?;
            let (new_buy_side_notional, sell_notional, c_notional) = calc_reservation_notionals(
                &entries,
                &sell_entries,
                market.base_decimals,
                market.price_decimals,
                pos.amount,
            )?;
            let order_fee_reserved =
                calc_maker_fee_for_order_qty(context, user, price, qty, market)?;
            // Adding an order can only grow the buy-side notional (checked before
            // set_reservations overwrites the stored value).
            if new_buy_side_notional < pos.buy_side_reserved_notional {
                return Err(perp_invariant_err(format!(
                    "buy-side reservation notional decreased after adding order: {} -> {}",
                    pos.buy_side_reserved_notional, new_buy_side_notional
                )));
            }

            // Wallet delta is the change in the flip-aware reservation
            // (pos.margin_reserved), NOT the per-side max — the per-side fields
            // lag margin_reserved under the flip-aware model.
            let old_reserved = pos.margin_reserved;
            let leverage = pos.leverage;
            pos.set_reservations(new_buy_side_notional, sell_notional, c_notional, leverage);
            let new_reserved = pos.margin_reserved;
            let margin_delta = new_reserved.saturating_sub(old_reserved);
            let delta = margin_delta
                .checked_add(order_fee_reserved)
                .ok_or_else(|| perp_err("placeOrder: reserve delta overflow"))?;

            if !account.has_available_perp(delta) {
                return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
            }
            account.debit_perp(delta)?;
            pos.fee_reserved = pos
                .fee_reserved
                .checked_add(order_fee_reserved)
                .ok_or_else(|| perp_err("placeOrder: fee reserve overflow"))?;

            // Persist order book state.
            storage::save_buy_orders(context, user, market_id, &entries)?;
            storage::insert_bid_price(context, market_id, price)?;
            storage::push_bid_order(context, market_id, price, *order_id)?;

            // Keep best_bid cache up to date.
            let cur_best_bid = storage::load_best_bid(context, market_id)?;
            if cur_best_bid == 0 || price > cur_best_bid {
                storage::save_best_bid(context, market_id, price)?;
                let best_ask = storage::load_best_ask(context, market_id)?;
                record_mid_price_sample_for_best_quote_change(context, market_id, price, best_ask)?;
            }
        }
        Side::Sell => {
            // Insert the new entry into the user's sell-order list (sorted price ASC).
            let mut entries = storage::load_sell_orders(context, user, market_id)?;
            let idx = entries.partition_point(|e| e.price < price);
            entries.insert(
                idx,
                OrderEntry {
                    order_id: *order_id,
                    price,
                    amount: qty,
                    maker_fee_bps,
                },
            );

            // Recompute the flip-aware reservation from both sides of the book;
            // the reservation fields are written by the single set_reservations
            // source of truth below.
            let buy_entries = storage::load_buy_orders(context, user, market_id)?;
            let (buy_notional, new_sell_side_notional, c_notional) = calc_reservation_notionals(
                &buy_entries,
                &entries,
                market.base_decimals,
                market.price_decimals,
                pos.amount,
            )?;
            let order_fee_reserved =
                calc_maker_fee_for_order_qty(context, user, price, qty, market)?;
            // Adding an order can only grow the sell-side notional (checked before
            // set_reservations overwrites the stored value).
            if new_sell_side_notional < pos.sell_side_reserved_notional {
                return Err(perp_invariant_err(format!(
                    "sell-side reservation notional decreased after adding order: {} -> {}",
                    pos.sell_side_reserved_notional, new_sell_side_notional
                )));
            }

            // Wallet delta is the change in the flip-aware reservation
            // (pos.margin_reserved), NOT the per-side max — the per-side fields
            // lag margin_reserved under the flip-aware model.
            let old_reserved = pos.margin_reserved;
            let leverage = pos.leverage;
            pos.set_reservations(buy_notional, new_sell_side_notional, c_notional, leverage);
            let new_reserved = pos.margin_reserved;
            let margin_delta = new_reserved.saturating_sub(old_reserved);
            let delta = margin_delta
                .checked_add(order_fee_reserved)
                .ok_or_else(|| perp_err("placeOrder: reserve delta overflow"))?;

            if !account.has_available_perp(delta) {
                return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
            }
            account.debit_perp(delta)?;
            pos.fee_reserved = pos
                .fee_reserved
                .checked_add(order_fee_reserved)
                .ok_or_else(|| perp_err("placeOrder: fee reserve overflow"))?;

            // Persist order book state.
            storage::save_sell_orders(context, user, market_id, &entries)?;
            storage::insert_ask_price(context, market_id, price)?;
            storage::push_ask_order(context, market_id, price, *order_id)?;

            // Keep best_ask cache up to date.
            let cur_best_ask = storage::load_best_ask(context, market_id)?;
            if cur_best_ask == 0 || price < cur_best_ask {
                storage::save_best_ask(context, market_id, price)?;
                let best_bid = storage::load_best_bid(context, market_id)?;
                record_mid_price_sample_for_best_quote_change(context, market_id, best_bid, price)?;
            }
        }
    }

    storage::save_position(context, user, market_id, &pos)?;
    storage::save_account(context, user, account)?;

    context.journal_mut().log(Log {
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
/// remove from book → release reserved margin → mark Cancelled → emit log.
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
pub(super) fn execute_order_cancellation<CTX: ContextTr, F>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    order_id: [u8; 32],
    mut order: Order,
    terminal_status: OrderStatus,
    market: &crate::perp_dex::types::Market,
    remove: F,
) -> Result<(), PrecompileError>
where
    F: FnOnce(&mut CTX, u64, Side, u64, &[u8; 32]) -> Result<(), PrecompileError>,
{
    remove(context, market_id, order.side, order.price, &order_id)?;
    release_margin_for_cancelled_order(context, user, market_id, order.side, &order_id, market)?;
    order.status = terminal_status;
    storage::save_order(context, &order_id, &order)?;
    context.journal_mut().log(Log {
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
fn detach_order_from_level<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    side: Side,
    price: u64,
    order_id: &[u8; 32],
) -> Result<(bool, u64), PrecompileError> {
    let (old_best, emptied) = match side {
        Side::Buy => {
            let old_best = storage::load_best_bid(context, market_id)?;
            let mut queue = storage::load_bid_level(context, market_id, price)?;
            queue.retain(|id| id != order_id);
            let emptied = queue.is_empty();
            if emptied {
                storage::remove_bid_price(context, market_id, price)?;
            }
            storage::save_bid_level(context, market_id, price, &queue)?;
            (old_best, emptied)
        }
        Side::Sell => {
            let old_best = storage::load_best_ask(context, market_id)?;
            let mut queue = storage::load_ask_level(context, market_id, price)?;
            queue.retain(|id| id != order_id);
            let emptied = queue.is_empty();
            if emptied {
                storage::remove_ask_price(context, market_id, price)?;
            }
            storage::save_ask_level(context, market_id, price, &queue)?;
            (old_best, emptied)
        }
    };
    Ok((emptied, old_best))
}

/// Recompute the side's best from its (already-mutated) price list and, if it moved
/// off `old_best`, record a mid-price sample. Shared by both removal entry points.
fn refresh_best_and_sample<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    side: Side,
    old_best: u64,
) -> Result<(), PrecompileError> {
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
pub(super) fn remove_from_book_after_cancel<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    side: Side,
    price: u64,
    order_id: &[u8; 32],
) -> Result<(), PrecompileError> {
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
pub(super) fn remove_from_book_during_match<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    side: Side,
    price: u64,
    order_id: &[u8; 32],
) -> Result<(), PrecompileError> {
    let (emptied, old_best) = detach_order_from_level(context, market_id, side, price, order_id)?;
    if emptied {
        refresh_best_and_sample(context, market_id, side, old_best)?;
    }
    Ok(())
}

pub(super) fn release_margin_for_cancelled_order<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    side: Side,
    order_id: &[u8; 32],
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let mut pos = storage::load_position(context, user, market_id)?;
    let mut account = storage::load_account(context, user)?;

    match side {
        Side::Buy => {
            // Flip-aware reservation snapshot before removal (pos.margin_reserved,
            // not the per-side max — the per-side fields lag it under the model).
            let old_reserved = pos.margin_reserved;
            let mut entries = storage::load_buy_orders(context, user, market_id)?;
            // The book entry's `amount` is the authoritative remaining quantity
            // (kept current by reduce_maker_order_entry_for_fill); the order's
            // `filled` can lag it during the same matching round, so the release
            // is sized from the entry, not from order.quantity - order.filled.
            let cancelled_entry = remove_order_entry(&mut entries, order_id, "buy")?;
            let sell_entries = storage::load_sell_orders(context, user, market_id)?;
            let (new_notional, sell_notional, c_notional) = calc_reservation_notionals(
                &entries,
                &sell_entries,
                market.base_decimals,
                market.price_decimals,
                pos.amount,
            )?;
            let leverage = pos.leverage;
            pos.set_reservations(new_notional, sell_notional, c_notional, leverage);
            let new_reserved = pos.margin_reserved;
            let freed = old_reserved.saturating_sub(new_reserved);
            let fee_freed = calc_maker_fee_for_order_qty_with_bps(
                cancelled_entry.price,
                cancelled_entry.amount,
                cancelled_entry.maker_fee_bps,
                market,
            )?;
            // Return released margin + fee reservation in one credit (mirrors the
            // combined debit on the placement path).
            let total_freed = freed
                .checked_add(fee_freed)
                .ok_or_else(|| perp_err("cancel: released reserve overflow"))?;
            account.credit_perp(total_freed)?;
            // Surface drift instead of masking it (was saturating_sub).
            let prev_fee = pos.fee_reserved;
            pos.fee_reserved = prev_fee.checked_sub(fee_freed).ok_or_else(|| {
                perp_invariant_err(format!(
                    "fee_reserved {prev_fee} < fee to release {fee_freed}"
                ))
            })?;
            storage::save_buy_orders(context, user, market_id, &entries)?;
        }
        Side::Sell => {
            // Flip-aware reservation snapshot before removal (pos.margin_reserved,
            // not the per-side max — the per-side fields lag it under the model).
            let old_reserved = pos.margin_reserved;
            let mut entries = storage::load_sell_orders(context, user, market_id)?;
            // The book entry's `amount` is the authoritative remaining quantity
            // (kept current by reduce_maker_order_entry_for_fill); the order's
            // `filled` can lag it during the same matching round, so the release
            // is sized from the entry, not from order.quantity - order.filled.
            let cancelled_entry = remove_order_entry(&mut entries, order_id, "sell")?;
            let buy_entries = storage::load_buy_orders(context, user, market_id)?;
            let (buy_notional, new_notional, c_notional) = calc_reservation_notionals(
                &buy_entries,
                &entries,
                market.base_decimals,
                market.price_decimals,
                pos.amount,
            )?;
            let leverage = pos.leverage;
            pos.set_reservations(buy_notional, new_notional, c_notional, leverage);
            let new_reserved = pos.margin_reserved;
            let freed = old_reserved.saturating_sub(new_reserved);
            let fee_freed = calc_maker_fee_for_order_qty_with_bps(
                cancelled_entry.price,
                cancelled_entry.amount,
                cancelled_entry.maker_fee_bps,
                market,
            )?;
            // Return released margin + fee reservation in one credit (mirrors the
            // combined debit on the placement path).
            let total_freed = freed
                .checked_add(fee_freed)
                .ok_or_else(|| perp_err("cancel: released reserve overflow"))?;
            account.credit_perp(total_freed)?;
            // Surface drift instead of masking it (was saturating_sub).
            let prev_fee = pos.fee_reserved;
            pos.fee_reserved = prev_fee.checked_sub(fee_freed).ok_or_else(|| {
                perp_invariant_err(format!(
                    "fee_reserved {prev_fee} < fee to release {fee_freed}"
                ))
            })?;
            storage::save_sell_orders(context, user, market_id, &entries)?;
        }
    }

    storage::save_position(context, user, market_id, &pos)?;
    storage::save_account(context, user, account)?;
    Ok(())
}

// ── FOK / PostOnly pre-checks ─────────────────────────────────────────────────

/// Check whether the book can fully fill a FOK order.  Returns error if not.
fn check_fok_feasibility<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    side: Side,
    limit_price: u64,
    quantity: u64,
    order_type: OrderType,
) -> Result<(), PrecompileError> {
    let mut available: u64 = 0;
    match side {
        Side::Buy => {
            let ask_prices = storage::load_ask_prices(context, market_id)?;
            'outer: for ask_price in ask_prices {
                if order_type == OrderType::Limit && ask_price > limit_price {
                    break;
                }
                let queue = storage::load_ask_level(context, market_id, ask_price)?;
                for maker_id in &queue {
                    if let Some(o) = storage::load_order(context, maker_id)? {
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
            let bid_prices = storage::load_bid_prices(context, market_id)?;
            'outer: for bid_price in bid_prices {
                if order_type == OrderType::Limit && bid_price < limit_price {
                    break;
                }
                let queue = storage::load_bid_level(context, market_id, bid_price)?;
                for maker_id in &queue {
                    if let Some(o) = storage::load_order(context, maker_id)? {
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
