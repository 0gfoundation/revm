//! Trading engine: place/cancel/query orders with on-chain order-book matching.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use context::{ContextTr, JournalTr};
use primitives::{keccak256, Address, Bytes, FixedBytes, Log};

use crate::{
    perp_dex::{
        errors::{perp_err, perp_invariant_err},
        interface::IPerpDex::{
            self, cancelOrderCall, getOrderCall, getOrderReturn, placeOrderCall,
        },
        math::{calc_buy_side_margin_reserved, calc_sell_side_margin_reserved, calc_value},
        storage,
        types::{Order, OrderEntry, OrderStatus, OrderType, Side, TimeInForce},
        PERP_DEX_ADDRESS,
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
    if args.quantity > market.max_quantity {
        return Err(perp_err("placeOrder: quantity exceeds maximum"));
    }
    if market.step_size > 0 && args.quantity % market.step_size != 0 {
        return Err(perp_err("placeOrder: quantity not multiple of step_size"));
    }
    if order_type == OrderType::Limit {
        if args.price == 0 {
            return Err(perp_err("placeOrder: limit order price must be > 0"));
        }
        if args.price > market.max_price {
            return Err(perp_err("placeOrder: price exceeds maximum"));
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

    // Determine whether matching can be skipped entirely.
    // For PostOnly this also validates the order won't cross — reads best_ask/bid once.
    let skip_match = should_skip_match(context, args.marketId, side, args.price, order_type, tif)?;

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
    let remaining = if skip_match {
        args.quantity
    } else {
        match_order(
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
        )?
    };

    // Rest remaining in book for GTC/PostOnly limit orders.
    if remaining > 0 && order_type == OrderType::Limit && matches!(tif, TimeInForce::Gtc | TimeInForce::PostOnly) {
        rest_in_book(context, caller, &order_id, args.marketId, side, args.price, remaining, tif, &market)?;
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

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderCancelled {
            user: caller,
            orderId: args.orderId,
            marketId: market_id,
        }
        .to_log_data(),
    });

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
            let mut ask_levels_cleared = false;
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
                        Some(o) => return Err(perp_invariant_err(format!(
                            "ask queue contains order {:?} with terminal status {:?}", maker_id, o.status
                        ))),
                        None => return Err(perp_invariant_err(format!(
                            "ask queue references order {:?} not found in storage", maker_id
                        ))),
                    };
                    let available = maker_order.quantity - maker_order.filled;
                    let fill_qty = remaining.min(available);

                    // Settle fill for both sides.
                    settle_fill(context, taker_addr, Address::from(maker_order.owner), taker_order_id, &maker_id, market_id, ask_price, fill_qty, Side::Buy, market)?;

                    // Update maker order.
                    let mut updated_maker = storage::load_order(context, &maker_id)?
                        .ok_or_else(|| perp_invariant_err(format!("maker order {:?} missing after settle_fill", maker_id)))?;
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
                    let mut taker_order = storage::load_order(context, taker_order_id)?
                        .ok_or_else(|| perp_invariant_err("taker order missing after settle_fill"))?;
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
                    ask_levels_cleared = true;
                }
                storage::save_ask_level(context, market_id, ask_price, &new_queue)?;
            }
            if ask_levels_cleared {
                storage::refresh_best_ask(context, market_id)?;
            }
        }
        Side::Sell => {
            // Match against bids (sorted DESC — highest bid first).
            let bid_prices = storage::load_bid_prices(context, market_id)?;
            let mut bid_levels_cleared = false;
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
                        Some(o) => return Err(perp_invariant_err(format!(
                            "bid queue contains order {:?} with terminal status {:?}", maker_id, o.status
                        ))),
                        None => return Err(perp_invariant_err(format!(
                            "bid queue references order {:?} not found in storage", maker_id
                        ))),
                    };
                    let available = maker_order.quantity - maker_order.filled;
                    let fill_qty = remaining.min(available);

                    settle_fill(context, taker_addr, Address::from(maker_order.owner), taker_order_id, &maker_id, market_id, bid_price, fill_qty, Side::Sell, market)?;

                    let mut updated_maker = storage::load_order(context, &maker_id)?
                        .ok_or_else(|| perp_invariant_err(format!("maker order {:?} missing after settle_fill", maker_id)))?;
                    updated_maker.filled += fill_qty;
                    updated_maker.status = if updated_maker.filled >= updated_maker.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    storage::save_order(context, &maker_id, &updated_maker)?;

                    update_buy_entry_after_fill(context, Address::from(maker_order.owner), market_id, &maker_id, fill_qty, market)?;

                    let mut taker_order = storage::load_order(context, taker_order_id)?
                        .ok_or_else(|| perp_invariant_err("taker order missing after settle_fill"))?;
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
                    bid_levels_cleared = true;
                }
                storage::save_bid_level(context, market_id, bid_price, &new_queue)?;
            }
            if bid_levels_cleared {
                storage::refresh_best_bid(context, market_id)?;
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
    taker_order_id: &[u8; 32],
    maker_order_id: &[u8; 32],
    market_id: u64,
    fill_price: u64,
    fill_qty: u64,
    taker_side: Side, // the taker's side (Buy = taker bought, maker sold)
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let fill_value = calc_value(fill_price, fill_qty, market.base_decimals);

    // Taker side.
    let taker_pos = {
        let mut pos = storage::load_position(context, taker, market_id)?;
        let mut account = storage::load_account(context, taker)?;
        let shortfall = apply_fill_to_position(&mut pos, &mut account.perp_wallet_balance, fill_qty, fill_value, taker_side == Side::Buy);
        storage::save_position(context, taker, market_id, &pos)?;
        storage::save_account(context, taker, account)?;
        if shortfall > 0 {
            let cancel_side = if taker_side == Side::Buy { Side::Sell } else { Side::Buy };
            cancel_underfunded_side(context, taker, market_id, cancel_side, shortfall, market)?;
        }
        pos
    };

    // Maker side (opposite of taker).
    let maker_pos = {
        let maker_side = taker_side.opposite();
        let mut pos = storage::load_position(context, maker, market_id)?;
        let mut account = storage::load_account(context, maker)?;
        let shortfall = apply_fill_to_position(&mut pos, &mut account.perp_wallet_balance, fill_qty, fill_value, maker_side == Side::Buy);
        storage::save_position(context, maker, market_id, &pos)?;
        storage::save_account(context, maker, account)?;
        if shortfall > 0 {
            let cancel_side = if maker_side == Side::Buy { Side::Sell } else { Side::Buy };
            cancel_underfunded_side(context, maker, market_id, cancel_side, shortfall, market)?;
        }
        pos
    };

    // Emit Trade event.
    let trade_id = storage::next_trade_id(context, market_id)?;
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Trade {
            marketId: market_id,
            tradeId: trade_id,
            takerOrderId: FixedBytes(*taker_order_id),
            makerOrderId: FixedBytes(*maker_order_id),
            taker,
            maker,
            price: fill_price,
            quantity: fill_qty,
            takerSide: taker_side as u8,
        }
        .to_log_data(),
    });

    // Emit PositionChanged for taker.
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user: taker,
            marketId: market_id,
            amount: taker_pos.amount,
            vQuoteBalance: taker_pos.v_quote_balance,
            margin: taker_pos.margin,
            leverage: taker_pos.leverage,
        }
        .to_log_data(),
    });

    // Emit PositionChanged for maker.
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user: maker,
            marketId: market_id,
            amount: maker_pos.amount,
            vQuoteBalance: maker_pos.v_quote_balance,
            margin: maker_pos.margin,
            leverage: maker_pos.leverage,
        }
        .to_log_data(),
    });

    Ok(())
}

