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
        trading::{can_fully_liquidate_on_book, execute_liquidation_market_order},
        types::{Market, Side},
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
    let released_margin = pos.margin_reserved;
    if released_margin > 0 {
        let mut account = storage::load_account(context, user)?;
        account.perp_wallet_balance = account
            .perp_wallet_balance
            .checked_add(released_margin)
            .ok_or_else(|| perp_err("cancelAllOrders: wallet balance overflow"))?;
        storage::save_account(context, user, account)?;
    }
    pos.buy_side_margin_reserved = 0;
    pos.sell_side_margin_reserved = 0;
    pos.margin_reserved = 0;
    storage::save_position(context, user, market_id, &pos)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolCall;
    use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
    use database::InMemoryDB;
    use primitives::{address, hardfork::SpecId};

    use crate::perp_dex::{
        interface::IPerpDex::{liquidateCall, placeOrderCall},
        trading::run_place_order,
        types::{PerpPosition, UserAccount},
        USDC_ADDRESS,
    };

    const ALICE: Address = address!("1111111111111111111111111111111111111111");
    const KEEPER: Address = address!("2222222222222222222222222222222222222222");
    const MAKER: Address = address!("3333333333333333333333333333333333333333");
    const MARKET_ID: u64 = 1;
    const PRICE_DECIMALS: u32 = 2;
    const ENTRY_PRICE: u64 = 10_000; // $100.00
    const LONG_LIQ_PRICE: u64 = 9_000; // $90.00
    const SHORT_LIQ_PRICE: u64 = 11_000; // $110.00
    const QTY: i64 = 10;
    const ENTRY_VALUE: i64 = 1_000_000_000; // $1,000.00 in quote units.
    const MARGIN: i64 = 200_000_000; // $200.00 in quote units.
    const USER_WALLET: u64 = 50_000_000; // $50.00 in quote units.
    const MAKER_WALLET: u64 = 2_000_000_000; // $2,000.00 in quote units.

    type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

    fn make_ctx() -> TestCtx {
        let db = InMemoryDB::default();
        let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
        for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, KEEPER, MAKER] {
            JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
        }
        ctx
    }

    fn setup_market(ctx: &mut TestCtx) {
        storage::save_market(
            ctx,
            &Market {
                market_id: MARKET_ID,
                base_decimals: 0,
                price_decimals: PRICE_DECIMALS,
                tick_size: 1,
                step_size: 1,
                min_quantity: 1,
                max_quantity: 1_000_000,
                max_price: 1_000_000,
                active: true,
            },
        )
        .unwrap();
        storage::save_mark_price(ctx, MARKET_ID, ENTRY_PRICE).unwrap();
        storage::save_account(
            ctx,
            ALICE,
            UserAccount {
                perp_wallet_balance: USER_WALLET,
                ..UserAccount::default()
            },
        )
        .unwrap();
        storage::save_account(
            ctx,
            MAKER,
            UserAccount {
                perp_wallet_balance: MAKER_WALLET,
                ..UserAccount::default()
            },
        )
        .unwrap();
    }

    fn liquidate(ctx: &mut TestCtx, user: Address) -> Result<Bytes, PrecompileError> {
        let input = liquidateCall {
            user,
            marketId: MARKET_ID,
        }
        .abi_encode();
        run_liquidate(&input, KEEPER, ctx)
    }

    fn place_maker_order(ctx: &mut TestCtx, side: u8, price: u64, qty: u64) {
        let input = placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode();
        run_place_order(&input, MAKER, ctx).unwrap();
    }

    fn wallet(ctx: &mut TestCtx, user: Address) -> u64 {
        storage::load_account(ctx, user)
            .unwrap()
            .perp_wallet_balance
    }

    fn position(ctx: &mut TestCtx, user: Address) -> PerpPosition {
        storage::load_position(ctx, user, MARKET_ID).unwrap()
    }

    fn save_position(ctx: &mut TestCtx, amount: i64, v_quote_balance: i64) {
        storage::save_position(
            ctx,
            ALICE,
            MARKET_ID,
            &PerpPosition {
                amount,
                v_quote_balance,
                margin: MARGIN,
                leverage: 5,
                ..PerpPosition::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn liquidate_rejects_healthy_position() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);

        let err = liquidate(&mut ctx, ALICE).unwrap_err();
        assert!(err.to_string().contains("above maintenance margin"));
    }

    #[test]
    fn liquidate_long_sells_full_position_into_bids() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
        place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64);

        liquidate(&mut ctx, ALICE).unwrap();

        let alice = position(&mut ctx, ALICE);
        assert_eq!(alice.amount, 0);
        assert_eq!(alice.v_quote_balance, 0);
        assert_eq!(alice.margin, 0);
        assert_eq!(alice.leverage, 5);

        let maker = position(&mut ctx, MAKER);
        assert_eq!(maker.amount, QTY);
        assert_eq!(maker.v_quote_balance, -900_000_000);
        assert_eq!(maker.margin, 900_000_000);

        // Alice closes at $90: 200 margin - 100 unrealized loss = 100 USDC.
        assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET + 100_000_000);
        assert_eq!(wallet(&mut ctx, KEEPER), 0);
    }

    #[test]
    fn liquidate_short_buys_full_position_from_asks() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position(&mut ctx, -QTY, ENTRY_VALUE);
        storage::save_mark_price(&mut ctx, MARKET_ID, SHORT_LIQ_PRICE).unwrap();
        place_maker_order(&mut ctx, Side::Sell as u8, SHORT_LIQ_PRICE, QTY as u64);

        liquidate(&mut ctx, ALICE).unwrap();

        let alice = position(&mut ctx, ALICE);
        assert_eq!(alice.amount, 0);
        assert_eq!(alice.v_quote_balance, 0);
        assert_eq!(alice.margin, 0);

        let maker = position(&mut ctx, MAKER);
        assert_eq!(maker.amount, -QTY);
        assert_eq!(maker.v_quote_balance, 1_100_000_000);
        assert_eq!(maker.margin, 1_100_000_000);

        // Alice closes at $110: 200 margin - 100 unrealized loss = 100 USDC.
        assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET + 100_000_000);
        assert_eq!(wallet(&mut ctx, KEEPER), 0);
    }

    #[test]
    fn liquidate_rejects_when_orderbook_cannot_fully_close() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
        place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, (QTY as u64) - 1);

        let err = liquidate(&mut ctx, ALICE).unwrap_err();
        assert!(err.to_string().contains("cannot fully close position"));

        let alice = position(&mut ctx, ALICE);
        assert_eq!(alice.amount, QTY);
        assert_eq!(alice.v_quote_balance, -ENTRY_VALUE);
    }

    #[test]
    fn liquidate_refunds_reserved_margin_before_market_close() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        let mut pos = position(&mut ctx, ALICE);
        pos.margin_reserved = 33_000_000;
        pos.buy_side_margin_reserved = 33_000_000;
        storage::save_position(&mut ctx, ALICE, MARKET_ID, &pos).unwrap();
        storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
        place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64);

        liquidate(&mut ctx, ALICE).unwrap();

        assert_eq!(
            wallet(&mut ctx, ALICE),
            USER_WALLET + 33_000_000 + 100_000_000
        );
        assert_eq!(position(&mut ctx, ALICE).margin_reserved, 0);
    }
}
