//! Trading engine: place/cancel/query orders with on-chain order-book matching.

use alloy_sol_types::SolCall;
use context::ContextTr;
use primitives::{keccak256, Address, Bytes, FixedBytes};

use crate::{
    perp_dex::{
        errors::perp_err,
        interface::IPerpDex::{cancelOrderCall, getOrderCall, getOrderReturn, placeOrderCall},
        math::{calc_buy_side_margin_reserved, calc_sell_side_margin_reserved, calc_value},
        storage,
        types::{Order, OrderEntry, OrderStatus, OrderType, Side, TimeInForce},
    },
    PrecompileError,
};

// ── Public entry-points ───────────────────────────────────────────────────────

/// `placeOrder(uint64 marketId, uint8 side, uint64 price, uint64 quantity, uint8 orderType, uint8 tif) returns (bytes32 orderId)`
pub fn run_place_order<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = placeOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("placeOrder: invalid calldata"))?;

    let market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("placeOrder: unknown market"))?;
    if !market.active {
        return Err(perp_err("placeOrder: market not active"));
    }

    let side = Side::from_u8(args.side)
        .ok_or_else(|| perp_err("placeOrder: invalid side"))?;
    let order_type = OrderType::from_u8(args.orderType)
        .ok_or_else(|| perp_err("placeOrder: invalid orderType"))?;
    let tif = TimeInForce::from_u8(args.tif)
        .ok_or_else(|| perp_err("placeOrder: invalid tif"))?;

    if args.quantity < market.min_quantity {
        return Err(perp_err("placeOrder: quantity below minimum"));
    }
    if market.step_size > 0 && args.quantity % market.step_size != 0 {
        return Err(perp_err("placeOrder: quantity not multiple of step_size"));
    }
    if order_type == OrderType::Limit {
        if args.price == 0 {
            return Err(perp_err("placeOrder: limit order price must be > 0"));
        }
        if market.tick_size > 0 && args.price % market.tick_size != 0 {
            return Err(perp_err("placeOrder: price not multiple of tick_size"));
        }
    }

    // Generate order_id = keccak256(caller || nonce)
    let nonce = storage::load_user_nonce(context, caller)?;
    let mut hash_input = [0u8; 28];
    hash_input[..20].copy_from_slice(caller.as_slice());
    hash_input[20..28].copy_from_slice(&nonce.to_be_bytes());
    let order_id: [u8; 32] = keccak256(&hash_input).0;
    storage::save_user_nonce(context, caller, nonce + 1)?;

    // For FOK: check whether the book has enough liquidity before touching state.
    if tif == TimeInForce::Fok {
        check_fok_feasibility(context, args.marketId, side, args.price, args.quantity, order_type)?;
    }

    // For PostOnly: reject if the order would immediately match.
    if tif == TimeInForce::PostOnly && order_type == OrderType::Limit {
        check_post_only(context, args.marketId, side, args.price)?;
    }

    // Save the order record.
    let order = Order {
        owner: caller.0 .0,
        market_id: args.marketId,
        side,
        price: args.price,
        quantity: args.quantity,
        filled: 0,
        order_type,
        tif,
        status: OrderStatus::Open,
    };
    storage::save_order(context, &order_id, &order)?;

    // Execute matching.
    let remaining = match_order(
        context,
        caller,
        &order_id,
        args.marketId,
        side,
        args.price,
        args.quantity,
        order_type,
        tif,
        &market,
    )?;

    // Rest remaining in book for GTC/PostOnly limit orders.
    if remaining > 0 && order_type == OrderType::Limit && matches!(tif, TimeInForce::Gtc | TimeInForce::PostOnly) {
        rest_in_book(context, caller, &order_id, args.marketId, side, args.price, remaining, &market)?;
    } else if remaining == 0 {
        // Already fully filled — status updated inside match_order.
    } else {
        // IOC/FOK remainder: cancel.
        if let Some(mut o) = storage::load_order(context, &order_id)? {
            o.status = OrderStatus::Cancelled;
            storage::save_order(context, &order_id, &o)?;
        }
    }

    let ret_id: FixedBytes<32> = FixedBytes(order_id);
    Ok(Bytes::from(placeOrderCall::abi_encode_returns(&ret_id)))
}