/// Update a single position and perp-wallet balance for one fill leg.
///
/// `is_buy`: true if this participant is buying (amount increases).
///
/// Returns the opening-margin **shortfall**: the amount of initial margin that could not be
/// deducted because the wallet was insufficient.  Under the max-reservation scheme the caller
/// must cancel enough orders on the opposite side to cover this shortfall.
fn apply_fill_to_position(
    pos: &mut crate::perp_dex::types::PerpPosition,
    wallet: &mut u64,
    fill_qty: u64,
    fill_value: u64,
    is_buy: bool,
) -> u64 {
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
        let margin_release = (pos.margin.max(0) as u128 * closing_qty as u128 / pos_abs) as i64;
        let vq_fraction = (pos.v_quote_balance as i128 * closing_qty as i128 / pos_abs as i128) as i64;
        let close_quote_delta: i64 = if is_buy {
            -((fill_value as u128 * closing_qty as u128 / fill_qty as u128) as i64)
        } else {
            (fill_value as u128 * closing_qty as u128 / fill_qty as u128) as i64
        };
        let realised = margin_release + vq_fraction + close_quote_delta;
        if realised > 0 {
            *wallet = wallet.saturating_add(realised as u64);
        } else if realised < 0 {
            *wallet = wallet.saturating_sub((-realised) as u64);
        }
        pos.margin -= margin_release;
    }

    // --- Opening portion: deduct initial margin from wallet ---
    // Only credit pos.margin with what was actually deducted; return any shortfall to the
    // caller so it can cancel opposite-side orders to recover the deficit.
    let shortfall = if opening_qty > 0 {
        let open_value = (fill_value as u128 * opening_qty as u128 / fill_qty.max(1) as u128) as u64;
        let initial_margin = open_value / leverage;
        let wallet_before = *wallet;
        *wallet = wallet.saturating_sub(initial_margin);
        let actually_deducted = wallet_before - *wallet; // min(wallet_before, initial_margin)
        pos.margin += actually_deducted as i64;
        initial_margin.saturating_sub(wallet_before) // > 0 only when wallet was insufficient
    } else {
        0
    };

    // Apply to position fields.
    if is_buy {
        pos.amount += fill_qty as i64;
        pos.v_quote_balance -= fill_value as i64;
    } else {
        pos.amount -= fill_qty as i64;
        pos.v_quote_balance += fill_value as i64;
    }

    shortfall
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
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let mut pos = storage::load_position(context, user, market_id)?;
    let mut account = storage::load_account(context, user)?;

    match side {
        Side::Buy => {
            // Insert the new entry into the user's buy-order list (sorted price DESC).
            let mut entries = storage::load_buy_orders(context, user, market_id)?;
            let idx = entries.partition_point(|e| e.price > price);
            entries.insert(idx, OrderEntry { order_id: *order_id, price, amount: qty });

            // Recompute how much buy-side margin this user now needs in total.
            let new_buy_side_reserved = calc_buy_side_margin_reserved(
                &entries, pos.leverage.max(1), market.base_decimals, pos.amount,
            );

            // Under max-reservation, the wallet delta is the change in max(buy, sell).
            let old_max = pos.buy_side_margin_reserved.max(pos.sell_side_margin_reserved);
            let new_max = new_buy_side_reserved.max(pos.sell_side_margin_reserved);
            if new_buy_side_reserved < pos.buy_side_margin_reserved {
                return Err(perp_invariant_err(format!(
                    "buy-side reservation decreased after adding order: {} -> {}",
                    pos.buy_side_margin_reserved, new_buy_side_reserved
                )));
            }
            let delta = new_max.saturating_sub(old_max);

            if account.perp_wallet_balance < delta {
                return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
            }
            account.perp_wallet_balance -= delta;
            pos.buy_side_margin_reserved = new_buy_side_reserved;
            pos.margin_reserved = new_max;

            // Persist order book state.
            storage::save_buy_orders(context, user, market_id, &entries)?;
            storage::insert_bid_price(context, market_id, price)?;
            storage::push_bid_order(context, market_id, price, *order_id)?;

            // Keep best_bid cache up to date.
            let cur_best_bid = storage::load_best_bid(context, market_id)?;
            if cur_best_bid == 0 || price > cur_best_bid {
                storage::save_best_bid(context, market_id, price)?;
            }
        }
        Side::Sell => {
            // Insert the new entry into the user's sell-order list (sorted price ASC).
            let mut entries = storage::load_sell_orders(context, user, market_id)?;
            let idx = entries.partition_point(|e| e.price < price);
            entries.insert(idx, OrderEntry { order_id: *order_id, price, amount: qty });

            // Recompute how much sell-side margin this user now needs in total.
            let new_sell_side_reserved = calc_sell_side_margin_reserved(
                &entries, pos.leverage.max(1), market.base_decimals, pos.amount,
            );

            // Under max-reservation, the wallet delta is the change in max(buy, sell).
            let old_max = pos.buy_side_margin_reserved.max(pos.sell_side_margin_reserved);
            let new_max = pos.buy_side_margin_reserved.max(new_sell_side_reserved);
            if new_sell_side_reserved < pos.sell_side_margin_reserved {
                return Err(perp_invariant_err(format!(
                    "sell-side reservation decreased after adding order: {} -> {}",
                    pos.sell_side_margin_reserved, new_sell_side_reserved
                )));
            }
            let delta = new_max.saturating_sub(old_max);

            if account.perp_wallet_balance < delta {
                return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
            }
            account.perp_wallet_balance -= delta;
            pos.sell_side_margin_reserved = new_sell_side_reserved;
            pos.margin_reserved = new_max;

            // Persist order book state.
            storage::save_sell_orders(context, user, market_id, &entries)?;
            storage::insert_ask_price(context, market_id, price)?;
            storage::push_ask_order(context, market_id, price, *order_id)?;

            // Keep best_ask cache up to date.
            let cur_best_ask = storage::load_best_ask(context, market_id)?;
            if cur_best_ask == 0 || price < cur_best_ask {
                storage::save_best_ask(context, market_id, price)?;
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
        }
        .to_log_data(),
    });

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
                storage::refresh_best_bid(context, market_id)?;
            }
            storage::save_bid_level(context, market_id, price, &queue)?;
        }
        Side::Sell => {
            let mut queue = storage::load_ask_level(context, market_id, price)?;
            queue.retain(|id| id != order_id);
            if queue.is_empty() {
                storage::remove_ask_price(context, market_id, price)?;
                storage::refresh_best_ask(context, market_id)?;
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
            let old_max = pos.buy_side_margin_reserved.max(pos.sell_side_margin_reserved);
            let mut entries = storage::load_buy_orders(context, user, market_id)?;
            entries.retain(|e| &e.order_id != order_id);
            let new_reserved = calc_buy_side_margin_reserved(&entries, pos.leverage.max(1), market.base_decimals, pos.amount);
            let new_max = new_reserved.max(pos.sell_side_margin_reserved);
            let freed = old_max.saturating_sub(new_max);
            account.perp_wallet_balance = account.perp_wallet_balance.saturating_add(freed);
            pos.buy_side_margin_reserved = new_reserved;
            pos.margin_reserved = new_max;
            storage::save_buy_orders(context, user, market_id, &entries)?;
        }
        Side::Sell => {
            let old_max = pos.buy_side_margin_reserved.max(pos.sell_side_margin_reserved);
            let mut entries = storage::load_sell_orders(context, user, market_id)?;
            entries.retain(|e| &e.order_id != order_id);
            let new_reserved = calc_sell_side_margin_reserved(&entries, pos.leverage.max(1), market.base_decimals, pos.amount);
            let new_max = pos.buy_side_margin_reserved.max(new_reserved);
            let freed = old_max.saturating_sub(new_max);
            account.perp_wallet_balance = account.perp_wallet_balance.saturating_add(freed);
            pos.sell_side_margin_reserved = new_reserved;
            pos.margin_reserved = new_max;
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

    let old_max = pos.buy_side_margin_reserved.max(pos.sell_side_margin_reserved);
    match entries.iter_mut().find(|e| &e.order_id == order_id) {
        Some(e) => {
            e.amount = e.amount.saturating_sub(fill_qty);
            if e.amount == 0 {
                entries.retain(|e| &e.order_id != order_id);
            }
        }
        None => return Err(perp_invariant_err(format!(
            "buy entry for order {:?} not found during fill update", order_id
        ))),
    }
    let new_reserved = calc_buy_side_margin_reserved(&entries, pos.leverage.max(1), market.base_decimals, pos.amount);
    let new_max = new_reserved.max(pos.sell_side_margin_reserved);
    let freed = old_max.saturating_sub(new_max);
    account.perp_wallet_balance = account.perp_wallet_balance.saturating_add(freed);
    pos.buy_side_margin_reserved = new_reserved;
    pos.margin_reserved = new_max;

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

    let old_max = pos.buy_side_margin_reserved.max(pos.sell_side_margin_reserved);
    match entries.iter_mut().find(|e| &e.order_id == order_id) {
        Some(e) => {
            e.amount = e.amount.saturating_sub(fill_qty);
            if e.amount == 0 {
                entries.retain(|e| &e.order_id != order_id);
            }
        }
        None => return Err(perp_invariant_err(format!(
            "sell entry for order {:?} not found during fill update", order_id
        ))),
    }
    let new_reserved = calc_sell_side_margin_reserved(&entries, pos.leverage.max(1), market.base_decimals, pos.amount);
    let new_max = pos.buy_side_margin_reserved.max(new_reserved);
    let freed = old_max.saturating_sub(new_max);
    account.perp_wallet_balance = account.perp_wallet_balance.saturating_add(freed);
    pos.sell_side_margin_reserved = new_reserved;
    pos.margin_reserved = new_max;

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

/// Returns `true` if the order can skip the matching engine entirely.
///
/// For PostOnly this also serves as the validity check — returns an error if the order
/// would immediately cross. Reads best_bid / best_ask at most once per call.
fn should_skip_match<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    side: Side,
    price: u64,
    order_type: OrderType,
    tif: TimeInForce,
) -> Result<bool, PrecompileError> {
    if order_type != OrderType::Limit {
        return Ok(false);
    }
    match tif {
        TimeInForce::PostOnly => {
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
            Ok(true)
        }
        TimeInForce::Gtc => {
            let no_match = match side {
                Side::Buy => {
                    let best_ask = storage::load_best_ask(context, market_id)?;
                    best_ask == 0 || best_ask > price
                }
                Side::Sell => {
                    let best_bid = storage::load_best_bid(context, market_id)?;
                    best_bid == 0 || best_bid < price
                }
            };
            Ok(no_match)
        }
        _ => Ok(false),
    }
}

// ── Cancel underfunded orders ─────────────────────────────────────────────────

/// Cancel orders on `cancel_side` (worst-priced first) until `shortfall` worth of reserved
/// margin has been freed.  Called after a fill that consumed wallet that was implicitly backing
/// the other side under the max-reservation scheme.
fn cancel_underfunded_side<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    cancel_side: Side,
    shortfall: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let mut remaining = shortfall;
    while remaining > 0 {
        // Pick the worst-priced order to cancel first (furthest from current market).
        // Buy entries are sorted price DESC → last = lowest price (worst buy).
        // Sell entries are sorted price ASC  → last = highest price (worst sell).
        let order_id = match cancel_side {
            Side::Buy => {
                let entries = storage::load_buy_orders(context, user, market_id)?;
                match entries.last() {
                    Some(e) => e.order_id,
                    None => break,
                }
            }
            Side::Sell => {
                let entries = storage::load_sell_orders(context, user, market_id)?;
                match entries.last() {
                    Some(e) => e.order_id,
                    None => break,
                }
            }
        };

        let order = match storage::load_order(context, &order_id)? {
            Some(o) if matches!(o.status, OrderStatus::Open | OrderStatus::PartiallyFilled) => o,
            _ => break,
        };

        let pos_before = storage::load_position(context, user, market_id)?;
        let old_max = pos_before.buy_side_margin_reserved.max(pos_before.sell_side_margin_reserved);

        remove_from_book(context, market_id, cancel_side, order.price, &order_id)?;
        let remaining_qty = order.quantity - order.filled;
        release_margin_for_cancelled_order(
            context, user, market_id, cancel_side, &order_id, remaining_qty, market,
        )?;

        let pos_after = storage::load_position(context, user, market_id)?;
        let new_max = pos_after.buy_side_margin_reserved.max(pos_after.sell_side_margin_reserved);
        let freed = old_max.saturating_sub(new_max);
        remaining = remaining.saturating_sub(freed);

        if let Some(mut o) = storage::load_order(context, &order_id)? {
            o.status = OrderStatus::Cancelled;
            storage::save_order(context, &order_id, &o)?;
        }
        context.journal_mut().log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::OrderCancelled {
                user,
                orderId: FixedBytes(order_id),
                marketId: market_id,
            }
            .to_log_data(),
        });
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolCall;
    use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
    use database::InMemoryDB;
    use primitives::{address, hardfork::SpecId, Address, U256};

    use crate::perp_dex::{
        interface::IPerpDex::{cancelOrderCall, getOrderCall, placeOrderCall},
        storage,
        types::{Market, OrderStatus, PerpPosition},
        PERP_DEX_ADDRESS, USDC_ADDRESS,
    };

    // ── Constants ──────────────────────────────────────────────────────────────

    const ALICE: Address = address!("1111111111111111111111111111111111111111");
    const BOB:   Address = address!("2222222222222222222222222222222222222222");
    const CAROL: Address = address!("3333333333333333333333333333333333333333");

    const MARKET_ID: u64 = 1;
    /// 1 tick = $1 in 9-decimal fixed-point (PRICE_ONE = 1e9).
    const TICK: u64 = 1_000_000_000;
    /// Test price: $100.
    const PRICE: u64 = 100 * TICK;
    /// Test quantity: 0.01 BTC (base_decimals = 8, so 1 BTC = 1e8 units).
    const QTY: u64 = 1_000_000;
    /// calc_value(PRICE, QTY, 8) = 1_000_000 (1 USDC in 6-decimal units).
    /// price * qty * 1e6 / (1e9 * 1e8) = 100e9 * 1e6 * 1e6 / 1e17 = 1e6.
    const FILL_VALUE: u64 = 1_000_000;
    /// Initial margin at leverage 1 = FILL_VALUE / 1.
    const INIT_MARGIN: u64 = FILL_VALUE;
    /// Starting perp wallet balance per user (10 USDC = 10_000_000 in 6-decimal units).
    const WALLET: u64 = 10_000_000;

    type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

    // ── Context helpers ────────────────────────────────────────────────────────

    fn make_ctx() -> TestCtx {
        let db = InMemoryDB::default();
        let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
        for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, CAROL] {
            JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
        }
        ctx
    }

    /// Register the default BTC-perp market and fund ALICE + BOB with WALLET.
    fn setup(ctx: &mut TestCtx) {
        storage::save_market(ctx, &Market {
            market_id:     MARKET_ID,
            base_decimals: 8,
            tick_size:     TICK,
            step_size:     QTY,
            min_quantity:  QTY,
            max_quantity:  QTY * 1_000,
            max_price:     PRICE * 1_000,
            active:        true,
        }).unwrap();
        fund(ctx, ALICE, WALLET);
        fund(ctx, BOB, WALLET);
    }

    /// Directly credit a user's perp wallet (bypasses deposit/transfer flow).
    fn fund(ctx: &mut TestCtx, user: Address, amount: u64) {
        let mut acc = storage::load_account(ctx, user).unwrap();
        acc.perp_wallet_balance += amount;
        storage::save_account(ctx, user, acc).unwrap();
    }

    fn wallet(ctx: &mut TestCtx, user: Address) -> u64 {
        storage::load_account(ctx, user).unwrap().perp_wallet_balance
    }

    fn pos(ctx: &mut TestCtx, user: Address) -> PerpPosition {
        storage::load_position(ctx, user, MARKET_ID).unwrap()
    }

    /// Place an order and return its 32-byte order ID.
    fn place(
        ctx: &mut TestCtx,
        caller: Address,
        side: u8,        // 0 = Buy,   1 = Sell
        price: u64,
        qty: u64,
        order_type: u8,  // 0 = Limit, 1 = Market
        tif: u8,         // 0 = GTC,   1 = IOC,  2 = FOK,  3 = PostOnly
    ) -> [u8; 32] {
        let input = placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: order_type,
            tif,
        }.abi_encode();
        let ret = run_place_order(&input, caller, ctx).unwrap();
        ret[..32].try_into().unwrap()
    }

    fn get_order(ctx: &mut TestCtx, id: [u8; 32]) -> crate::perp_dex::types::Order {
        storage::load_order(ctx, &id).unwrap().unwrap()
    }

    // ── Input validation ───────────────────────────────────────────────────────

    #[test]
    fn rejects_unknown_market() {
        let mut ctx = make_ctx();
        let input = placeOrderCall {
            marketId: 99, side: 0, price: PRICE, quantity: QTY, orderType: 0, tif: 0,
        }.abi_encode();
        let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("unknown market"), "{err}");
    }

    #[test]
    fn rejects_quantity_above_maximum() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let input = placeOrderCall {
            marketId: MARKET_ID, side: 0, price: PRICE, quantity: QTY * 1_001,
            orderType: 0, tif: 0,
        }.abi_encode();
        let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"), "{err}");
    }

    #[test]
    fn rejects_price_above_maximum() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let input = placeOrderCall {
            marketId: MARKET_ID, side: 0, price: PRICE * 1_001, quantity: QTY,
            orderType: 0, tif: 0,
        }.abi_encode();
        let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("exceeds maximum"), "{err}");
    }

    #[test]
    fn rejects_quantity_below_minimum() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let input = placeOrderCall {
            marketId: MARKET_ID, side: 0, price: PRICE, quantity: QTY / 2,
            orderType: 0, tif: 0,
        }.abi_encode();
        let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("below minimum"), "{err}");
    }

    #[test]
    fn rejects_quantity_not_multiple_of_step_size() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let input = placeOrderCall {
            marketId: MARKET_ID, side: 0, price: PRICE, quantity: QTY + 1,
            orderType: 0, tif: 0,
        }.abi_encode();
        let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("step_size"), "{err}");
    }

    #[test]
    fn rejects_limit_order_with_zero_price() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let input = placeOrderCall {
            marketId: MARKET_ID, side: 0, price: 0, quantity: QTY,
            orderType: 0, tif: 0,
        }.abi_encode();
        let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("limit order price must be > 0"), "{err}");
    }

    #[test]
    fn rejects_price_not_multiple_of_tick_size() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let input = placeOrderCall {
            marketId: MARKET_ID, side: 0, price: PRICE + 1, quantity: QTY,
            orderType: 0, tif: 0,
        }.abi_encode();
        let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("tick_size"), "{err}");
    }

    // ── Resting orders & margin reservation ───────────────────────────────────

    #[test]
    fn limit_buy_rests_in_book_when_no_ask() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // GTC limit buy
        assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open);
        assert_eq!(storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(), vec![PRICE]);
    }

    #[test]
    fn limit_sell_rests_in_book_when_no_bid() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let id = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // GTC limit sell
        assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open);
        assert_eq!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(), vec![PRICE]);
    }

    #[test]
    fn resting_buy_reserves_margin_from_perp_wallet() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

        // buy_side_margin_reserved = calc_value(PRICE, QTY, 8) / leverage(1) = FILL_VALUE
        assert_eq!(pos(&mut ctx, ALICE).buy_side_margin_reserved, INIT_MARGIN);
        assert_eq!(wallet(&mut ctx, ALICE), WALLET - INIT_MARGIN);
    }

    #[test]
    fn resting_sell_reserves_margin_from_perp_wallet() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);

        // sell_side_margin_reserved = calc_value(PRICE, QTY, 8) / leverage(1) = FILL_VALUE
        assert_eq!(pos(&mut ctx, BOB).sell_side_margin_reserved, INIT_MARGIN);
        assert_eq!(wallet(&mut ctx, BOB), WALLET - INIT_MARGIN);
    }

    #[test]
    fn consecutive_orders_have_unique_ids() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let id1 = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let id2 = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        assert_ne!(id1, id2);
    }

    // ── Matching ───────────────────────────────────────────────────────────────

    #[test]
    fn buy_taker_fully_matches_resting_ask() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let sell_id = place(&mut ctx, BOB,   1, PRICE, QTY, 0, 0); // resting ask
        let buy_id  = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // taker buy

        assert_eq!(get_order(&mut ctx, buy_id).status,  OrderStatus::Filled);
        assert_eq!(get_order(&mut ctx, sell_id).status, OrderStatus::Filled);
        assert!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
    }

    #[test]
    fn sell_taker_fully_matches_resting_bid() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let buy_id  = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // resting bid
        let sell_id = place(&mut ctx, BOB,   1, PRICE, QTY, 0, 0); // taker sell

        assert_eq!(get_order(&mut ctx, buy_id).status,  OrderStatus::Filled);
        assert_eq!(get_order(&mut ctx, sell_id).status, OrderStatus::Filled);
        assert!(storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
    }

    #[test]
    fn fill_opens_correct_long_and_short_positions() {
        // Bob rests a sell, Alice buys as taker.
        // After fill:
        //   Alice: long  QTY,  v_quote = -FILL_VALUE, margin = INIT_MARGIN
        //   Bob:   short QTY,  v_quote = +FILL_VALUE, margin = INIT_MARGIN
        let mut ctx = make_ctx();
        setup(&mut ctx);

        place(&mut ctx, BOB,   1, PRICE, QTY, 0, 0);
        place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

        let alice = pos(&mut ctx, ALICE);
        let bob   = pos(&mut ctx, BOB);

        assert_eq!(alice.amount,          QTY as i64);
        assert_eq!(alice.v_quote_balance, -(FILL_VALUE as i64));
        assert_eq!(alice.margin,          INIT_MARGIN as i64);

        assert_eq!(bob.amount,           -(QTY as i64));
        assert_eq!(bob.v_quote_balance,   FILL_VALUE as i64);
        assert_eq!(bob.margin,            INIT_MARGIN as i64);
    }

    #[test]
    fn fill_debits_init_margin_from_both_wallets() {
        // Maker reserves margin when resting; that reservation is released then
        // re-spent as initial margin on fill.  Net effect: both sides lose INIT_MARGIN.
        let mut ctx = make_ctx();
        setup(&mut ctx);

        place(&mut ctx, BOB,   1, PRICE, QTY, 0, 0); // resting ask
        place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // taker buy

        assert_eq!(wallet(&mut ctx, ALICE), WALLET - INIT_MARGIN);
        assert_eq!(wallet(&mut ctx, BOB),   WALLET - INIT_MARGIN);
    }

    #[test]
    fn partial_fill_leaves_maker_partially_filled_in_book() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let sell_id = place(&mut ctx, BOB,   1, PRICE, QTY * 2, 0, 0);
        let buy_id  = place(&mut ctx, ALICE, 0, PRICE, QTY,     0, 0);

        assert_eq!(get_order(&mut ctx, sell_id).status, OrderStatus::PartiallyFilled);
        assert_eq!(get_order(&mut ctx, sell_id).filled, QTY);
        assert_eq!(get_order(&mut ctx, buy_id).status,  OrderStatus::Filled);

        // Remaining sell still in ask book.
        assert_eq!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(), vec![PRICE]);
    }

    #[test]
    fn fifo_queue_fills_earlier_order_first() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        fund(&mut ctx, CAROL, WALLET);

        // Bob and Carol both rest sells at the same price.
        let bob_id   = place(&mut ctx, BOB,   1, PRICE, QTY, 0, 0);
        let carol_id = place(&mut ctx, CAROL, 1, PRICE, QTY, 0, 0);

        // Alice buys QTY — should hit Bob's order first (FIFO).
        place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

        assert_eq!(get_order(&mut ctx, bob_id).status,   OrderStatus::Filled);
        assert_eq!(get_order(&mut ctx, carol_id).status, OrderStatus::Open);
    }

    #[test]
    fn market_buy_matches_lowest_ask_first() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let low_price  = PRICE;
        let high_price = PRICE + 10 * TICK;

        let low_sell = place(&mut ctx, BOB, 1, low_price,  QTY, 0, 0);
        let _hi_sell = place(&mut ctx, BOB, 1, high_price, QTY, 0, 0);

        // Market IOC buy — should hit the lowest ask.
        let mkt_buy = place(&mut ctx, ALICE, 0, 0, QTY, 1, 1); // orderType=Market, tif=IOC

        assert_eq!(get_order(&mut ctx, low_sell).status, OrderStatus::Filled);
        assert_eq!(get_order(&mut ctx, mkt_buy).status,  OrderStatus::Filled);
        // High-price level must still be present.
        assert!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap().contains(&high_price));
    }

    // ── IOC ───────────────────────────────────────────────────────────────────

    #[test]
    fn ioc_with_no_liquidity_is_immediately_cancelled() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 1); // IOC, no asks
        let o = get_order(&mut ctx, id);
        assert_eq!(o.status, OrderStatus::Cancelled);
        assert_eq!(o.filled, 0);
    }

    #[test]
    fn ioc_partial_fill_cancels_remainder() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // only QTY available

        // IOC buy for 2×QTY: fills QTY, remainder cancelled.
        let id = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 1);
        let o = get_order(&mut ctx, id);
        assert_eq!(o.status, OrderStatus::Cancelled);
        assert_eq!(o.filled, QTY);
    }

    // ── FOK ───────────────────────────────────────────────────────────────────

    #[test]
    fn fok_rejected_when_insufficient_liquidity() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // only QTY available

        let input = placeOrderCall {
            marketId: MARKET_ID, side: 0, price: PRICE,
            quantity: QTY * 2, orderType: 0, tif: 2, // FOK
        }.abi_encode();
        let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("FOK order cannot be fully filled"), "{err}");
    }

    #[test]
    fn fok_fully_fills_when_sufficient_liquidity() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        // Bob places sell for 2×QTY; both wallets have enough for 2×INIT_MARGIN.
        let sell_id = place(&mut ctx, BOB,   1, PRICE, QTY * 2, 0, 0);
        let buy_id  = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 2); // FOK

        assert_eq!(get_order(&mut ctx, buy_id).status,  OrderStatus::Filled);
        assert_eq!(get_order(&mut ctx, sell_id).status, OrderStatus::Filled);
    }

    // ── PostOnly ──────────────────────────────────────────────────────────────

    #[test]
    fn post_only_rejected_if_would_immediately_match() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0); // resting bid at PRICE

        // PostOnly sell at PRICE would cross → rejected.
        let input = placeOrderCall {
            marketId: MARKET_ID, side: 1, price: PRICE,
            quantity: QTY, orderType: 0, tif: 3, // PostOnly
        }.abi_encode();
        let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("PostOnly order would match"), "{err}");
    }

    #[test]
    fn post_only_rests_when_above_best_bid() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0); // resting bid at PRICE

        // PostOnly sell one tick above the best bid — should rest.
        let ask_price = PRICE + TICK;
        let id = place(&mut ctx, ALICE, 1, ask_price, QTY, 0, 3); // PostOnly
        assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open);
        assert_eq!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(), vec![ask_price]);
    }

    // ── Cancel ────────────────────────────────────────────────────────────────

    #[test]
    fn cancel_resting_order_releases_margin_and_clears_book() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        assert!(wallet(&mut ctx, ALICE) < WALLET, "margin should be reserved");

        let input = cancelOrderCall { orderId: id.into() }.abi_encode();
        run_cancel_order(&input, ALICE, &mut ctx).unwrap();

        assert_eq!(wallet(&mut ctx, ALICE), WALLET, "margin should be returned");
        assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Cancelled);
        assert!(storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
    }

    #[test]
    fn cancel_rejects_non_owner() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let input = cancelOrderCall { orderId: id.into() }.abi_encode();
        let err = run_cancel_order(&input, BOB, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("not owner"), "{err}");
    }

    #[test]
    fn cancel_rejects_already_filled_order() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let sell_id = place(&mut ctx, BOB,   1, PRICE, QTY, 0, 0);
        place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // fills Bob's sell

        let input = cancelOrderCall { orderId: sell_id.into() }.abi_encode();
        let err = run_cancel_order(&input, BOB, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("not cancellable"), "{err}");
    }

    #[test]
    fn cancel_rejects_nonexistent_order() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let input = cancelOrderCall { orderId: [0xab_u8; 32].into() }.abi_encode();
        let err = run_cancel_order(&input, ALICE, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("order not found"), "{err}");
    }

    // ── getOrder ──────────────────────────────────────────────────────────────

    #[test]
    fn get_order_returns_all_correct_fields() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

        let input = getOrderCall { orderId: id.into() }.abi_encode();
        let ret = run_get_order(&input, &mut ctx).unwrap();

        // ABI layout: 7 × 32-byte slots.
        // address is right-aligned: leading 12 zero bytes then 20 address bytes.
        let owner     = Address::from_slice(&ret[12..32]);
        let market_id = U256::from_be_slice(&ret[32..64]).to::<u64>();
        let side      = U256::from_be_slice(&ret[64..96]).to::<u8>();
        let price     = U256::from_be_slice(&ret[96..128]).to::<u64>();
        let qty       = U256::from_be_slice(&ret[128..160]).to::<u64>();
        let filled    = U256::from_be_slice(&ret[160..192]).to::<u64>();
        let status    = U256::from_be_slice(&ret[192..224]).to::<u8>();

        assert_eq!(owner,     ALICE);
        assert_eq!(market_id, MARKET_ID);
        assert_eq!(side,      0u8);    // Buy
        assert_eq!(price,     PRICE);
        assert_eq!(qty,       QTY);
        assert_eq!(filled,    0u64);
        assert_eq!(status,    0u8);    // Open
    }

    #[test]
    fn get_order_rejects_nonexistent_order() {
        let mut ctx = make_ctx();
        setup(&mut ctx);

        let input = getOrderCall { orderId: [0u8; 32].into() }.abi_encode();
        let err = run_get_order(&input, &mut ctx).unwrap_err();
        assert!(err.to_string().contains("order not found"), "{err}");
    }
}

