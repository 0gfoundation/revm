//! Risk management: markets, leverage, mark price, positions, liquidation.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use crate::host::PerpHost;
use primitives::{Address, Bytes, FixedBytes, Log};

use crate::{
        errors::perp_err,
    funding::{apply_funding_settlement, compute_funding_settlement},
    interface::IPerpDex::{
        self, addMarketCall, addPositionMarginCall, depositInsuranceFundCall, getAdminCall,
        getAveragePremiumIndexCall, getAveragePremiumIndexReturn, getFundingStateCall,
        getFundingStateReturn, getIndexPriceCall, getIndexPriceReturn, getInsuranceFundCall,
        getMarginTiersCall, getMarginTiersReturn, getMarkPriceCall, getMarketCall,
        getMarketManagerAddressCall, getMarketReturn, getOracleAddressCall, getPositionCall,
        getPositionReturn, initAdminCall, liquidateCall, removePositionMarginCall,
        setLeverageCall, setLeverageSignedCall, setMarginTiersCall,
        setMarketManagerAddressCall, setOracleAddressCall, transferAdminCall,
        updateIndexPriceCall, updateMarketCall, withdrawInsuranceFundCall,
    },
    math::{
        calc_funding_rate, calc_position_equity, calc_value, calc_value_i64, checked_u64_to_i64,
        is_above_maintenance_margin, max_leverage_for_notional, FUNDING_RATE_ONE,
    },
    storage,
    trading::{
        check_api_key_expiry, check_recv_window, execute_liquidation_market_order, run_adl,
        settle_liquidation_residual_at_mark_price, verify_ed25519,
    },
    types::{
        FundingState, IndexPriceState, MarginTier, MarginTiers, Market,
        PremiumIndexAccumulator, Side, MAX_LEVERAGE_HARD_CAP, MAX_MARGIN_TIERS,
    },
    PERP_DEX_ADDRESS,
    PerpError,
};

// ── Admin: ownership ──────────────────────────────────────────────────────────

/// `initAdmin(address admin)` — one-time initialisation; fails if already set.
pub fn run_init_admin<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
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

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AdminInitialized { admin: args.admin }.to_log_data(),
    });
    Ok(Bytes::new())
}

