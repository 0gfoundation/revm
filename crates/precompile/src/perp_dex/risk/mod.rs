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
            self, addMarketCall, addPositionMarginCall, depositInsuranceFundCall,
            getAdminCall, getAveragePremiumIndexCall, getAveragePremiumIndexReturn,
            getFundingStateCall, getFundingStateReturn, getIndexPriceCall, getIndexPriceReturn,
            getInsuranceFundCall, getMarkPriceCall, getMarketCall, getMarketReturn,
            getMarketManagerAddressCall, getOracleAddressCall, getPositionCall, getPositionReturn,
            initAdminCall, liquidateCall, removePositionMarginCall, setLeverageCall,
            setLeverageSignedCall, setMarketManagerAddressCall, setOracleAddressCall,
            transferAdminCall, updateIndexPriceCall, updateMarketCall, withdrawInsuranceFundCall,
        },
        math::{
            calc_funding_rate, calc_value, checked_u64_to_i64, is_above_maintenance_margin,
            FUNDING_RATE_ONE,
        },
        storage,
        trading::{
            check_api_key_expiry, check_recv_window, execute_liquidation_market_order,
            settle_liquidation_residual_at_mark_price, verify_ed25519,
        },
        types::{FundingState, IndexPriceState, Market, PremiumIndexAccumulator, Side},
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

/// `addMarket(uint64 marketId, uint32 baseDecimals, uint32 priceDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, uint64 fundingInterval, int64 interestRate)`
pub fn run_add_market<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = addMarketCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("addMarket: invalid calldata"))?;

    require_admin_or_market_manager(caller, context)?;

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
    if args.priceUpdateInterval == 0 {
        return Err(perp_err("addMarket: priceUpdateInterval must be > 0"));
    }
    validate_funding_config("addMarket", args.priceUpdateInterval, args.fundingInterval)?;
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
        price_update_interval: args.priceUpdateInterval,
        active: true,
        funding_interval: args.fundingInterval,
        interest_rate: args.interestRate,
        liquidation_fee_rate_bps: args.liquidationFeeRateBps,
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
            priceUpdateInterval: args.priceUpdateInterval,
            fundingInterval: args.fundingInterval,
            interestRate: args.interestRate,
            liquidationFeeRateBps: args.liquidationFeeRateBps,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `updateMarket(uint64 marketId, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, bool active, uint64 fundingInterval, int64 interestRate)`
pub fn run_update_market<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = updateMarketCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("updateMarket: invalid calldata"))?;

    require_admin_or_market_manager(caller, context)?;

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
    if args.priceUpdateInterval == 0 {
        return Err(perp_err("updateMarket: priceUpdateInterval must be > 0"));
    }
    validate_funding_config(
        "updateMarket",
        args.priceUpdateInterval,
        args.fundingInterval,
    )?;
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
    market.price_update_interval = args.priceUpdateInterval;
    market.active = args.active;
    market.funding_interval = args.fundingInterval;
    market.interest_rate = args.interestRate;
    market.liquidation_fee_rate_bps = args.liquidationFeeRateBps;
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
            priceUpdateInterval: args.priceUpdateInterval,
            active: args.active,
            fundingInterval: args.fundingInterval,
            interestRate: args.interestRate,
            liquidationFeeRateBps: args.liquidationFeeRateBps,
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

