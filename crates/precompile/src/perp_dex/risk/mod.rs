//! Risk management: markets, leverage, mark price, positions, liquidation.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use context::{ContextTr, JournalTr};
use primitives::{Address, Bytes, FixedBytes, Log};

use crate::{
    perp_dex::{
        errors::perp_err,
        interface::IPerpDex::{
            self, addMarketCall, getAdminCall, getMarkPriceCall, getMarketCall, getMarketReturn,
            getPositionCall, getPositionReturn, initAdminCall, liquidateCall, setLeverageCall,
            setMarkPriceCall, transferAdminCall, updateMarketCall,
        },
        math::{calc_value, is_above_maintenance_margin},
        storage,
        types::{Market, PerpPosition},
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};

// ── Admin: ownership ──────────────────────────────────────────────────────────

/// `initAdmin(address admin)` — one-time initialisation; fails if already set.
pub fn run_init_admin<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = initAdminCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("initAdmin: invalid calldata"))?;

    if args.admin == Address::ZERO {
        return Err(perp_err("initAdmin: admin cannot be zero address"));
    }
    let current = storage::load_admin(context)?;
    if current != Address::ZERO {
        return Err(perp_err("initAdmin: admin already initialised"));
    }
    storage::save_admin(context, args.admin)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AdminInitialized { admin: args.admin }.to_log_data(),
    });
    Ok(Bytes::new())
}

/// `transferAdmin(address newAdmin)` — only callable by current admin.
pub fn run_transfer_admin<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = transferAdminCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("transferAdmin: invalid calldata"))?;

    if args.newAdmin == Address::ZERO {
        return Err(perp_err("transferAdmin: new admin cannot be zero address"));
    }
    let current = storage::load_admin(context)?;
    if current == Address::ZERO {
        return Err(perp_err("transferAdmin: admin not yet initialised"));
    }
    if caller != current {
        return Err(perp_err("transferAdmin: caller is not admin"));
    }
    storage::save_admin(context, args.newAdmin)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AdminTransferred {
            previousAdmin: current,
            newAdmin: args.newAdmin,
        }
        .to_log_data(),
    });
    Ok(Bytes::new())
}

/// `getAdmin() returns (address admin)`
pub fn run_get_admin<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    getAdminCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAdmin: invalid calldata"))?;
    let admin = storage::load_admin(context)?;
    Ok(Bytes::from(getAdminCall::abi_encode_returns(&admin)))
}

// ── Admin: market management ──────────────────────────────────────────────────

