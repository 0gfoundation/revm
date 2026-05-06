//! Risk management: markets, leverage, mark price, positions, liquidation.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use context::{Block as BlockTr, ContextTr, JournalTr};
use ed25519_dalek::{Signature, VerifyingKey};
use primitives::{Address, Bytes, FixedBytes, Log};

use crate::{
    perp_dex::{
        errors::perp_err,
        interface::IPerpDex::{
            self, addMarketCall, addPositionMarginCall, getAdminCall, getMarkPriceCall,
            getMarketCall, getMarketReturn, getPositionCall, getPositionReturn, initAdminCall,
            liquidateCall, removePositionMarginCall, setLeverageCall, setLeverageSignedCall,
            setMarkPriceCall, transferAdminCall, updateMarketCall,
        },
        math::{calc_value, is_above_maintenance_margin},
        storage,
        trading::{
            can_fully_liquidate_on_book, check_api_key_expiry, check_recv_window,
            execute_liquidation_market_order, verify_ed25519,
        },
        types::{ApiKey, Market, Side},
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
pub fn run_set_leverage<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = setLeverageCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setLeverage: invalid calldata"))?;
    set_leverage_core(context, caller, args.marketId, args.leverage)
}

/// `setLeverageSigned(address account, uint64 marketId, uint64 leverage, uint64 timestamp, uint64 recvWindow, uint8 keyId, bytes signature)`
pub fn run_set_leverage_signed<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = setLeverageSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setLeverageSigned: invalid calldata"))?;

    let api_key = storage::load_api_key(context, args.account, args.keyId)?
        .ok_or_else(|| perp_err("setLeverageSigned: no api key registered for account"))?;

    check_recv_window(context, args.timestamp, args.recvWindow)
        .map_err(|e| perp_err(&format!("setLeverageSigned: {e}")))?;

    check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("setLeverageSigned: {e}")))?;

    // Canonical message (fixed-layout, 72 bytes):
    //   "perpdex_v1_leverage"(19) || account(20) || marketId(8) || leverage(8)
    //   || timestamp(8) || recvWindow(8) || keyId(1)
    let mut msg = [0u8; 72];
    msg[..19].copy_from_slice(b"perpdex_v1_leverage");
    msg[19..39].copy_from_slice(args.account.as_slice());
    msg[39..47].copy_from_slice(&args.marketId.to_be_bytes());
    msg[47..55].copy_from_slice(&args.leverage.to_be_bytes());
    msg[55..63].copy_from_slice(&args.timestamp.to_be_bytes());
    msg[63..71].copy_from_slice(&args.recvWindow.to_be_bytes());
    msg[71] = args.keyId;

    verify_ed25519(&api_key.pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(&format!("setLeverageSigned: {e}")))?;

    set_leverage_core(context, args.account, args.marketId, args.leverage)
}

fn set_leverage_core<CTX: ContextTr>(
    context: &mut CTX,
    account: Address,
    market_id: u64,
    leverage: u64,
) -> Result<Bytes, PrecompileError> {
    if leverage == 0 || leverage > 20 {
        return Err(perp_err("setLeverage: leverage must be 1–20"));
    }
    storage::load_market(context, market_id)?
        .ok_or_else(|| perp_err("setLeverage: unknown market"))?;

    let mut pos = storage::load_position(context, account, market_id)?;
    let old_leverage = pos.leverage.max(1);
    if pos.amount != 0 && leverage < old_leverage {
        return Err(perp_err(
            "setLeverage: cannot reduce leverage with open position",
        ));
    }

    rebalance_order_margin_for_leverage(context, account, &mut pos, leverage)?;
    pos.leverage = leverage;
    storage::save_position(context, account, market_id, &pos)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::LeverageChanged {
            user: account,
            marketId: market_id,
            leverage,
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

/// `addPositionMargin(uint64 marketId, uint64 amount)`
pub fn run_add_position_margin<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = addPositionMarginCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("addPositionMargin: invalid calldata"))?;
    if args.amount == 0 {
        return Err(perp_err("addPositionMargin: amount must be > 0"));
    }
    if args.amount > i64::MAX as u64 {
        return Err(perp_err("addPositionMargin: amount exceeds i64::MAX"));
    }
    storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("addPositionMargin: unknown market"))?;

    let mut pos = storage::load_position(context, caller, args.marketId)?;
    if pos.amount == 0 {
        return Err(perp_err("addPositionMargin: no open position"));
    }
    let mut account = storage::load_account(context, caller)?;
    if account.perp_wallet_balance < args.amount {
        return Err(perp_err(
            "addPositionMargin: insufficient perp wallet balance",
        ));
    }

    account.perp_wallet_balance -= args.amount;
    pos.margin = pos
        .margin
        .checked_add(args.amount as i64)
        .ok_or_else(|| perp_err("addPositionMargin: margin overflow"))?;

    storage::save_account(context, caller, account)?;
    storage::save_position(context, caller, args.marketId, &pos)?;
    emit_position_margin_adjusted(context, caller, args.marketId, args.amount as i64, &pos);
    emit_position_changed(context, caller, args.marketId, &pos);
    Ok(Bytes::new())
}

