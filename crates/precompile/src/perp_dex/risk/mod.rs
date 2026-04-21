//! Risk management: markets, leverage, mark price, positions, liquidation.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use context::{ContextTr, JournalTr};
use primitives::{Address, Bytes, FixedBytes, Log};

use crate::{
    perp_dex::{
        errors::perp_err,
        interface::IPerpDex::{
            self, addMarketCall, getMarkPriceCall, getPositionCall, getPositionReturn,
            liquidateCall, setLeverageCall, setMarkPriceCall,
        },
        math::is_above_maintenance_margin,
        storage,
        types::{Market, PerpPosition},
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};

// ── Admin: market management ──────────────────────────────────────────────────

/// `addMarket(uint64 marketId, uint32 baseDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity)`
pub fn run_add_market<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = addMarketCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("addMarket: invalid calldata"))?;

    if storage::load_market(context, args.marketId)?.is_some() {
        return Err(perp_err("addMarket: market already exists"));
    }
    if args.tickSize == 0 || args.stepSize == 0 || args.minQuantity == 0 {
        return Err(perp_err("addMarket: tick/step/min must be > 0"));
    }
    if args.maxQuantity < args.minQuantity {
        return Err(perp_err("addMarket: maxQuantity must be >= minQuantity"));
    }
    if args.maxQuantity % args.stepSize != 0 {
        return Err(perp_err("addMarket: maxQuantity must be a multiple of stepSize"));
    }
    if args.maxPrice < args.tickSize {
        return Err(perp_err("addMarket: maxPrice must be >= tickSize"));
    }
    if args.maxPrice % args.tickSize != 0 {
        return Err(perp_err("addMarket: maxPrice must be a multiple of tickSize"));
    }

    let market = Market {
        market_id: args.marketId,
        base_decimals: args.baseDecimals,
        tick_size: args.tickSize,
        step_size: args.stepSize,
        min_quantity: args.minQuantity,
        max_quantity: args.maxQuantity,
        max_price: args.maxPrice,
        active: true,
    };
    storage::save_market(context, &market)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarketAdded {
            marketId: args.marketId,
            baseDecimals: args.baseDecimals,
            tickSize: args.tickSize,
            stepSize: args.stepSize,
            minQuantity: args.minQuantity,
            maxQuantity: args.maxQuantity,
            maxPrice: args.maxPrice,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `setMarkPrice(uint64 marketId, uint64 price)`
pub fn run_set_mark_price<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = setMarkPriceCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setMarkPrice: invalid calldata"))?;

    storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("setMarkPrice: unknown market"))?;
    storage::save_mark_price(context, args.marketId, args.price)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarkPriceUpdated {
            marketId: args.marketId,
            price: args.price,
            updater: caller,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `getMarkPrice(uint64 marketId) returns (uint64 price)`
pub fn run_get_mark_price<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getMarkPriceCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarkPrice: invalid calldata"))?;

    let price = storage::load_mark_price(context, args.marketId)?;
    Ok(Bytes::from(getMarkPriceCall::abi_encode_returns(&price)))
}

// ── Leverage ──────────────────────────────────────────────────────────────────

/// `setLeverage(uint64 marketId, uint64 leverage)`
///
/// Only allowed when the user has no open position in the market.
pub fn run_set_leverage<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = setLeverageCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setLeverage: invalid calldata"))?;

    if args.leverage == 0 || args.leverage > 20 {
        return Err(perp_err("setLeverage: leverage must be 1–20"));
    }
    storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("setLeverage: unknown market"))?;

    let mut pos = storage::load_position(context, caller, args.marketId)?;
    if pos.amount != 0 {
        return Err(perp_err("setLeverage: cannot change leverage with open position"));
    }
    pos.leverage = args.leverage;
    storage::save_position(context, caller, args.marketId, &pos)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::LeverageChanged {
            user: caller,
            marketId: args.marketId,
            leverage: args.leverage,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

// ── Positions ─────────────────────────────────────────────────────────────────

/// `getPosition(address user, uint64 marketId) returns (int64 amount, int64 vQuoteBalance, int64 margin, uint64 marginReserved, uint64 leverage)`
pub fn run_get_position<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getPositionCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getPosition: invalid calldata"))?;

    let pos = storage::load_position(context, args.user, args.marketId)?;
    Ok(Bytes::from(getPositionCall::abi_encode_returns(
        &getPositionReturn {
            amount: pos.amount,
            vQuoteBalance: pos.v_quote_balance,
            margin: pos.margin,
            marginReserved: pos.margin_reserved,
            leverage: pos.leverage,
        },
    )))
}