/// `getMarket(uint64 marketId) returns (uint32 baseDecimals, uint32 priceDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, bool active)`
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
            priceUpdateInterval: market.price_update_interval,
            active: market.active,
            fundingInterval: market.funding_interval,
            interestRate: market.interest_rate,
            liquidationFeeRateBps: market.liquidation_fee_rate_bps,
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
            feeReserved: pos.fee_reserved,
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
    let amount = checked_u64_to_i64(args.amount, "addPositionMargin: amount")?;
    storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("addPositionMargin: unknown market"))?;

    let mut pos = storage::load_position(context, caller, args.marketId)?;
    if pos.amount == 0 {
        return Err(perp_err("addPositionMargin: no open position"));
    }
    let mut account = storage::load_account(context, caller)?;
    if !account.has_available_perp(args.amount) {
        return Err(perp_err(
            "addPositionMargin: insufficient perp wallet balance",
        ));
    }

    account.debit_perp(args.amount)?;
    pos.margin = pos
        .margin
        .checked_add(amount)
        .ok_or_else(|| perp_err("addPositionMargin: margin overflow"))?;

    storage::save_account(context, caller, account)?;
    storage::save_position(context, caller, args.marketId, &pos)?;
    emit_position_margin_adjusted(context, caller, args.marketId, amount, &pos);
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
    let amount = checked_u64_to_i64(args.amount, "removePositionMargin: amount")?;
    let market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("removePositionMargin: unknown market"))?;
    let mark_price = storage::load_mark_price(context, args.marketId)?;

    let mut pos = storage::load_position(context, caller, args.marketId)?;
    if pos.amount == 0 {
        return Err(perp_err("removePositionMargin: no open position"));
    }
    if pos.margin < amount {
        return Err(perp_err(
            "removePositionMargin: insufficient position margin",
        ));
    }
    let new_margin = pos.margin - amount;
    let required_initial_margin = calc_value(
        mark_price,
        pos.amount.unsigned_abs(),
        market.base_decimals,
        market.price_decimals,
    )? / pos.leverage.max(1);
    let required_initial_margin = checked_u64_to_i64(
        required_initial_margin,
        "removePositionMargin: initial margin",
    )?;
    if new_margin < required_initial_margin {
        return Err(perp_err(
            "removePositionMargin: resulting margin below initial margin requirement",
        ));
    }

    let mut account = storage::load_account(context, caller)?;
    account.credit_perp(args.amount)?;
    pos.margin = new_margin;

    storage::save_account(context, caller, account)?;
    storage::save_position(context, caller, args.marketId, &pos)?;
    emit_position_margin_adjusted(context, caller, args.marketId, -amount, &pos);
    emit_position_changed(context, caller, args.marketId, &pos);
    Ok(Bytes::new())
}
/// `liquidate(address user, uint64 marketId)`
///
/// Anyone can call this to liquidate an under-margined position.
/// Closes through the orderbook first; any residual the book cannot absorb is
/// settled directly at mark price. If solvent after close, a liquidation clearance
/// fee (market.liquidation_fee_rate_bps of the pre-liquidation margin) is credited
/// to the Insurance Fund. If bankrupt, the deficit is absorbed by the IF instead.
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
    let liquidation_side = if pos.amount > 0 { Side::Sell } else { Side::Buy };
    let liquidation_quantity = pos.amount.unsigned_abs();
    let pre_liq_margin = pos.margin.max(0) as u64;

    // Cancel all open orders for this user/market (emits OrderCancelled events).
    cancel_all_orders_for_market(context, args.user, args.marketId, &market)?;

    // Try to close through the orderbook; settle any residual at mark price.
    let remaining = execute_liquidation_market_order(
        context,
        args.user,
        &market,
        liquidation_side,
        liquidation_quantity,
    )?;
    if remaining > 0 {
        settle_liquidation_residual_at_mark_price(
            context,
            args.user,
            &market,
            liquidation_side,
            mark_price,
        )?;
    }

    // Solvent: deduct clearance fee from wallet and credit to Insurance Fund.
    // Bankrupt: absorb deficit from Insurance Fund; excess becomes bad debt.
    let mut account = storage::load_account(context, args.user)?;
    let clearance_fee = if account.perp_wallet_balance >= 0 {
        let fee = (pre_liq_margin as u128)
            .saturating_mul(market.liquidation_fee_rate_bps as u128)
            / 10_000;
        let fee = (fee as u64).min(account.perp_wallet_balance as u64);
        if fee > 0 {
            account.debit_perp(fee)?;
            storage::save_account(context, args.user, account)?;
            let old_if = storage::load_insurance_fund(context)?;
            let new_if = old_if
                .checked_add(fee)
                .ok_or_else(|| perp_err("liquidate: insurance fund overflow"))?;
            storage::save_insurance_fund(context, new_if)?;
            let fee_i64 = checked_u64_to_i64(fee, "liquidate: clearance fee delta")?;
            context.journal_mut().log(Log {
                address: PERP_DEX_ADDRESS,
                data: IPerpDex::InsuranceFundChanged {
                    delta: fee_i64,
                    newBalance: new_if,
                }
                .to_log_data(),
            });
        }
        fee
    } else {
        let deficit = (-account.perp_wallet_balance) as u64;
        let (absorbed, bad_debt) = storage::absorb_from_insurance_fund(context, deficit)?;
        let new_if = storage::load_insurance_fund(context)?;
        account.credit_perp(absorbed)?;
        if bad_debt > 0 {
            account.perp_wallet_balance = 0;
        }
        storage::save_account(context, args.user, account)?;
        if absorbed > 0 {
            let absorbed_i64 = checked_u64_to_i64(absorbed, "liquidate: IF absorption delta")?;
            context.journal_mut().log(Log {
                address: PERP_DEX_ADDRESS,
                data: IPerpDex::InsuranceFundChanged {
                    delta: -absorbed_i64,
                    newBalance: new_if,
                }
                .to_log_data(),
            });
        }
        if bad_debt > 0 {
            context.journal_mut().log(Log {
                address: PERP_DEX_ADDRESS,
                data: IPerpDex::InsuranceFundDepleted {
                    marketId: args.marketId,
                    badDebt: bad_debt,
                }
                .to_log_data(),
            });
        }
        0
    };

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Liquidation {
            user: args.user,
            marketId: args.marketId,
            liquidator: caller,
            amount: liq_amount,
            reward: clearance_fee,
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
        if !account.has_available_perp(delta) {
            return Err(perp_err(
                "setLeverage: insufficient perp wallet for order margin",
            ));
        }
        account.debit_perp(delta)?;
        storage::save_account(context, user, account)?;
    } else if old_reserved > new_reserved {
        let delta = old_reserved - new_reserved;
        let mut account = storage::load_account(context, user)?;
        account.credit_perp(delta)?;
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
    if max_price > i64::MAX as u64 {
        return Err(perp_err(format!("{prefix}: maxPrice must be <= i64::MAX")));
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

fn validate_funding_config(
    prefix: &str,
    price_update_interval: u64,
    funding_interval: u64,
) -> Result<(), PrecompileError> {
    if funding_interval == 0 {
        return Ok(());
    }
    if funding_interval < price_update_interval {
        return Err(perp_err(format!(
            "{prefix}: fundingInterval must be >= priceUpdateInterval"
        )));
    }
    if funding_interval % price_update_interval != 0 {
        return Err(perp_err(format!(
            "{prefix}: fundingInterval must be a multiple of priceUpdateInterval"
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

    let old_best_bid = storage::load_best_bid(context, market_id)?;
    let old_best_ask = storage::load_best_ask(context, market_id)?;

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
    let best_bid = storage::refresh_best_bid(context, market_id)?;
    let best_ask = storage::refresh_best_ask(context, market_id)?;
    if best_bid != old_best_bid || best_ask != old_best_ask {
        record_mid_price_sample_for_best_quote_change(context, market_id, best_bid, best_ask)?;
    }

    // Recalculate reserves (now 0 since all orders cancelled).
    let mut pos = storage::load_position(context, user, market_id)?;
    let released = pos
        .margin_reserved
        .checked_add(pos.fee_reserved)
        .ok_or_else(|| perp_err("cancelAllOrders: released reserve overflow"))?;
    if released > 0 {
        let mut account = storage::load_account(context, user)?;
        account.credit_perp(released)?;
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

// ── Insurance Fund ────────────────────────────────────────────────────────────

/// `depositInsuranceFund(uint64 amount)` — admin only.
/// Debits `amount` from the admin's perp wallet and credits it to the insurance fund.
pub fn run_deposit_insurance_fund<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = depositInsuranceFundCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("depositInsuranceFund: invalid calldata"))?;

    require_admin(caller, context)?;

    if args.amount == 0 {
        return Err(perp_err("depositInsuranceFund: amount must be > 0"));
    }

    let mut account = storage::load_account(context, caller)?;
    if !account.has_available_perp(args.amount) {
        return Err(perp_err("depositInsuranceFund: insufficient perp wallet balance"));
    }
    account.debit_perp(args.amount)?;
    storage::save_account(context, caller, account)?;

    let delta = checked_u64_to_i64(args.amount, "depositInsuranceFund: delta")?;
    let old_balance = storage::load_insurance_fund(context)?;
    let new_balance = old_balance
        .checked_add(args.amount)
        .ok_or_else(|| perp_err("depositInsuranceFund: balance overflow"))?;
    storage::save_insurance_fund(context, new_balance)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::InsuranceFundChanged {
            delta,
            newBalance: new_balance,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `withdrawInsuranceFund(uint64 amount)` — admin only.
/// Withdraws `amount` from the insurance fund back to the admin's perp wallet.
pub fn run_withdraw_insurance_fund<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = withdrawInsuranceFundCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("withdrawInsuranceFund: invalid calldata"))?;

    require_admin(caller, context)?;

    if args.amount == 0 {
        return Err(perp_err("withdrawInsuranceFund: amount must be > 0"));
    }

    let delta = checked_u64_to_i64(args.amount, "withdrawInsuranceFund: delta")?;
    let balance = storage::load_insurance_fund(context)?;
    if args.amount > balance {
        return Err(perp_err("withdrawInsuranceFund: amount exceeds fund balance"));
    }
    let new_balance = balance - args.amount;
    storage::save_insurance_fund(context, new_balance)?;

    let mut account = storage::load_account(context, caller)?;
    account.credit_perp(args.amount)?;
    storage::save_account(context, caller, account)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::InsuranceFundChanged {
            delta: -delta,
            newBalance: new_balance,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `getInsuranceFund()` — returns the current insurance fund balance.
pub fn run_get_insurance_fund<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    getInsuranceFundCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getInsuranceFund: invalid calldata"))?;
    let balance = storage::load_insurance_fund(context)?;
    Ok(Bytes::from(getInsuranceFundCall::abi_encode_returns(&balance)))
}

// ── Market manager role ───────────────────────────────────────────────────────

/// `setMarketManagerAddress(address manager)` — admin only. Zero revokes.
pub fn run_set_market_manager<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = setMarketManagerAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setMarketManagerAddress: invalid calldata"))?;

    require_admin(caller, context)?;

    let previous = storage::load_market_manager(context)?;
    storage::save_market_manager(context, args.manager)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarketManagerUpdated {
            previousManager: previous,
            newManager: args.manager,
        }
        .to_log_data(),
    });
    Ok(Bytes::new())
}

/// `getMarketManagerAddress() returns (address manager)`
pub fn run_get_market_manager<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    getMarketManagerAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarketManagerAddress: invalid calldata"))?;
    let manager = storage::load_market_manager(context)?;
    Ok(Bytes::from(getMarketManagerAddressCall::abi_encode_returns(
        &manager,
    )))
}

// ── Oracle address ────────────────────────────────────────────────────────────

/// `setOracleAddress(address oracle)` — admin only.
pub fn run_set_oracle_address<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = setOracleAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setOracleAddress: invalid calldata"))?;

    require_admin(caller, context)?;

    let previous = storage::load_oracle(context)?;
    storage::save_oracle(context, args.oracle)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OracleAddressUpdated {
            previousOracle: previous,
            newOracle: args.oracle,
        }
        .to_log_data(),
    });
    Ok(Bytes::new())
}