/// `transferAdmin(address newAdmin)` — only callable by current admin.
pub fn run_transfer_admin<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
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

    context.log(Log {
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
pub fn run_get_admin<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    getAdminCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAdmin: invalid calldata"))?;
    let admin = storage::load_admin(context)?;
    Ok(Bytes::from(getAdminCall::abi_encode_returns(&admin)))
}

// ── Admin: market management ──────────────────────────────────────────────────

/// `addMarket(uint64 marketId, uint32 baseDecimals, uint32 priceDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, uint64 fundingInterval, int64 interestRate)`
pub fn run_add_market<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = addMarketCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("addMarket: invalid calldata"))?;

    require_admin_or_market_manager(caller, context)?;

    if storage::load_market_ref(context, args.marketId)?.is_some() {
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
    // A mandatory initial mark price lets the price band be active from block one
    // (no unpriced bootstrap window). It is overwritten by the first updateIndexPrice.
    if args.initialMarkPrice == 0 {
        return Err(perp_err("addMarket: initialMarkPrice must be > 0"));
    }
    if args.initialMarkPrice > args.maxPrice {
        return Err(perp_err("addMarket: initialMarkPrice exceeds maxPrice"));
    }
    if args.initialMarkPrice % args.tickSize != 0 {
        return Err(perp_err(
            "addMarket: initialMarkPrice must be a multiple of tickSize",
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
        price_band_bps: args.priceBandBps,
        // mark_price now lives in the Market blob (was a separate save_mark_price call).
        mark_price: args.initialMarkPrice,
        // `addMarket` is deliberately NOT grown to carry the tier table (it is already
        // 14 args with a fixed-offset signed layout). Every market is born single-tier
        // `[{0, DEFAULT_MAX_LEVERAGE}]` — maintenance rate 1/6, leverage cap 3 — and is
        // retuned afterwards by `setMarginTiers`.
        tiers: MarginTiers::default(),
    };
    storage::save_market(context, &market)?;

    context.log(Log {
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
            initialMarkPrice: args.initialMarkPrice,
            priceBandBps: args.priceBandBps,
        }
        .to_log_data(),
    });
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarkPriceUpdated {
            marketId: args.marketId,
            price: args.initialMarkPrice,
            updater: caller,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `updateMarket(uint64 marketId, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, bool active, uint64 fundingInterval, int64 interestRate, uint32 liquidationFeeRateBps, uint32 priceBandBps)`
pub fn run_update_market<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
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
    market.price_band_bps = args.priceBandBps;
    // NOTE: `market.tiers` is deliberately absent here. The update is field-by-field over
    // the LOADED market, so the risk table survives verbatim — retuning tick/step/funding
    // can never reset it. Tiers move only through `setMarginTiers`.
    storage::save_market(context, &market)?;

    context.log(Log {
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
            priceBandBps: args.priceBandBps,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `setMarginTiers(uint64 marketId, uint64[] lowerBounds, uint32[] maxLeverages)`
///
/// Replaces a market's margin-tier table wholesale. ALL validation runs before the single
/// `save_market` (validate-then-apply: perp writes are commit-only, so no genuine reject
/// may follow a write). Each invariant carries its own error string.
pub fn run_set_margin_tiers<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setMarginTiersCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setMarginTiers: invalid calldata"))?;

    require_admin_or_market_manager(caller, context)?;

    let mut market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("setMarginTiers: unknown market"))?;

    // ── validate (every check precedes the write) ──
    if args.lowerBounds.len() != args.maxLeverages.len() {
        return Err(perp_err(
            "setMarginTiers: lowerBounds and maxLeverages length mismatch",
        ));
    }
    if args.lowerBounds.is_empty() {
        return Err(perp_err("setMarginTiers: at least one tier required"));
    }
    if args.lowerBounds.len() > MAX_MARGIN_TIERS {
        return Err(perp_err(format!(
            "setMarginTiers: at most {MAX_MARGIN_TIERS} tiers allowed"
        )));
    }
    if args.lowerBounds[0] != 0 {
        return Err(perp_err("setMarginTiers: first tier must start at 0"));
    }
    if args.lowerBounds.windows(2).any(|w| w[1] <= w[0]) {
        return Err(perp_err(
            "setMarginTiers: lowerBounds must be strictly increasing",
        ));
    }
    if args
        .maxLeverages
        .iter()
        .any(|&l| l == 0 || l > MAX_LEVERAGE_HARD_CAP)
    {
        return Err(perp_err(format!(
            "setMarginTiers: maxLeverage must be 1–{MAX_LEVERAGE_HARD_CAP}"
        )));
    }
    if args.maxLeverages.windows(2).any(|w| w[1] > w[0]) {
        return Err(perp_err(
            "setMarginTiers: maxLeverages must be non-increasing",
        ));
    }

    let rows: Vec<MarginTier> = args
        .lowerBounds
        .iter()
        .zip(args.maxLeverages.iter())
        .map(|(&lower_bound_notional, &max_leverage)| MarginTier {
            lower_bound_notional,
            max_leverage,
        })
        .collect();
    // Unreachable: the length bounds above are exactly `from_tiers`' precondition.
    let tiers = MarginTiers::from_tiers(&rows)
        .ok_or_else(|| perp_err("setMarginTiers: invalid tier count"))?;

    // ── APPLY (all rejects passed) ──
    market.tiers = tiers;
    storage::save_market(context, &market)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarginTiersUpdated {
            marketId: args.marketId,
            lowerBounds: args.lowerBounds,
            maxLeverages: args.maxLeverages,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `getMarginTiers(uint64 marketId) returns (uint64[] lowerBounds, uint32[] maxLeverages)`
pub fn run_get_margin_tiers<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getMarginTiersCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarginTiers: invalid calldata"))?;

    let market = storage::load_market_ref(context, args.marketId)?
        .ok_or_else(|| perp_err("getMarginTiers: unknown market"))?;

    let table = market.tiers.as_slice();
    Ok(Bytes::from(getMarginTiersCall::abi_encode_returns(
        &getMarginTiersReturn {
            lowerBounds: table.iter().map(|t| t.lower_bound_notional).collect(),
            maxLeverages: table.iter().map(|t| t.max_leverage).collect(),
        },
    )))
}

/// `getMarkPrice(uint64 marketId) returns (uint64 price)`
pub fn run_get_mark_price<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getMarkPriceCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarkPrice: invalid calldata"))?;

    let price = storage::load_mark_price(context, args.marketId)?;
    Ok(Bytes::from(getMarkPriceCall::abi_encode_returns(&price)))
}

/// `getMarket(uint64 marketId) returns (uint32 baseDecimals, uint32 priceDecimals, uint64 tickSize, uint64 stepSize, uint64 minQuantity, uint64 maxQuantity, uint64 maxPrice, uint64 priceUpdateInterval, bool active)`
pub fn run_get_market<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getMarketCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarket: invalid calldata"))?;

    let market = storage::load_market_ref(context, args.marketId)?
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
pub fn run_set_leverage<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setLeverageCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setLeverage: invalid calldata"))?;
    set_leverage_core(context, caller, args.marketId, args.leverage)
}

/// `setLeverageSigned(address account, uint64 marketId, uint64 leverage, uint64 timestamp, uint64 recvWindow, uint8 keyId, bytes signature)`
pub fn run_set_leverage_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
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

fn set_leverage_core<H: PerpHost>(
    context: &mut H,
    account: Address,
    market_id: u64,
    leverage: u64,
) -> Result<Bytes, PerpError> {
    let market = storage::load_market_ref(context, market_id)?
        .ok_or_else(|| perp_err("setLeverage: unknown market"))?;

    let mut pos = storage::load_position(context, account, market_id)?;

    // The cap comes from the market's own risk table — the single source of truth, so the
    // cap and the maintenance rate can never disagree (mmr(n) = 1/(2*max_leverage(n)); the
    // legacy 1/6 rate IS L₀ = 3).
    //
    // Look the tier up by the position's CURRENT notional rather than hardcoding tier 0.
    // Tier 0 is merely the value this collapses to in two cases that both happen to hold
    // today — a single-tier table, and a flat position (notional 0 lands in tier 0) — so
    // hardcoding it would bake in a special case. Under a real multi-tier table a position
    // already sitting in a higher bracket must be held to THAT bracket's cap: otherwise a
    // trader in a 2x tier could set 3x here, and `rebalance_order_margin_for_leverage`
    // below would release resting-order margin down to the 3x requirement while the
    // position's bracket demands 2x. (Binance rejects the same call with "exceeded the
    // maximum allowable position at current leverage".) Same quantity and same helper the
    // per-open guard uses at the settlement cores, so the two agree by construction.
    let abs_notional = calc_value_i64(
        storage::load_mark_price(context, market_id)?,
        pos.amount,
        market.base_decimals,
        market.price_decimals,
    )?
    .checked_abs()
    .ok_or_else(|| perp_err("setLeverage: tier notional abs overflow"))?;
    let cap = max_leverage_for_notional(&market.tiers, abs_notional) as u64;
    if leverage == 0 || leverage > cap {
        return Err(perp_err(format!("setLeverage: leverage must be 1–{cap}")));
    }

    let old_leverage = pos.leverage.max(1);
    if pos.amount != 0 && leverage < old_leverage {
        return Err(perp_err(
            "setLeverage: cannot reduce leverage with open position",
        ));
    }

    rebalance_order_margin_for_leverage(context, account, &market, &mut pos, leverage)?;
    pos.leverage = leverage;
    storage::save_position(context, account, market_id, &pos)?;

    context.log(Log {
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

/// `getPosition(address user, uint64 marketId) returns (int64 amount, int64 vQuoteBalance, int64 margin, uint64 openOrderMargin, uint64 leverage)`
///
/// `openOrderMargin` is DERIVED (`getMarginInfo`'s `openOrderInitialMargin`), not stored: the
/// escrow field that used to occupy this slot is gone. It needs the market's mark price and
/// decimals, hence the extra `load_market_ref`; an unknown market yields `0` rather than a revert,
/// preserving this view's "reads back all zeros for a user with nothing here" behaviour.
pub fn run_get_position<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getPositionCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getPosition: invalid calldata"))?;

    let pos = storage::load_position_ref(context, args.user, args.marketId)?;
    let open_order_margin = match storage::load_market_ref(context, args.marketId)? {
        Some(market) => crate::margin_view::position_open_order_margin(&market, &pos)?,
        None => 0,
    };
    Ok(Bytes::from(getPositionCall::abi_encode_returns(
        &getPositionReturn {
            amount: pos.amount,
            vQuoteBalance: pos.v_quote_balance,
            margin: pos.margin,
            openOrderMargin: open_order_margin,
            leverage: pos.leverage,
        },
    )))
}

// ── Liquidation ───────────────────────────────────────────────────────────────

/// `addPositionMargin(uint64 marketId, uint64 amount)`
pub fn run_add_position_margin<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = addPositionMarginCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("addPositionMargin: invalid calldata"))?;
    if args.amount == 0 {
        return Err(perp_err("addPositionMargin: amount must be > 0"));
    }
    let amount = checked_u64_to_i64(args.amount, "addPositionMargin: amount")?;
    let market = storage::load_market_ref(context, args.marketId)?
        .ok_or_else(|| perp_err("addPositionMargin: unknown market"))?;

    let mut pos = storage::load_position(context, caller, args.marketId)?;
    if pos.amount == 0 {
        return Err(perp_err("addPositionMargin: no open position"));
    }
    let mut account = storage::load_account(context, caller)?;
    // commit-only #23: compute funding IN MEMORY (no insurance-fund write yet), so a reject below
    // leaves the IF untouched. The credit/charge lands on the in-memory `pos.margin` — funding is
    // isolated to the position and never touches the account-global wallet.
    let pending_funding = compute_funding_settlement(context, caller, &market, &mut pos)?;
    // Derived-ooIM gate. A CASH move (wallet → position margin), not an open-order requirement:
    // it changes neither `Bid`/`Ask` nor `N` nor `L`, so Σ ooIM is unchanged and the requirement
    // is exactly `amount`. It must come out of AVAILABLE, not the raw wallet — otherwise a user
    // could park their whole balance in position margin out from under their resting orders.
    let available = crate::margin_view::derived_available_balance(context, caller)?;
    if !crate::margin_view::derived_can_afford(available, args.amount as i128) {
        return Err(perp_err(
            "addPositionMargin: insufficient perp wallet balance",
        ));
    }
    account.debit_perp(args.amount)?;
    pos.margin = pos
        .margin
        .checked_add(amount)
        .ok_or_else(|| perp_err("addPositionMargin: margin overflow"))?;

    // ── APPLY (all rejects passed) ──
    if let Some(p) = pending_funding {
        apply_funding_settlement(context, p)?;
    }
    // Position BEFORE account: the account write emits the account-level `AccountBalanceChanged`
    // roll-up, whose `totalWalletBalance` is `cross + Σ positionMargin`. Written the other way round
    // the one event this call emits would report the debited wallet against the OLD silo and
    // under-state the gross wallet by exactly `amount` — an intermediate snapshot for no reason,
    // since both writes are unconditional here. Delta order is irrelevant to the commitment (the
    // block delta is a net key→value map), and `pos.amount` is unchanged so no registry/index hook
    // behaves differently.
    storage::save_position(context, caller, args.marketId, &pos)?;
    storage::save_account(context, caller, account)?;
    emit_position_margin_adjusted(context, caller, args.marketId, amount, &pos);
    emit_position_changed(context, caller, args.marketId, &pos);
    Ok(Bytes::new())
}

/// `removePositionMargin(uint64 marketId, uint64 amount)`
pub fn run_remove_position_margin<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = removePositionMarginCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("removePositionMargin: invalid calldata"))?;
    if args.amount == 0 {
        return Err(perp_err("removePositionMargin: amount must be > 0"));
    }
    let amount = checked_u64_to_i64(args.amount, "removePositionMargin: amount")?;
    let market = storage::load_market_ref(context, args.marketId)?
        .ok_or_else(|| perp_err("removePositionMargin: unknown market"))?;
    let mark_price = storage::load_mark_price(context, args.marketId)?;
    if mark_price == 0 {
        return Err(perp_err("removePositionMargin: mark price unavailable"));
    }

    let mut pos = storage::load_position(context, caller, args.marketId)?;
    if pos.amount == 0 {
        return Err(perp_err("removePositionMargin: no open position"));
    }
    let mut account = storage::load_account(context, caller)?;
    // commit-only #23: compute funding IN MEMORY first (no IF write yet) so the checks below see
    // post-funding margin, and a reject leaves the insurance fund untouched.
    let pending_funding = compute_funding_settlement(context, caller, &market, &mut pos)?;
    if pos.margin < amount {
        return Err(perp_err(
            "removePositionMargin: insufficient position margin",
        ));
    }
    let new_margin = pos.margin - amount;
    // MAINTENANCE margin is the only requirement gate here — initial margin is deliberately NOT
    // checked (B2, Binance parity).
    //
    // There used to be an additional `new_margin < ROUND_DOWN(N/L)` gate. It was strictly
    // stronger than the maintenance check below and it made `removePositionMargin` unusable in
    // the ordinary case: since `7cc26360` an opening fill funds the trading fee OUT of the
    // margin, so a freshly opened position at its market's max leverage already sits at
    // `floor(N/L) − fee`, i.e. BELOW the initial-margin requirement. Every removal was refused,
    // and even `addPositionMargin(X)` followed by `removePositionMargin(X)` — a round trip that
    // moves the position nowhere — was refused.
    //
    // Binance checks maintenance margin continuously and never re-checks initial margin
    // (`binance-margin-verified-model.md` §1.5: margin is validated at placement and not again;
    // `addPositionMargin`/`removePositionMargin` are a pure transfer that "不改 IM,不改 MM"). The
    // formula set §1.2 states it flatly — 「**Binance 只连续检查 MM,不检查 IM**」 — and
    // `binance-flip-and-admission.md` §3.9 supplies the MEASUREMENT: run1's market open computed
    // `PIM = 6.34041` while the silo received `6.30870795`, short by exactly one opening commission,
    // and the position 「照常存活」. So 「silo 低于 IM」 is 「**常态,不是异常**」 — a continuous IM
    // check here (or anywhere else) would be a divergence, not a safety net.
    // The maintenance gate is the one that matters and is strictly the right one here: it
    // accounts for unrealized PnL via `v_quote_balance`, which the initial-margin form did not,
    // so collateral still cannot be stripped from a position that is sliding underwater — and a
    // position can never be left immediately liquidatable.
    if !is_above_maintenance_margin(
        &market.tiers,
        mark_price,
        pos.amount,
        pos.v_quote_balance,
        new_margin,
        market.base_decimals,
        market.price_decimals,
    )? {
        return Err(perp_err(
            "removePositionMargin: resulting position below maintenance margin",
        ));
    }

    account.credit_perp(args.amount)?;
    pos.margin = new_margin;

    // ── APPLY (all rejects passed) ──
    if let Some(p) = pending_funding {
        apply_funding_settlement(context, p)?;
    }
    // Position BEFORE account — see `run_add_position_margin` for why.
    storage::save_position(context, caller, args.marketId, &pos)?;
    storage::save_account(context, caller, account)?;
    emit_position_margin_adjusted(context, caller, args.marketId, -amount, &pos);
    emit_position_changed(context, caller, args.marketId, &pos);
    Ok(Bytes::new())
}
/// Result of a single `liquidate_position` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiquidationOutcome {
    /// Position was closed; carries the pre-close signed amount and the clearance
    /// fee credited to the Insurance Fund.
    Liquidated { amount: i64, reward: u64 },
    /// No open position (`amount == 0`) — nothing to do.
    NoPosition,
    /// Position is at or above the maintenance-margin threshold — not liquidatable.
    AboveMaintenance,
}