/// `cancelOrder(bytes32 orderId)`
pub fn run_cancel_order<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = cancelOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("cancelOrder: invalid calldata"))?;
    let order_id: [u8; 32] = args.orderId.0;

    let mut order = storage::load_order(context, &order_id)?
        .ok_or_else(|| perp_err("cancelOrder: order not found"))?;

    if order.owner != caller.0 .0 {
        return Err(perp_err("cancelOrder: not owner"));
    }
    if !matches!(order.status, OrderStatus::Open | OrderStatus::PartiallyFilled) {
        return Err(perp_err("cancelOrder: order not cancellable"));
    }

    let remaining = order.quantity - order.filled;
    let market_id = order.market_id;
    let price = order.price;

    // Remove from order book queue.
    remove_from_book(context, market_id, order.side, price, &order_id)?;

    // Remove from user order entry list, recalculate margin reserved.
    let market = storage::load_market(context, market_id)?
        .ok_or_else(|| perp_err("cancelOrder: unknown market"))?;
    release_margin_for_cancelled_order(
        context, caller, market_id, order.side, &order_id, remaining, &market,
    )?;

    order.status = OrderStatus::Cancelled;
    storage::save_order(context, &order_id, &order)?;

    Ok(Bytes::new())
}

/// `getOrder(bytes32 orderId) returns (address owner, uint64 marketId, uint8 side, uint64 price, uint64 quantity, uint64 filled, uint8 status)`
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
    Ok(Bytes::from(getOrderCall::abi_encode_returns(&getOrderReturn {
        owner,
        marketId: order.market_id,
        side: order.side as u8,
        price: order.price,
        quantity: order.quantity,
        filled: order.filled,
        status: order.status as u8,
    })))
}

// ── Matching engine ───────────────────────────────────────────────────────────