/// `getOracleAddress() returns (address oracle)`
pub fn run_get_oracle_address<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    getOracleAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getOracleAddress: invalid calldata"))?;
    let oracle = storage::load_oracle(context)?;
    Ok(Bytes::from(getOracleAddressCall::abi_encode_returns(
        &oracle,
    )))
}

// ── Index price & mark price computation ──────────────────────────────────────

/// `updateIndexPrice(uint64 marketId, uint64 indexPrice, uint64 timestamp)`
///
/// Callable by admin or the configured oracle address.
///
/// Steps:
/// 1. Compute Price1, Price2, ContractPrice and take their median as mark price.
/// 2. Save the new index price state.
/// 4. Snap mark price to tick_size and persist it.
/// 5. Emit IndexPriceUpdated + MarkPriceUpdated.
pub fn run_update_index_price<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = updateIndexPriceCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("updateIndexPrice: invalid calldata"))?;

    require_admin_or_oracle(caller, context)?;

    if args.indexPrice == 0 {
        return Err(perp_err("updateIndexPrice: indexPrice must be > 0"));
    }
    let market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("updateIndexPrice: unknown market"))?;
    if args.indexPrice > market.max_price {
        return Err(perp_err(
            "updateIndexPrice: indexPrice exceeds market maximum",
        ));
    }
    let effective_timestamp =
        align_price_update_timestamp(args.timestamp, market.price_update_interval);
    let current_index_state = storage::load_index_price_state(context, args.marketId)?;
    if effective_timestamp <= current_index_state.timestamp {
        return Ok(Bytes::new());
    }

    // ── 1. Compute mark price components ─────────────────────────────────────
    let mut funding = storage::load_funding_state(context, args.marketId)?;
    // Price1: index adjusted by funding basis.
    let price1 = compute_price1(args.indexPrice, &funding, &market, effective_timestamp);

    // Price2: index adjusted by time-weighted top-of-book basis.
    let window = storage::load_price_basis_window(context, args.marketId)?;
    let max_index_checkpoints = max_index_price_checkpoints(market.price_update_interval);
    let mut index_history = storage::load_index_price_history(context, args.marketId)?;
    index_history.push(current_index_state, max_index_checkpoints);
    let ma_basis = window.moving_average_basis(&index_history, effective_timestamp)?;
    let index_price_i64 = checked_u64_to_i64(args.indexPrice, "updateIndexPrice: indexPrice")?;
    let price2 = index_price_i64.saturating_add(ma_basis).max(1) as u64;

    // Contract price: latest traded price, falling back to index before any trade.
    let last_traded = storage::load_last_traded_price(context, args.marketId)?;
    let contract_price = if last_traded == 0 {
        args.indexPrice
    } else {
        last_traded
    };

    let raw_mark = median_u64(price1, price2, contract_price);

    // Snap to tick_size (floor), bounded by [tick_size, max_price].
    let mark_price = if market.tick_size > 0 {
        let snapped = (raw_mark / market.tick_size) * market.tick_size;
        snapped.max(market.tick_size).min(market.max_price)
    } else {
        raw_mark.min(market.max_price).max(1)
    };

    // ── 4. Persist index price + mark price ──────────────────────────────────
    storage::save_index_price_state(
        context,
        args.marketId,
        &IndexPriceState {
            index_price: args.indexPrice,
            timestamp: effective_timestamp,
        },
    )?;
    index_history.push(
        IndexPriceState {
            index_price: args.indexPrice,
            timestamp: effective_timestamp,
        },
        max_index_checkpoints,
    );
    storage::save_index_price_history(context, args.marketId, &index_history)?;
    storage::save_mark_price(context, args.marketId, mark_price)?;

    // ── 5. Accumulate premium index for funding rate calculation ──────────────
    let mut computed_rate: Option<(i64, i64, u64)> = None; // (rate, avg_pi, sample_count)
    if market.funding_interval > 0 {
        let mut acc = storage::load_premium_accumulator(context, args.marketId)?;

        // PI = (mark_price − index_price) × FUNDING_RATE_ONE / index_price
        let pi = ((mark_price as i128 - args.indexPrice as i128)
            .checked_mul(FUNDING_RATE_ONE as i128)
            .ok_or_else(|| perp_err("updateIndexPrice: premium index overflow"))?
            / args.indexPrice as i128)
            .try_into()
            .map_err(|_| perp_err("updateIndexPrice: premium index exceeds i64::MAX"))?;

        // Initialize epoch on first oracle update. The first observed PI is the
        // first theoretical sample slot for this funding epoch.
        if acc.epoch_start_ts == 0 {
            acc.start_epoch(effective_timestamp, effective_timestamp, pi)?;
            if funding.next_funding_ts == 0 {
                funding.next_funding_ts = effective_timestamp
                    .checked_add(market.funding_interval)
                    .ok_or_else(|| perp_err("updateIndexPrice: next funding timestamp overflow"))?;
            }
        }

        // Epoch boundary: compute and store new funding rate, reset accumulator.
        if effective_timestamp >= funding.next_funding_ts {
            let epoch_last_slot_ts = funding
                .next_funding_ts
                .saturating_sub(market.price_update_interval);
            acc.fill_slots_until(epoch_last_slot_ts, market.price_update_interval, None)?;
            let avg_pi = acc.average()?;
            let rate = calc_funding_rate(avg_pi, market.interest_rate);
            let sample_count = acc.sample_count;

            funding.last_funding_rate = rate;
            while funding.next_funding_ts <= effective_timestamp {
                funding.next_funding_ts = funding
                    .next_funding_ts
                    .checked_add(market.funding_interval)
                    .ok_or_else(|| perp_err("updateIndexPrice: next funding timestamp overflow"))?;
            }
            storage::save_funding_state(context, args.marketId, &funding)?;

            acc = PremiumIndexAccumulator::default();
            acc.start_epoch(
                funding.next_funding_ts - market.funding_interval,
                effective_timestamp,
                pi,
            )?;
            computed_rate = Some((rate, avg_pi, sample_count));
        } else if acc.epoch_start_ts > 0 || funding.next_funding_ts > 0 {
            acc.fill_slots_until(effective_timestamp, market.price_update_interval, Some(pi))?;
            // Save updated next_funding_ts if it was just initialized.
            storage::save_funding_state(context, args.marketId, &funding)?;
        }

        storage::save_premium_accumulator(context, args.marketId, &acc)?;
    }

    // ── 6. Emit events ────────────────────────────────────────────────────────
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::IndexPriceUpdated {
            marketId: args.marketId,
            indexPrice: args.indexPrice,
            markPrice: mark_price,
            price1,
            price2,
            timestamp: effective_timestamp,
        }
        .to_log_data(),
    });
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarkPriceUpdated {
            marketId: args.marketId,
            price: mark_price,
            updater: caller,
        }
        .to_log_data(),
    });
    if let Some((rate, avg_pi, sample_count)) = computed_rate {
        context.journal_mut().log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::FundingRateComputed {
                marketId: args.marketId,
                fundingRate: rate,
                avgPremiumIndex: avg_pi,
                sampleCount: sample_count,
                timestamp: effective_timestamp,
            }
            .to_log_data(),
        });
    }

    Ok(Bytes::new())
}