/// Core liquidation logic, shared by the manual `liquidate` entry point
/// (`run_liquidate`) and the protocol-automatic sweep (inside
/// `run_update_index_price`). The caller must already hold the loaded `market`
/// and a non-zero `mark_price` (the mark==0 degenerate case is rejected by the
/// wrapper). Settles funding on the position first (so the charge counts toward
/// insolvency), then:
/// - `amount == 0`  → `NoPosition` (no state change beyond the funding settle);
/// - above maintenance → `AboveMaintenance`;
/// - otherwise closes through the orderbook, settles any residual at mark, charges
///   the clearance fee to the IF, emits `Liquidation`, and returns `Liquidated`.
///
/// `liquidator` is recorded verbatim in the `Liquidation` event (the caller for a
/// manual liquidation; a system address for the sweep). Bad debt from the close
/// legs is routed to the IF inside the close paths, not here.
pub(crate) fn liquidate_position<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    market: &crate::types::Market,
    mark_price: u64,
    liquidator: Address,
    adl_budget: &mut u32,
) -> Result<LiquidationOutcome, PerpError> {
    let mut pos = storage::load_position(context, user, market_id)?;

    if pos.amount == 0 {
        return Ok(LiquidationOutcome::NoPosition);
    }
    // commit-only #23: compute accrued funding IN MEMORY first (no insurance-fund write), so the
    // maintenance check sees the post-funding position but an AboveMaintenance outcome leaves
    // ZERO writes. This is what makes the liquidation sweep's healthy-candidate scan write-free
    // (previously every scanned healthy account wrote a funding settle that only checkpoint_revert
    // discarded) and makes manual liquidate reject cleanly without relying on undo.
    //
    // Funding settles against `pos.margin` only, so this path no longer loads/writes the account at
    // all: the load existed solely to hand the wallet to the settlement, and the matching
    // `save_account` below would have re-written a byte-identical blob (and emitted a spurious
    // `AccountBalanceChanged` for a balance that did not move). The liquidation's real wallet
    // movements happen inside the close legs, which write the account themselves.
    let pending_funding = compute_funding_settlement(context, user, market, &mut pos)?;
    if is_above_maintenance_margin(
        &market.tiers,
        mark_price,
        pos.amount,
        pos.v_quote_balance,
        pos.margin,
        market.base_decimals,
        market.price_decimals,
    )? {
        return Ok(LiquidationOutcome::AboveMaintenance);
    }

    let liq_amount = pos.amount;
    let liquidation_side = if pos.amount > 0 {
        Side::Sell
    } else {
        Side::Buy
    };
    let liquidation_quantity = pos.amount.unsigned_abs();
    let pre_liq_margin = pos.margin.max(0) as u64;

    // ── APPLY (liquidatable — commit the funding settle, then close) ──
    if let Some(p) = pending_funding {
        apply_funding_settlement(context, p)?;
    }
    storage::save_position(context, user, market_id, &pos)?;

    // Cancel all open orders for this user/market (emits OrderCancelled events).
    cancel_all_orders_for_market(context, user, market_id, market)?;

    // Try to close through the orderbook; settle any residual at mark price.
    let remaining = execute_liquidation_market_order(
        context,
        user,
        market,
        liquidation_side,
        liquidation_quantity,
    )?;
    if remaining > 0 {
        // Residual: a SOLVENT residual (equity >= 0 at mark — the position is below
        // maintenance but not yet bankrupt) is returned to the loser by closing at mark
        // (no bad debt, no ADL, no IF). An INSOLVENT residual (equity < 0) is closed as
        // a forced trade against opposite-side holders at the bankruptcy price via ADL
        // (scheme X: no IF). ADL leftover (budget exhausted / not enough deeply-in-profit
        // opposite holders) stays open and is re-swept next update.
        let residual = storage::load_position(context, user, market_id)?;
        let residual_equity = calc_position_equity(
            mark_price,
            residual.amount,
            residual.v_quote_balance,
            residual.margin,
            market.base_decimals,
            market.price_decimals,
        )?;
        if residual_equity >= 0 {
            settle_liquidation_residual_at_mark_price(
                context,
                user,
                market,
                liquidation_side,
                mark_price,
            )?;
        } else {
            run_adl(context, user, market, mark_price, adl_budget)?;
        }
    }

    // Isolated margin: the position's loss (book leg + residual) was already contained
    // to its margin and any bad debt routed directly to the Insurance Fund by the close
    // paths (apply_position_fill / settle_liquidation_residual), so LIQUIDATION never drives
    // the wallet negative. It may nonetheless ARRIVE here negative — a maker fill charges the part
    // of its commission the (M1-capped) opening margin could not absorb to the wallet, which on a
    // pure close is the whole fee (`settle_maker_fill_core`; a margin shortfall no longer reaches
    // the wallet at all) — hence the `.max(0)` below: the clearance fee is capped at the
    // POSITIVE balance, so an already-negative wallet is charged nothing rather than being pushed
    // further under (and `as u64` on a negative i64 would otherwise wrap to an astronomical cap).
    // Charge the clearance fee from the liquidated user's remaining wallet (capped at the balance)
    // and credit it to the Insurance Fund.
    let mut account = storage::load_account(context, user)?;
    let clearance_fee = {
        let fee = (pre_liq_margin as u128).saturating_mul(market.liquidation_fee_rate_bps as u128)
            / 10_000;
        let fee = (fee as u64).min(account.perp_wallet_balance.max(0) as u64);
        if fee > 0 {
            account.debit_perp(fee)?;
            storage::save_account(context, user, account)?;
            let old_if = storage::load_insurance_fund(context)?;
            let new_if = old_if
                .checked_add(fee)
                .ok_or_else(|| perp_err("liquidate: insurance fund overflow"))?;
            storage::save_insurance_fund(context, new_if)?;
            let fee_i64 = checked_u64_to_i64(fee, "liquidate: clearance fee delta")?;
            context.log(Log {
                address: PERP_DEX_ADDRESS,
                data: IPerpDex::InsuranceFundChanged {
                    delta: fee_i64,
                    newBalance: new_if,
                }
                .to_log_data(),
            });
        }
        fee
    };

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Liquidation {
            user,
            marketId: market_id,
            liquidator,
            amount: liq_amount,
            reward: clearance_fee,
            markPrice: mark_price,
        }
        .to_log_data(),
    });

    Ok(LiquidationOutcome::Liquidated {
        amount: liq_amount,
        reward: clearance_fee,
    })
}

