use alloy_primitives::IntoLogData;
use context::{ContextTr, JournalTr};
use primitives::{Address, FixedBytes, Log};

use super::{match_order, next_order_id};
use crate::{
    perp_dex::{
        errors::{perp_err, perp_invariant_err},
        interface::IPerpDex,
        storage,
        types::{Market, Order, OrderStatus, OrderType, Side, TimeInForce},
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};
/// Return whether the opposite side of the book can fully close `user`'s
/// position, excluding the user's own orders that liquidation will cancel first.
pub(crate) fn can_fully_liquidate_on_book<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    liquidation_side: Side,
    quantity: u64,
) -> Result<bool, PrecompileError> {
    let mut available = 0u64;
    match liquidation_side {
        Side::Buy => {
            for price in storage::load_ask_prices(context, market_id)? {
                for order_id in storage::load_ask_level(context, market_id, price)? {
                    let Some(order) = storage::load_order(context, &order_id)? else {
                        return Err(perp_invariant_err(format!(
                            "ask queue references order {:?} not found in storage",
                            order_id
                        )));
                    };
                    if order.owner == user.0 .0 {
                        continue;
                    }
                    if !matches!(
                        order.status,
                        OrderStatus::Open | OrderStatus::PartiallyFilled
                    ) {
                        return Err(perp_invariant_err(format!(
                            "ask queue contains order {:?} with terminal status {:?}",
                            order_id, order.status
                        )));
                    }
                    available = available.saturating_add(order.quantity - order.filled);
                    if available >= quantity {
                        return Ok(true);
                    }
                }
            }
        }
        Side::Sell => {
            for price in storage::load_bid_prices(context, market_id)? {
                for order_id in storage::load_bid_level(context, market_id, price)? {
                    let Some(order) = storage::load_order(context, &order_id)? else {
                        return Err(perp_invariant_err(format!(
                            "bid queue references order {:?} not found in storage",
                            order_id
                        )));
                    };
                    if order.owner == user.0 .0 {
                        continue;
                    }
                    if !matches!(
                        order.status,
                        OrderStatus::Open | OrderStatus::PartiallyFilled
                    ) {
                        return Err(perp_invariant_err(format!(
                            "bid queue contains order {:?} with terminal status {:?}",
                            order_id, order.status
                        )));
                    }
                    available = available.saturating_add(order.quantity - order.filled);
                    if available >= quantity {
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

/// Execute the liquidation close as an internal market IOC order.
///
/// The caller must pre-check full book depth. This helper still rejects any
/// unfilled remainder to keep the first version all-or-nothing.
pub(crate) fn execute_liquidation_market_order<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market: &Market,
    side: Side,
    quantity: u64,
) -> Result<[u8; 32], PrecompileError> {
    let order_id = next_order_id(context, user)?;
    let order = Order {
        owner: user.0 .0,
        market_id: market.market_id,
        side,
        price: 0,
        quantity,
        filled: 0,
        order_type: OrderType::Market,
        tif: TimeInForce::Ioc,
        status: OrderStatus::Open,
    };
    storage::save_order(context, &order_id, &order)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderPlaced {
            user,
            marketId: market.market_id,
            orderId: FixedBytes(order_id),
            side: side as u8,
            price: 0,
            quantity,
            orderType: OrderType::Market as u8,
            tif: TimeInForce::Ioc as u8,
            clientOrderId: FixedBytes::default(),
        }
        .to_log_data(),
    });

    let remaining = match_order(
        context,
        user,
        &order_id,
        market.market_id,
        side,
        0,
        quantity,
        OrderType::Market,
        TimeInForce::Ioc,
        market,
    )?;
    if remaining != 0 {
        return Err(perp_err(
            "liquidate: orderbook liquidity cannot fully close position",
        ));
    }

    let mut pos = storage::load_position(context, user, market.market_id)?;
    if pos.amount != 0 {
        return Err(perp_invariant_err(
            "liquidation market order did not close position",
        ));
    }
    pos.v_quote_balance = 0;
    pos.margin = 0;
    storage::save_position(context, user, market.market_id, &pos)?;

    Ok(order_id)
}