/// Core matching loop.  Returns the unfilled quantity after matching.
fn match_order<CTX: ContextTr>(
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

    match side {
        Side::Buy => {
            // Match against asks (sorted ASC — lowest ask first).
            let ask_prices = storage::load_ask_prices(context, market_id)?;
            'outer: for ask_price in ask_prices {
                // For limit buy: only match if ask_price <= our limit.
                if order_type == OrderType::Limit && ask_price > limit_price {
                    break;
                }
                let queue = storage::load_ask_level(context, market_id, ask_price)?;
                let mut new_queue: Vec<[u8; 32]> = Vec::new();
                let mut qi = 0;

                while qi < queue.len() {
                    if remaining == 0 {
                        new_queue.extend_from_slice(&queue[qi..]);
                        storage::save_ask_level(context, market_id, ask_price, &new_queue)?;
                        break 'outer;
                    }
                    let maker_id = queue[qi];
                    qi += 1;

                    let maker_order = match storage::load_order(context, &maker_id)? {
                        Some(o) if matches!(o.status, OrderStatus::Open | OrderStatus::PartiallyFilled) => o,
                        _ => continue,
                    };
                    let available = maker_order.quantity - maker_order.filled;
                    let fill_qty = remaining.min(available);

                    // Settle fill for both sides.
                    settle_fill(context, taker_addr, Address::from(maker_order.owner), market_id, ask_price, fill_qty, Side::Buy, market)?;

                    // Update maker order.
                    let mut updated_maker = storage::load_order(context, &maker_id)?.unwrap();
                    updated_maker.filled += fill_qty;
                    updated_maker.status = if updated_maker.filled >= updated_maker.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    storage::save_order(context, &maker_id, &updated_maker)?;

                    // Update maker's sell-order entry list.
                    update_sell_entry_after_fill(context, Address::from(maker_order.owner), market_id, &maker_id, fill_qty, market)?;

                    // Update taker order.
                    let mut taker_order = storage::load_order(context, taker_order_id)?.unwrap();
                    taker_order.filled += fill_qty;
                    taker_order.status = if taker_order.filled >= taker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    storage::save_order(context, taker_order_id, &taker_order)?;

                    remaining -= fill_qty;
                    if updated_maker.status != OrderStatus::Filled {
                        new_queue.push(maker_id);
                    }
                }

                if new_queue.is_empty() {
                    storage::remove_ask_price(context, market_id, ask_price)?;
                }
                storage::save_ask_level(context, market_id, ask_price, &new_queue)?;
            }
        }
        Side::Sell => {
            // Match against bids (sorted DESC — highest bid first).
            let bid_prices = storage::load_bid_prices(context, market_id)?;
            'outer: for bid_price in bid_prices {
                // For limit sell: only match if bid_price >= our limit.
                if order_type == OrderType::Limit && bid_price < limit_price {
                    break;
                }
                let queue = storage::load_bid_level(context, market_id, bid_price)?;
                let mut new_queue: Vec<[u8; 32]> = Vec::new();
                let mut qi = 0;

                while qi < queue.len() {
                    if remaining == 0 {
                        new_queue.extend_from_slice(&queue[qi..]);
                        storage::save_bid_level(context, market_id, bid_price, &new_queue)?;
                        break 'outer;
                    }
                    let maker_id = queue[qi];
                    qi += 1;

                    let maker_order = match storage::load_order(context, &maker_id)? {
                        Some(o) if matches!(o.status, OrderStatus::Open | OrderStatus::PartiallyFilled) => o,
                        _ => continue,
                    };
                    let available = maker_order.quantity - maker_order.filled;
                    let fill_qty = remaining.min(available);

                    settle_fill(context, taker_addr, Address::from(maker_order.owner), market_id, bid_price, fill_qty, Side::Sell, market)?;

                    let mut updated_maker = storage::load_order(context, &maker_id)?.unwrap();
                    updated_maker.filled += fill_qty;
                    updated_maker.status = if updated_maker.filled >= updated_maker.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    storage::save_order(context, &maker_id, &updated_maker)?;

                    update_buy_entry_after_fill(context, Address::from(maker_order.owner), market_id, &maker_id, fill_qty, market)?;

                    let mut taker_order = storage::load_order(context, taker_order_id)?.unwrap();
                    taker_order.filled += fill_qty;
                    taker_order.status = if taker_order.filled >= taker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    storage::save_order(context, taker_order_id, &taker_order)?;

                    remaining -= fill_qty;
                    if updated_maker.status != OrderStatus::Filled {
                        new_queue.push(maker_id);
                    }
                }

                if new_queue.is_empty() {
                    storage::remove_bid_price(context, market_id, bid_price)?;
                }
                storage::save_bid_level(context, market_id, bid_price, &new_queue)?;
            }
        }
    }

    Ok(remaining)
}

// ── Position settlement ───────────────────────────────────────────────────────

/// Apply a fill to both taker and maker positions and account wallets.
fn settle_fill<CTX: ContextTr>(
    context: &mut CTX,
    taker: Address,
    maker: Address,
    market_id: u64,
    fill_price: u64,
    fill_qty: u64,
    taker_side: Side, // the taker's side (Buy = taker bought, maker sold)
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let fill_value = calc_value(fill_price, fill_qty, market.base_decimals);

    // Taker side.
    {
        let mut pos = storage::load_position(context, taker, market_id)?;
        let mut account = storage::load_account(context, taker)?;
        apply_fill_to_position(&mut pos, &mut account.perp_wallet_balance, fill_qty, fill_value, taker_side == Side::Buy);
        storage::save_position(context, taker, market_id, &pos)?;
        storage::save_account(context, taker, account)?;
    }

    // Maker side (opposite of taker).
    {
        let maker_side = taker_side.opposite();
        let mut pos = storage::load_position(context, maker, market_id)?;
        let mut account = storage::load_account(context, maker)?;
        apply_fill_to_position(&mut pos, &mut account.perp_wallet_balance, fill_qty, fill_value, maker_side == Side::Buy);
        storage::save_position(context, maker, market_id, &pos)?;
        storage::save_account(context, maker, account)?;
    }

    Ok(())
}