/// `liquidate(address user, uint64 marketId)`
///
/// Anyone can call this to liquidate an under-margined position. Thin wrapper over
/// [`liquidate_position`]: decodes calldata, loads the market + mark price (refusing
/// a market with no oracle price), and maps the core outcome to the caller-facing
/// result. `caller` is recorded as the liquidator.
pub fn run_liquidate<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = liquidateCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("liquidate: invalid calldata"))?;

    let market = storage::load_market_ref(context, args.marketId)?
        .ok_or_else(|| perp_err("liquidate: unknown market"))?;
    let mark_price = storage::load_mark_price(context, args.marketId)?;
    if mark_price == 0 {
        // A market that never received an oracle price would make the
        // maintenance-margin gate degenerate (notional and threshold both 0),
        // so solvency would be judged on the sign of `v_quote + margin` alone.
        // Refuse to liquidate without a real mark price.
        return Err(perp_err("liquidate: mark price unavailable"));
    }

    let mut adl_budget = ADL_BUDGET_PER_UPDATE;
    match liquidate_position(
        context,
        args.user,
        args.marketId,
        &market,
        mark_price,
        caller,
        &mut adl_budget,
    )? {
        LiquidationOutcome::Liquidated { .. } => Ok(Bytes::new()),
        LiquidationOutcome::NoPosition => Err(perp_err("liquidate: no open position")),
        LiquidationOutcome::AboveMaintenance => {
            Err(perp_err("liquidate: position is above maintenance margin"))
        }
    }
}