/// `getIndexPrice(uint64 marketId) returns (uint64 indexPrice, uint64 lastTimestamp)`
pub fn run_get_index_price<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getIndexPriceCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getIndexPrice: invalid calldata"))?;
    let state = storage::load_index_price_state(context, args.marketId)?;
    Ok(Bytes::from(getIndexPriceCall::abi_encode_returns(
        &getIndexPriceReturn {
            indexPrice: state.index_price,
            lastTimestamp: state.timestamp,
        },
    )))
}

// ── Funding state ─────────────────────────────────────────────────────────────

/// `getFundingState(uint64 marketId) returns (int64 lastFundingRate, uint64 fundingInterval, uint64 nextFundingTs)`
pub fn run_get_funding_state<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getFundingStateCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getFundingState: invalid calldata"))?;
    let state = storage::load_funding_state(context, args.marketId)?;
    let market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("getFundingState: unknown market"))?;
    Ok(Bytes::from(getFundingStateCall::abi_encode_returns(
        &getFundingStateReturn {
            lastFundingRate: state.last_funding_rate,
            fundingInterval: market.funding_interval,
            nextFundingTs: state.next_funding_ts,
        },
    )))
}

/// `getAveragePremiumIndex(uint64 marketId) returns (int64 avgPremiumIndex, uint64 sampleCount)`
pub fn run_get_average_premium_index<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getAveragePremiumIndexCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAveragePremiumIndex: invalid calldata"))?;
    storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("getAveragePremiumIndex: unknown market"))?;
    let acc = storage::load_premium_accumulator(context, args.marketId)?;
    Ok(Bytes::from(getAveragePremiumIndexCall::abi_encode_returns(
        &getAveragePremiumIndexReturn {
            avgPremiumIndex: acc.average()?,
            sampleCount: acc.sample_count,
        },
    )))
}