/// Update a single position and perp-wallet balance for one fill leg.
///
/// `is_buy`: true if this participant is buying (amount increases).
fn apply_fill_to_position(
    pos: &mut crate::perp_dex::types::PerpPosition,
    wallet: &mut u64,
    fill_qty: u64,
    fill_value: u64,
    is_buy: bool,
) {
    let leverage = pos.leverage.max(1);

    // How much of this fill is closing existing position vs opening new?
    let closing_qty: u64 = if is_buy && pos.amount < 0 {
        fill_qty.min((-pos.amount) as u64)
    } else if !is_buy && pos.amount > 0 {
        fill_qty.min(pos.amount as u64)
    } else {
        0
    };
    let opening_qty = fill_qty - closing_qty;

    // --- Closing portion: realise PnL, release margin ---
    if closing_qty > 0 {
        let pos_abs = pos.amount.unsigned_abs() as u128;
        // Proportional margin to release
        let margin_release = (pos.margin.max(0) as u128 * closing_qty as u128 / pos_abs) as i64;
        // Proportional v_quote contribution for the closing portion
        let vq_fraction = (pos.v_quote_balance as i128 * closing_qty as i128 / pos_abs as i128) as i64;
        // Quote delta for closing portion
        let close_quote_delta: i64 = if is_buy {
            -((fill_value as u128 * closing_qty as u128 / fill_qty as u128) as i64)
        } else {
            (fill_value as u128 * closing_qty as u128 / fill_qty as u128) as i64
        };
        // Realised equity returned to wallet = margin_release + pnl
        // pnl = vq_fraction + close_quote_delta  (both in quote units = USDC 6-decimal)
        let realised = margin_release + vq_fraction + close_quote_delta;
        if realised > 0 {
            *wallet = wallet.saturating_add(realised as u64);
        } else if realised < 0 {
            *wallet = wallet.saturating_sub((-realised) as u64);
        }
        pos.margin -= margin_release;
    }

    // --- Opening portion: deduct initial margin from wallet ---
    if opening_qty > 0 {
        let open_value = (fill_value as u128 * opening_qty as u128 / fill_qty.max(1) as u128) as u64;
        let initial_margin = open_value / leverage;
        *wallet = wallet.saturating_sub(initial_margin);
        pos.margin += initial_margin as i64;
    }

    // Apply to position fields.
    if is_buy {
        pos.amount += fill_qty as i64;
        pos.v_quote_balance -= fill_value as i64;
    } else {
        pos.amount -= fill_qty as i64;
        pos.v_quote_balance += fill_value as i64;
    }
}

// ── Resting in book ───────────────────────────────────────────────────────────