/// Max positions actually LIQUIDATED per `updateIndexPrice` sweep. This bounds the
/// expensive half — each liquidation closes through the book + writes — but NOT the
/// registry scan itself: the loop still visits every registered candidate until it
/// has liquidated this many, so a sweep over N mostly-healthy positions is O(N)
/// (load + funding-settle + revert per healthy candidate). That O(N) scan on the
/// (delayed-execution) newPayload critical path is a known scale limit; the real fix
/// is the tick-bucket candidate index (auto-liq Phase E), which visits only
/// liquidatable candidates. Overflow of THIS cap defers to the next update: the
/// registry is re-scanned every update and un-liquidated candidates stay underwater
/// until the mark moves, so nothing is permanently missed.
const MAX_LIQUIDATIONS_PER_UPDATE: u32 = 50;

/// Total ADL fills allowed across ONE `updateIndexPrice` (shared by every liquidation
/// in the sweep). ADL closes an insolvent book-unfillable residual as forced trades
/// against opposite-side holders (scheme X: no Insurance Fund on the residual path);
/// each fill writes two positions, so this bounds the per-tx work on the delayed-
/// execution critical path. A residual not fully closed within the budget stays open
/// and is re-swept next update — deferral is safe (no realized bad debt, the opposite
/// side's offsetting gains persist).
const ADL_BUDGET_PER_UPDATE: u32 = 128;