/// `removePositionMargin(uint64 marketId, uint64 amount)`
pub fn run_remove_position_margin<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = removePositionMarginCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("removePositionMargin: invalid calldata"))?;
    if args.amount == 0 {
        return Err(perp_err("removePositionMargin: amount must be > 0"));
    }
    if args.amount > i64::MAX as u64 {
        return Err(perp_err("removePositionMargin: amount exceeds i64::MAX"));
    }
    let market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("removePositionMargin: unknown market"))?;
    let mark_price = storage::load_mark_price(context, args.marketId)?;

    let mut pos = storage::load_position(context, caller, args.marketId)?;
    if pos.amount == 0 {
        return Err(perp_err("removePositionMargin: no open position"));
    }
    if pos.margin < args.amount as i64 {
        return Err(perp_err(
            "removePositionMargin: insufficient position margin",
        ));
    }
    let new_margin = pos.margin - args.amount as i64;
    let required_initial_margin = calc_value(
        mark_price,
        pos.amount.unsigned_abs(),
        market.base_decimals,
        market.price_decimals,
    )? / pos.leverage.max(1);
    if new_margin < required_initial_margin as i64 {
        return Err(perp_err(
            "removePositionMargin: resulting margin below initial margin requirement",
        ));
    }

    let mut account = storage::load_account(context, caller)?;
    account.perp_wallet_balance = account
        .perp_wallet_balance
        .checked_add(args.amount)
        .ok_or_else(|| perp_err("removePositionMargin: wallet balance overflow"))?;
    pos.margin = new_margin;

    storage::save_account(context, caller, account)?;
    storage::save_position(context, caller, args.marketId, &pos)?;
    emit_position_margin_adjusted(context, caller, args.marketId, -(args.amount as i64), &pos);
    emit_position_changed(context, caller, args.marketId, &pos);
    Ok(Bytes::new())
}
/// `liquidate(address user, uint64 marketId)`
///
/// Anyone can call this to liquidate an under-margined position.
/// Liquidation closes the position through the order book as an internal market order.
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
    let liquidation_side = if pos.amount > 0 {
        Side::Sell
    } else {
        Side::Buy
    };
    let liquidation_quantity = pos.amount.unsigned_abs();

    if !can_fully_liquidate_on_book(
        context,
        args.user,
        args.marketId,
        liquidation_side,
        liquidation_quantity,
    )? {
        return Err(perp_err(
            "liquidate: orderbook liquidity cannot fully close position",
        ));
    }

    // Cancel all open orders for this user/market (emits OrderCancelled events).
    cancel_all_orders_for_market(context, args.user, args.marketId, &market)?;

    execute_liquidation_market_order(
        context,
        args.user,
        &market,
        liquidation_side,
        liquidation_quantity,
    )?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Liquidation {
            user: args.user,
            marketId: args.marketId,
            liquidator: caller,
            amount: liq_amount,
            reward: 0,
            markPrice: mark_price,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn rebalance_order_margin_for_leverage<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    pos: &mut crate::perp_dex::types::PerpPosition,
    new_leverage: u64,
) -> Result<(), PrecompileError> {
    let new_buy_reserved = pos.buy_side_reserved_notional / new_leverage;
    let new_sell_reserved = pos.sell_side_reserved_notional / new_leverage;
    let old_reserved = pos.margin_reserved;
    let new_reserved = new_buy_reserved.max(new_sell_reserved);

    if new_reserved > old_reserved {
        let delta = new_reserved - old_reserved;
        let mut account = storage::load_account(context, user)?;
        if account.perp_wallet_balance < delta {
            return Err(perp_err(
                "setLeverage: insufficient perp wallet for order margin",
            ));
        }
        account.perp_wallet_balance -= delta;
        storage::save_account(context, user, account)?;
    } else if old_reserved > new_reserved {
        let delta = old_reserved - new_reserved;
        let mut account = storage::load_account(context, user)?;
        account.perp_wallet_balance = account
            .perp_wallet_balance
            .checked_add(delta)
            .ok_or_else(|| perp_err("setLeverage: wallet balance overflow"))?;
        storage::save_account(context, user, account)?;
    }

    pos.buy_side_margin_reserved = new_buy_reserved;
    pos.sell_side_margin_reserved = new_sell_reserved;
    pos.margin_reserved_notional = pos
        .buy_side_reserved_notional
        .max(pos.sell_side_reserved_notional);
    pos.margin_reserved = new_reserved;
    Ok(())
}

fn emit_position_changed<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    pos: &crate::perp_dex::types::PerpPosition,
) {
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user,
            marketId: market_id,
            amount: pos.amount,
            vQuoteBalance: pos.v_quote_balance,
            margin: pos.margin,
            leverage: pos.leverage,
        }
        .to_log_data(),
    });
}

fn emit_position_margin_adjusted<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    delta: i64,
    pos: &crate::perp_dex::types::PerpPosition,
) {
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionMarginAdjusted {
            user,
            marketId: market_id,
            delta,
            margin: pos.margin,
        }
        .to_log_data(),
    });
}

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

    // Recalculate reserves (now 0 since all orders cancelled).
    let mut pos = storage::load_position(context, user, market_id)?;
    let released = pos
        .margin_reserved
        .checked_add(pos.fee_reserved)
        .ok_or_else(|| perp_err("cancelAllOrders: released reserve overflow"))?;
    if released > 0 {
        let mut account = storage::load_account(context, user)?;
        account.perp_wallet_balance = account
            .perp_wallet_balance
            .checked_add(released)
            .ok_or_else(|| perp_err("cancelAllOrders: wallet balance overflow"))?;
        storage::save_account(context, user, account)?;
    }
    pos.buy_side_margin_reserved = 0;
    pos.buy_side_reserved_notional = 0;
    pos.sell_side_margin_reserved = 0;
    pos.sell_side_reserved_notional = 0;
    pos.margin_reserved = 0;
    pos.margin_reserved_notional = 0;
    pos.fee_reserved = 0;
    storage::save_position(context, user, market_id, &pos)?;

    Ok(())
}

#[cfg(test)]
mod tests;