/// Add a resting limit order to the order book and reserve margin.
fn rest_in_book<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    order_id: &[u8; 32],
    market_id: u64,
    side: Side,
    price: u64,
    qty: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let mut pos = storage::load_position(context, user, market_id)?;
    let mut account = storage::load_account(context, user)?;

    match side {
        Side::Buy => {
            let mut entries = storage::load_buy_orders(context, user, market_id)?;
            let old_reserved = pos.buy_side_margin_reserved;

            let new_entry = OrderEntry { order_id: *order_id, price, amount: qty };
            let idx = entries.partition_point(|e| e.price > price);
            entries.insert(idx, new_entry);

            let new_reserved = calc_buy_side_margin_reserved(&entries, pos.leverage.max(1), market.base_decimals, pos.amount);
            let delta = new_reserved.saturating_sub(old_reserved);

            if account.perp_wallet_balance < delta {
                return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
            }
            account.perp_wallet_balance -= delta;
            pos.buy_side_margin_reserved = new_reserved;
            pos.margin_reserved = pos.buy_side_margin_reserved + pos.sell_side_margin_reserved;

            storage::save_buy_orders(context, user, market_id, &entries)?;
            storage::insert_bid_price(context, market_id, price)?;
            storage::push_bid_order(context, market_id, price, *order_id)?;
        }
        Side::Sell => {
            let mut entries = storage::load_sell_orders(context, user, market_id)?;
            let old_reserved = pos.sell_side_margin_reserved;

            let new_entry = OrderEntry { order_id: *order_id, price, amount: qty };
            let idx = entries.partition_point(|e| e.price < price);
            entries.insert(idx, new_entry);

            let new_reserved = calc_sell_side_margin_reserved(&entries, pos.leverage.max(1), market.base_decimals, pos.amount);
            let delta = new_reserved.saturating_sub(old_reserved);

            if account.perp_wallet_balance < delta {
                return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
            }
            account.perp_wallet_balance -= delta;
            pos.sell_side_margin_reserved = new_reserved;
            pos.margin_reserved = pos.buy_side_margin_reserved + pos.sell_side_margin_reserved;

            storage::save_sell_orders(context, user, market_id, &entries)?;
            storage::insert_ask_price(context, market_id, price)?;
            storage::push_ask_order(context, market_id, price, *order_id)?;
        }
    }

    storage::save_position(context, user, market_id, &pos)?;
    storage::save_account(context, user, account)?;
    Ok(())
}

// ── Cancel helpers ─────────────────────────────────────────────────────────────

fn remove_from_book<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    side: Side,
    price: u64,
    order_id: &[u8; 32],
) -> Result<(), PrecompileError> {
    match side {
        Side::Buy => {
            let mut queue = storage::load_bid_level(context, market_id, price)?;
            queue.retain(|id| id != order_id);
            if queue.is_empty() {
                storage::remove_bid_price(context, market_id, price)?;
            }
            storage::save_bid_level(context, market_id, price, &queue)?;
        }
        Side::Sell => {
            let mut queue = storage::load_ask_level(context, market_id, price)?;
            queue.retain(|id| id != order_id);
            if queue.is_empty() {
                storage::remove_ask_price(context, market_id, price)?;
            }
            storage::save_ask_level(context, market_id, price, &queue)?;
        }
    }
    Ok(())
}

fn release_margin_for_cancelled_order<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    side: Side,
    order_id: &[u8; 32],
    _remaining_qty: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let mut pos = storage::load_position(context, user, market_id)?;
    let mut account = storage::load_account(context, user)?;

    match side {
        Side::Buy => {
            let old_reserved = pos.buy_side_margin_reserved;
            let mut entries = storage::load_buy_orders(context, user, market_id)?;
            entries.retain(|e| &e.order_id != order_id);
            let new_reserved = calc_buy_side_margin_reserved(&entries, pos.leverage.max(1), market.base_decimals, pos.amount);
            let freed = old_reserved.saturating_sub(new_reserved);
            account.perp_wallet_balance = account.perp_wallet_balance.saturating_add(freed);
            pos.buy_side_margin_reserved = new_reserved;
            pos.margin_reserved = pos.buy_side_margin_reserved + pos.sell_side_margin_reserved;
            storage::save_buy_orders(context, user, market_id, &entries)?;
        }
        Side::Sell => {
            let old_reserved = pos.sell_side_margin_reserved;
            let mut entries = storage::load_sell_orders(context, user, market_id)?;
            entries.retain(|e| &e.order_id != order_id);
            let new_reserved = calc_sell_side_margin_reserved(&entries, pos.leverage.max(1), market.base_decimals, pos.amount);
            let freed = old_reserved.saturating_sub(new_reserved);
            account.perp_wallet_balance = account.perp_wallet_balance.saturating_add(freed);
            pos.sell_side_margin_reserved = new_reserved;
            pos.margin_reserved = pos.buy_side_margin_reserved + pos.sell_side_margin_reserved;
            storage::save_sell_orders(context, user, market_id, &entries)?;
        }
    }

    storage::save_position(context, user, market_id, &pos)?;
    storage::save_account(context, user, account)?;
    Ok(())
}