/// Protocol-automatic liquidation sweep, run synchronously at the tail of
/// [`run_update_index_price`] after the new mark + funding are persisted (so the
/// `Liquidation` events attach to the oracle's updateIndexPrice receipt). Scans
/// the market's open-position registry and liquidates every candidate below
/// maintenance at the new mark.
///
/// Each candidate is attempted under its own journal checkpoint:
/// - `Liquidated` → commit the writes;
/// - healthy / stale (`AboveMaintenance` / `NoPosition`) → revert (the sweep leaves no trace).
///
/// Any other error after the liquidatability check is a commit-only invariant failure and aborts
/// the node rather than allowing a partial write set.
fn run_liquidation_sweep<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &crate::types::Market,
    mark_price: u64,
) -> Result<(), PerpError> {
    // Snapshot the registry (owned Vec, deterministic insertion order). Liquidations
    // mutate the live registry via save_position, but iterating the snapshot is stable;
    // a candidate already closed by an earlier cascade in this sweep resolves to
    // NoPosition (skipped), and cascade-created candidates are picked up next update.
    let candidates = storage::load_position_registry(context, market_id)?;
    let mut liquidated = 0u32;
    // One ADL-fill budget shared across every liquidation in this sweep (bounds per-tx
    // ADL work regardless of how many positions liquidate).
    let mut adl_budget = ADL_BUDGET_PER_UPDATE;
    for user in candidates {
        if liquidated >= MAX_LIQUIDATIONS_PER_UPDATE {
            break;
        }
        // commit-only (#23): healthy candidates are write-free (funding is computed in memory
        // and only applied when liquidatable), so the checkpoint only balances EVM-side state.
        // A liquidation that FAILS mid-apply would leave partial perp writes with no undo — a
        // corruption-anyway condition: halt loudly rather than skip silently.
        let cp = context.checkpoint();
        match liquidate_position(
            context,
            user,
            market_id,
            market,
            mark_price,
            Address::ZERO,
            &mut adl_budget,
        ) {
            Ok(LiquidationOutcome::Liquidated { .. }) => {
                context.checkpoint_commit();
                liquidated += 1;
            }
            Ok(_) => {
                // AboveMaintenance / NoPosition: zero perp writes were made.
                context.checkpoint_revert(cp);
            }
            Err(e) => {
                panic!(
                    "liquidation sweep: liquidate_position failed mid-apply (commit-only invariant): {e:?}"
                );
            }
        }
    }
    Ok(())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Gate a leverage change on the DERIVED open-order requirement it would create.
///
/// Under the escrow this function MOVED MONEY: the reservation was `c_notional / leverage`, so
/// raising leverage released wallet cash and lowering it debited more. Nothing is escrowed any
/// more, so nothing moves here — but the requirement still changes, because `ooIM` divides by
/// `L`. Lowering leverage raises `Σ ooIM` and must be refused when the account cannot carry it;
/// raising leverage lowers it and is always free.
///
/// `Bid`/`Ask`/`N` are untouched by `setLeverage` (it moves neither the position nor the book), so
/// "before" is the stored position and "after" is the same position at `new_leverage` — the only
/// input that differs is the divisor. It does **not** maintain the aggregates and must not: an
/// order's Assuming Price is frozen at PLACEMENT, and a leverage change is not a placement, so
/// re-freezing here would silently re-price the whole resting book at the current mark. Pure read +
/// reject: zero writes on either outcome, so this may precede every write on the path
/// (commit-only).
fn rebalance_order_margin_for_leverage<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &crate::types::Market,
    pos: &crate::types::PerpPosition,
    new_leverage: u64,
) -> Result<(), PerpError> {
    let mut after = pos.clone();
    after.leverage = new_leverage;
    let delta = crate::margin_view::derived_requirement_delta(market, pos, &after)?;
    // The available is measured on the CURRENT (pre-change) state, which is what `delta` is the
    // increment to — with the caller's in-memory `pos` overriding storage for this market, since
    // it is not written until after this gate. A non-positive delta is free (`derived_can_afford`).
    let available = crate::margin_view::derived_available_balance_with(
        context,
        user,
        None,
        Some((market.market_id, pos)),
    )?;
    if !crate::margin_view::derived_can_afford(available, delta) {
        return Err(perp_err(
            "setLeverage: insufficient perp wallet for order margin",
        ));
    }
    Ok(())
}