// ── Oracle math helpers ───────────────────────────────────────────────────────

/// Price1 = index × [1 + (last_funding_rate × time_until_next / funding_interval)]
///
/// Uses FUNDING_RATE_ONE (1e6) as the fixed-point base for the rate.
/// Returns `index` unchanged if funding is not configured or the epoch has passed.
fn compute_price1(index: u64, funding: &FundingState, market: &Market, now: u64) -> u64 {
    if market.funding_interval == 0 || funding.next_funding_ts == 0 {
        return index;
    }
    let time_until = funding.next_funding_ts.saturating_sub(now);
    if time_until == 0 {
        return index;
    }
    // adjustment = rate × time_until / interval  (still in FUNDING_RATE_ONE units)
    let adjustment = (funding.last_funding_rate as i128) * (time_until as i128)
        / (market.funding_interval as i128);
    let total_factor = FUNDING_RATE_ONE as i128 + adjustment;
    let price1 = ((index as i128) * total_factor / FUNDING_RATE_ONE as i128).max(1);
    price1.min(u64::MAX as i128) as u64
}

fn median_u64(a: u64, b: u64, c: u64) -> u64 {
    let mut arr = [a, b, c];
    arr.sort_unstable();
    arr[1]
}

fn align_price_update_timestamp(timestamp: u64, interval: u64) -> u64 {
    if interval == 0 {
        return timestamp;
    }
    timestamp - (timestamp % interval)
}