/// `addMarket(uint64 marketId, uint32 baseDecimals, uint32 priceDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice)`
pub fn run_add_market<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = addMarketCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("addMarket: invalid calldata"))?;

    require_admin(caller, context)?;

    if storage::load_market(context, args.marketId)?.is_some() {
        return Err(perp_err("addMarket: market already exists"));
    }
    if args.priceDecimals > 18 {
        return Err(perp_err("addMarket: priceDecimals must be <= 18"));
    }
    validate_market_bounds(
        "addMarket",
        args.baseDecimals,
        args.priceDecimals,
        args.maxQuantity,
        args.maxPrice,
    )?;
    if args.tickSize == 0 || args.stepSize == 0 || args.minQuantity == 0 {
        return Err(perp_err("addMarket: tick/step/min must be > 0"));
    }
    if args.maxQuantity < args.minQuantity {
        return Err(perp_err("addMarket: maxQuantity must be >= minQuantity"));
    }
    if args.maxQuantity % args.stepSize != 0 {
        return Err(perp_err(
            "addMarket: maxQuantity must be a multiple of stepSize",
        ));
    }
    if args.maxPrice < args.tickSize {
        return Err(perp_err("addMarket: maxPrice must be >= tickSize"));
    }
    if args.maxPrice % args.tickSize != 0 {
        return Err(perp_err(
            "addMarket: maxPrice must be a multiple of tickSize",
        ));
    }

    let market = Market {
        market_id: args.marketId,
        base_decimals: args.baseDecimals,
        price_decimals: args.priceDecimals,
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
            priceDecimals: args.priceDecimals,
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

/// `updateMarket(uint64 marketId, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, bool active)`
pub fn run_update_market<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = updateMarketCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("updateMarket: invalid calldata"))?;

    require_admin(caller, context)?;

    let mut market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("updateMarket: unknown market"))?;

    validate_market_bounds(
        "updateMarket",
        market.base_decimals,
        market.price_decimals,
        args.maxQuantity,
        args.maxPrice,
    )?;
    if args.tickSize == 0 || args.stepSize == 0 || args.minQuantity == 0 {
        return Err(perp_err("updateMarket: tick/step/min must be > 0"));
    }
    if args.maxQuantity < args.minQuantity {
        return Err(perp_err("updateMarket: maxQuantity must be >= minQuantity"));
    }
    if args.maxQuantity % args.stepSize != 0 {
        return Err(perp_err(
            "updateMarket: maxQuantity must be a multiple of stepSize",
        ));
    }
    if args.maxPrice < args.tickSize {
        return Err(perp_err("updateMarket: maxPrice must be >= tickSize"));
    }
    if args.maxPrice % args.tickSize != 0 {
        return Err(perp_err(
            "updateMarket: maxPrice must be a multiple of tickSize",
        ));
    }

    market.tick_size = args.tickSize;
    market.step_size = args.stepSize;
    market.min_quantity = args.minQuantity;
    market.max_quantity = args.maxQuantity;
    market.max_price = args.maxPrice;
    market.active = args.active;
    storage::save_market(context, &market)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarketUpdated {
            marketId: args.marketId,
            tickSize: args.tickSize,
            stepSize: args.stepSize,
            minQuantity: args.minQuantity,
            maxQuantity: args.maxQuantity,
            maxPrice: args.maxPrice,
            active: args.active,
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

    require_admin(caller, context)?;

    let market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("setMarkPrice: unknown market"))?;
    if args.price > market.max_price {
        return Err(perp_err("setMarkPrice: price exceeds maximum"));
    }
    if market.tick_size > 0 && args.price % market.tick_size != 0 {
        return Err(perp_err("setMarkPrice: price not multiple of tick_size"));
    }
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

/// `getMarket(uint64 marketId) returns (uint32 baseDecimals, uint32 priceDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, bool active)`
pub fn run_get_market<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getMarketCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarket: invalid calldata"))?;

    let market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("getMarket: unknown market"))?;

    Ok(Bytes::from(getMarketCall::abi_encode_returns(
        &getMarketReturn {
            baseDecimals: market.base_decimals,
            priceDecimals: market.price_decimals,
            tickSize: market.tick_size,
            stepSize: market.step_size,
            minQuantity: market.min_quantity,
            maxQuantity: market.max_quantity,
            maxPrice: market.max_price,
            active: market.active,
        },
    )))
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
        return Err(perp_err(
            "setLeverage: cannot change leverage with open position",
        ));
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
        market.price_decimals,
    )? {
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
        liq_account.perp_wallet_balance = liq_account.perp_wallet_balance.saturating_add(reward);
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

fn require_admin<CTX: ContextTr>(
    caller: Address,
    context: &mut CTX,
) -> Result<(), PrecompileError> {
    let admin = storage::load_admin(context)?;
    if admin == Address::ZERO {
        return Err(perp_err("not authorised: admin not initialised"));
    }
    if caller != admin {
        return Err(perp_err("not authorised: caller is not admin"));
    }
    Ok(())
}

fn validate_market_bounds(
    prefix: &str,
    base_decimals: u32,
    price_decimals: u32,
    max_quantity: u64,
    max_price: u64,
) -> Result<(), PrecompileError> {
    if base_decimals > 18 {
        return Err(perp_err(format!("{prefix}: baseDecimals must be <= 18")));
    }
    if price_decimals > 18 {
        return Err(perp_err(format!("{prefix}: priceDecimals must be <= 18")));
    }
    if max_quantity > i64::MAX as u64 {
        return Err(perp_err(format!(
            "{prefix}: maxQuantity must be <= i64::MAX"
        )));
    }
    let max_value =
        calc_value(max_price, max_quantity, base_decimals, price_decimals).map_err(|_| {
            perp_err(format!(
                "{prefix}: maxPrice * maxQuantity exceeds numeric limits"
            ))
        })?;
    if max_value > i64::MAX as u64 {
        return Err(perp_err(format!(
            "{prefix}: max order value must be <= i64::MAX quote units"
        )));
    }
    Ok(())
}

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