fn emit_position_changed<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    pos: &crate::types::PerpPosition,
) {
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user,
            marketId: market_id,
            amount: pos.amount,
            vQuoteBalance: pos.v_quote_balance,
            margin: pos.margin,
            leverage: pos.leverage,
            realizedPnl: 0,
            closedQuantity: 0,
        }
        .to_log_data(),
    });
}

fn emit_position_margin_adjusted<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    delta: i64,
    pos: &crate::types::PerpPosition,
) {
    context.log(Log {
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

fn require_admin<H: PerpHost>(
    caller: Address,
    context: &mut H,
) -> Result<(), PerpError> {
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
) -> Result<(), PerpError> {
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
) -> Result<(), PerpError> {
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
pub(crate) fn cancel_all_orders_for_market<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    _market: &Market,
) -> Result<(), PerpError> {
    let old_best_bid = storage::load_best_bid(context, market_id)?;
    let old_best_ask = storage::load_best_ask(context, market_id)?;

    // --- Buy orders ---
    let buy_entries = storage::load_buy_orders_ref(context, user, market_id)?;
    for entry in buy_entries.iter() {
        // delete-on-terminal: drop the order record (was: save Cancelled).
        storage::delete_order(context, &entry.order_id)?;
        // lazy-queue: decrement the level's live count and leave the id for the next match walk to
        // sweep (other users' orders may share this price). decr_level_count clears the FIFO on
        // reaching 0 (blob → delete); we only drop the price from the index.
        let empty = storage::decr_level_count(context, market_id, Side::Buy, entry.price, 1)? == 0;
        if empty {
            storage::remove_bid_price(context, market_id, entry.price)?;
        }

        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::OrderCancelled {
                user,
                orderId: FixedBytes(entry.order_id),
                marketId: market_id,
            }
            .to_log_data(),
        });
    }
    storage::save_buy_orders(context, user, market_id, &std::collections::VecDeque::new())?;

    // --- Sell orders ---
    let sell_entries = storage::load_sell_orders_ref(context, user, market_id)?;
    for entry in sell_entries.iter() {
        // delete-on-terminal + lazy-queue (see the buy loop above).
        storage::delete_order(context, &entry.order_id)?;
        let empty = storage::decr_level_count(context, market_id, Side::Sell, entry.price, 1)? == 0;
        if empty {
            storage::remove_ask_price(context, market_id, entry.price)?;
        }

        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::OrderCancelled {
                user,
                orderId: FixedBytes(entry.order_id),
                marketId: market_id,
            }
            .to_log_data(),
        });
    }
    storage::save_sell_orders(context, user, market_id, &std::collections::VecDeque::new())?;
    let best_bid = storage::refresh_best_bid(context, market_id)?;
    let best_ask = storage::refresh_best_ask(context, market_id)?;
    if best_bid != old_best_bid || best_ask != old_best_ask {
        record_mid_price_sample_for_best_quote_change(context, market_id, best_bid, best_ask)?;
    }

    // Both order lists were cleared → `Bid = Ask = 0`, so this position's derived open-order
    // requirement drops to 0 by itself. NOTHING is credited back to the wallet: a resting order
    // escrows nothing (neither margin nor fee), so a cancel — including this cancel-all — moves
    // no money at all. It only shrinks `Σ ooIM`, i.e. frees AVAILABLE, not balance.
    let mut pos = storage::load_position(context, user, market_id)?;
    pos.clear_side_aggregates();
    storage::save_position(context, user, market_id, &pos)?;

    Ok(())
}

// ── Insurance Fund ────────────────────────────────────────────────────────────