// ── Liquidation ───────────────────────────────────────────────────────────────

/// `liquidate(address user, uint64 marketId)`
///
/// Anyone can call this to liquidate an under-margined position.
/// Liquidation zeroes the position and credits the liquidator's perp wallet.
pub fn run_liquidate<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = liquidateCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("liquidate: invalid calldata"))?;

    let market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("liquidate: unknown market"))?;
    let mark_price = storage::load_mark_price(context, args.marketId)?;
    let pos = storage::load_position(context, args.user, args.marketId)?;

    if pos.amount == 0 {
        return Err(perp_err("liquidate: no open position"));
    }
    if is_above_maintenance_margin(
        mark_price,
        pos.amount,
        pos.v_quote_balance,
        pos.margin,
        market.base_decimals,
    ) {
        return Err(perp_err("liquidate: position is above maintenance margin"));
    }

    let liq_amount = pos.amount;

    // Cancel all open orders for this user/market (emits OrderCancelled events).
    cancel_all_orders_for_market(context, args.user, args.marketId, &market)?;

    // The remaining margin after zeroing the position goes to the liquidator
    // as a liquidation reward. Any deficit is socialised (absorbed silently).
    let reward = pos.margin.max(0) as u64;

    // Zero out position.
    let zeroed = PerpPosition {
        leverage: pos.leverage, // keep leverage setting
        ..PerpPosition::default()
    };
    storage::save_position(context, args.user, args.marketId, &zeroed)?;

    // Credit liquidator.
    if reward > 0 && caller != args.user {
        let mut liq_account = storage::load_account(context, caller)?;
        liq_account.perp_wallet_balance = liq_account
            .perp_wallet_balance
            .saturating_add(reward);
        storage::save_account(context, caller, liq_account)?;
    }

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Liquidation {
            user: args.user,
            marketId: args.marketId,
            liquidator: caller,
            amount: liq_amount,
            reward,
            markPrice: mark_price,
        }
        .to_log_data(),
    });

    // Emit zeroed position state.
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user: args.user,
            marketId: args.marketId,
            amount: 0,
            vQuoteBalance: 0,
            margin: 0,
            leverage: pos.leverage,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Cancel every open order for `user` in `market`, returning reserved margin
/// back to their perp wallet.
pub(crate) fn cancel_all_orders_for_market<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    _market: &Market,
) -> Result<(), PrecompileError> {
    use crate::perp_dex::types::OrderStatus;

    // --- Buy orders ---
    let buy_entries = storage::load_buy_orders(context, user, market_id)?;
    for entry in &buy_entries {
        if let Some(mut order) = storage::load_order(context, &entry.order_id)? {
            order.status = OrderStatus::Cancelled;
            storage::save_order(context, &entry.order_id, &order)?;
        }
        // Remove from price level queue.
        let mut queue = storage::load_bid_level(context, market_id, entry.price)?;
        queue.retain(|id| id != &entry.order_id);
        if queue.is_empty() {
            storage::remove_bid_price(context, market_id, entry.price)?;
        }
        storage::save_bid_level(context, market_id, entry.price, &queue)?;

        context.journal_mut().log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::OrderCancelled {
                user,
                orderId: FixedBytes(entry.order_id),
                marketId: market_id,
            }
            .to_log_data(),
        });
    }
    storage::save_buy_orders(context, user, market_id, &[])?;

    // --- Sell orders ---
    let sell_entries = storage::load_sell_orders(context, user, market_id)?;
    for entry in &sell_entries {
        if let Some(mut order) = storage::load_order(context, &entry.order_id)? {
            order.status = OrderStatus::Cancelled;
            storage::save_order(context, &entry.order_id, &order)?;
        }
        let mut queue = storage::load_ask_level(context, market_id, entry.price)?;
        queue.retain(|id| id != &entry.order_id);
        if queue.is_empty() {
            storage::remove_ask_price(context, market_id, entry.price)?;
        }
        storage::save_ask_level(context, market_id, entry.price, &queue)?;

        context.journal_mut().log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::OrderCancelled {
                user,
                orderId: FixedBytes(entry.order_id),
                marketId: market_id,
            }
            .to_log_data(),
        });
    }
    storage::save_sell_orders(context, user, market_id, &[])?;

    // Recalculate margin reserved (now 0 since all orders cancelled).
    let mut pos = storage::load_position(context, user, market_id)?;
    pos.buy_side_margin_reserved = 0;
    pos.sell_side_margin_reserved = 0;
    pos.margin_reserved = 0;
    storage::save_position(context, user, market_id, &pos)?;

    Ok(())
}