fn max_index_price_checkpoints(price_update_interval: u64) -> usize {
    let interval = price_update_interval.max(1);
    (crate::perp_dex::types::PRICE_BASIS_WINDOW_SIZE as u64)
        .div_ceil(interval)
        .saturating_add(2) as usize
}

pub(crate) fn record_mid_price_sample_for_best_quote_change<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    best_bid: u64,
    best_ask: u64,
) -> Result<(), PrecompileError> {
    let mid_price = match (best_bid, best_ask) {
        (0, 0) => return Ok(()),
        (0, ask) => ask,
        (bid, 0) => bid,
        (bid, ask) => ((bid as u128 + ask as u128) / 2) as u64,
    };
    let timestamp: u64 = context.block().timestamp().saturating_to();

    let mut window = storage::load_price_basis_window(context, market_id)?;
    window.record_observation(timestamp, mid_price);
    storage::save_price_basis_window(context, market_id, &window)
}

fn require_admin_or_oracle<CTX: ContextTr>(
    caller: Address,
    context: &mut CTX,
) -> Result<(), PrecompileError> {
    let admin = storage::load_admin(context)?;
    if admin == Address::ZERO {
        return Err(perp_err("not authorised: admin not initialised"));
    }
    if caller == admin {
        return Ok(());
    }
    let oracle = storage::load_oracle(context)?;
    if oracle != Address::ZERO && caller == oracle {
        return Ok(());
    }
    Err(perp_err("not authorised: caller is not admin or oracle"))
}

fn require_admin_or_market_manager<CTX: ContextTr>(
    caller: Address,
    context: &mut CTX,
) -> Result<(), PrecompileError> {
    let admin = storage::load_admin(context)?;
    if admin == Address::ZERO {
        return Err(perp_err("not authorised: admin not initialised"));
    }
    if caller == admin {
        return Ok(());
    }
    let manager = storage::load_market_manager(context)?;
    if manager != Address::ZERO && caller == manager {
        return Ok(());
    }
    Err(perp_err(
        "not authorised: caller is not admin or market manager",
    ))
}

#[cfg(test)]
mod tests;