/// `depositInsuranceFund(uint64 amount)` — admin only.
/// Debits `amount` from the admin's perp wallet and credits it to the insurance fund.
pub fn run_deposit_insurance_fund<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = depositInsuranceFundCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("depositInsuranceFund: invalid calldata"))?;

    require_admin(caller, context)?;

    if args.amount == 0 {
        return Err(perp_err("depositInsuranceFund: amount must be > 0"));
    }

    let mut account = storage::load_account(context, caller)?;
    // Derived-ooIM gate — a pure cash-out, see `addPositionMargin`.
    let available = crate::margin_view::derived_available_balance(context, caller)?;
    if !crate::margin_view::derived_can_afford(available, args.amount as i128) {
        return Err(perp_err(
            "depositInsuranceFund: insufficient perp wallet balance",
        ));
    }
    account.debit_perp(args.amount)?;
    storage::save_account(context, caller, account)?;

    let delta = checked_u64_to_i64(args.amount, "depositInsuranceFund: delta")?;
    let old_balance = storage::load_insurance_fund(context)?;
    let new_balance = old_balance
        .checked_add(args.amount)
        .ok_or_else(|| perp_err("depositInsuranceFund: balance overflow"))?;
    storage::save_insurance_fund(context, new_balance)?;

    context.log(Log {
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
pub fn run_withdraw_insurance_fund<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = withdrawInsuranceFundCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("withdrawInsuranceFund: invalid calldata"))?;

    require_admin(caller, context)?;

    if args.amount == 0 {
        return Err(perp_err("withdrawInsuranceFund: amount must be > 0"));
    }

    let delta = checked_u64_to_i64(args.amount, "withdrawInsuranceFund: delta")?;
    let balance = storage::load_insurance_fund(context)?;
    if args.amount > balance {
        return Err(perp_err(
            "withdrawInsuranceFund: amount exceeds fund balance",
        ));
    }
    let new_balance = balance - args.amount;
    storage::save_insurance_fund(context, new_balance)?;

    let mut account = storage::load_account(context, caller)?;
    account.credit_perp(args.amount)?;
    storage::save_account(context, caller, account)?;

    context.log(Log {
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
pub fn run_get_insurance_fund<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    getInsuranceFundCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getInsuranceFund: invalid calldata"))?;
    let balance = storage::load_insurance_fund(context)?;
    Ok(Bytes::from(getInsuranceFundCall::abi_encode_returns(
        &balance,
    )))
}

// ── Market manager role ───────────────────────────────────────────────────────

/// `setMarketManagerAddress(address manager)` — admin only. Zero revokes.
pub fn run_set_market_manager<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setMarketManagerAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setMarketManagerAddress: invalid calldata"))?;

    require_admin(caller, context)?;

    let previous = storage::load_market_manager(context)?;
    storage::save_market_manager(context, args.manager)?;

    context.log(Log {
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
pub fn run_get_market_manager<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    getMarketManagerAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarketManagerAddress: invalid calldata"))?;
    let manager = storage::load_market_manager(context)?;
    Ok(Bytes::from(
        getMarketManagerAddressCall::abi_encode_returns(&manager),
    ))
}

// ── Oracle address ────────────────────────────────────────────────────────────

/// `setOracleAddress(address oracle)` — admin only.
pub fn run_set_oracle_address<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setOracleAddressCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setOracleAddress: invalid calldata"))?;

    require_admin(caller, context)?;

    let previous = storage::load_oracle(context)?;
    storage::save_oracle(context, args.oracle)?;

    context.log(Log {
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
pub fn run_get_oracle_address<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
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
pub fn run_update_index_price<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = updateIndexPriceCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("updateIndexPrice: invalid calldata"))?;

    require_admin_or_oracle(caller, context)?;

    if args.indexPrice == 0 {
        return Err(perp_err("updateIndexPrice: indexPrice must be > 0"));
    }
    let market = storage::load_market_ref(context, args.marketId)?
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
            // Fold this epoch's funding into the cumulative index so positions
            // can settle lazily against the delta since their last touch.
            let index_step = (mark_price as i128)
                .checked_mul(rate as i128)
                .ok_or_else(|| perp_err("updateIndexPrice: funding index step overflow"))?;
            funding.cumulative_funding_index = funding
                .cumulative_funding_index
                .checked_add(index_step)
                .ok_or_else(|| perp_err("updateIndexPrice: funding index overflow"))?;
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

    // ── 5b. Protocol-automatic liquidation sweep ────────────────────────────────
    // The new mark + funding are now persisted. Liquidate any position that fell
    // below maintenance, synchronously inside this oracle tx (so Liquidation logs
    // attach to this receipt, and there is zero window before the sweep runs).
    run_liquidation_sweep(context, args.marketId, &market, mark_price)?;

    // ── 6. Emit events ────────────────────────────────────────────────────────
    context.log(Log {
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
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarkPriceUpdated {
            marketId: args.marketId,
            price: mark_price,
            updater: caller,
        }
        .to_log_data(),
    });
    if let Some((rate, avg_pi, sample_count)) = computed_rate {
        context.log(Log {
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
pub fn run_get_index_price<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
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
pub fn run_get_funding_state<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getFundingStateCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getFundingState: invalid calldata"))?;
    let state = storage::load_funding_state(context, args.marketId)?;
    let market = storage::load_market_ref(context, args.marketId)?
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
pub fn run_get_average_premium_index<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getAveragePremiumIndexCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAveragePremiumIndex: invalid calldata"))?;
    storage::load_market_ref(context, args.marketId)?
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
    (crate::types::PRICE_BASIS_WINDOW_SIZE as u64)
        .div_ceil(interval)
        .saturating_add(2) as usize
}

pub(crate) fn record_mid_price_sample_for_best_quote_change<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    best_bid: u64,
    best_ask: u64,
) -> Result<(), PerpError> {
    let mid_price = match (best_bid, best_ask) {
        (0, 0) => return Ok(()),
        (0, ask) => ask,
        (bid, 0) => bid,
        (bid, ask) => ((bid as u128 + ask as u128) / 2) as u64,
    };
    let timestamp: u64 = context.timestamp();

    let mut window = storage::load_price_basis_window(context, market_id)?;
    // Skip the (large) re-store when the observation changed nothing — i.e. every best-quote
    // change after the first within a block (same block timestamp). Avoids appending an unchanged
    // ~hundreds-of-bytes blob to the per-call commitment log and churning the overlay (P4/#17).
    if window.record_observation(timestamp, mid_price) {
        storage::save_price_basis_window(context, market_id, &window)?;
    }
    Ok(())
}

fn require_admin_or_oracle<H: PerpHost>(
    caller: Address,
    context: &mut H,
) -> Result<(), PerpError> {
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

fn require_admin_or_market_manager<H: PerpHost>(
    caller: Address,
    context: &mut H,
) -> Result<(), PerpError> {
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