// ── Order entry updates after fill ───────────────────────────────────────────

/// Update the maker's buy-order entry after it was filled (partial or full).
fn update_buy_entry_after_fill<CTX: ContextTr>(
    context: &mut CTX,
    maker: Address,
    market_id: u64,
    order_id: &[u8; 32],
    fill_qty: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let mut entries = storage::load_buy_orders(context, maker, market_id)?;
    let mut pos = storage::load_position(context, maker, market_id)?;
    let mut account = storage::load_account(context, maker)?;

    let old_reserved = pos.buy_side_margin_reserved;
    if let Some(e) = entries.iter_mut().find(|e| &e.order_id == order_id) {
        e.amount = e.amount.saturating_sub(fill_qty);
        if e.amount == 0 {
            entries.retain(|e| &e.order_id != order_id);
        }
    }
    let new_reserved = calc_buy_side_margin_reserved(&entries, pos.leverage.max(1), market.base_decimals, pos.amount);
    let freed = old_reserved.saturating_sub(new_reserved);
    account.perp_wallet_balance = account.perp_wallet_balance.saturating_add(freed);
    pos.buy_side_margin_reserved = new_reserved;
    pos.margin_reserved = pos.buy_side_margin_reserved + pos.sell_side_margin_reserved;

    storage::save_buy_orders(context, maker, market_id, &entries)?;
    storage::save_position(context, maker, market_id, &pos)?;
    storage::save_account(context, maker, account)?;
    Ok(())
}

/// Update the maker's sell-order entry after it was filled (partial or full).
fn update_sell_entry_after_fill<CTX: ContextTr>(
    context: &mut CTX,
    maker: Address,
    market_id: u64,
    order_id: &[u8; 32],
    fill_qty: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let mut entries = storage::load_sell_orders(context, maker, market_id)?;
    let mut pos = storage::load_position(context, maker, market_id)?;
    let mut account = storage::load_account(context, maker)?;

    let old_reserved = pos.sell_side_margin_reserved;
    if let Some(e) = entries.iter_mut().find(|e| &e.order_id == order_id) {
        e.amount = e.amount.saturating_sub(fill_qty);
        if e.amount == 0 {
            entries.retain(|e| &e.order_id != order_id);
        }
    }
    let new_reserved = calc_sell_side_margin_reserved(&entries, pos.leverage.max(1), market.base_decimals, pos.amount);
    let freed = old_reserved.saturating_sub(new_reserved);
    account.perp_wallet_balance = account.perp_wallet_balance.saturating_add(freed);
    pos.sell_side_margin_reserved = new_reserved;
    pos.margin_reserved = pos.buy_side_margin_reserved + pos.sell_side_margin_reserved;

    storage::save_sell_orders(context, maker, market_id, &entries)?;
    storage::save_position(context, maker, market_id, &pos)?;
    storage::save_account(context, maker, account)?;
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

/// Check that a PostOnly order would not immediately match (returns error if it would).
fn check_post_only<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    side: Side,
    limit_price: u64,
) -> Result<(), PrecompileError> {
    match side {
        Side::Buy => {
            let ask_prices = storage::load_ask_prices(context, market_id)?;
            if let Some(&best_ask) = ask_prices.first() {
                if best_ask <= limit_price {
                    return Err(perp_err("placeOrder: PostOnly order would match"));
                }
            }
        }
        Side::Sell => {
            let bid_prices = storage::load_bid_prices(context, market_id)?;
            if let Some(&best_bid) = bid_prices.first() {
                if best_bid >= limit_price {
                    return Err(perp_err("placeOrder: PostOnly order would match"));
                }
            }
        }
    }
    Ok(())
}
