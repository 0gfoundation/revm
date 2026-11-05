//! Market management: add / update a market and its margin tiers, and the market reads.

use super::*;

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
        // Born on the legacy window basis, like every market that already exists. A market is
        // opted into the EWMA basis afterwards by `setBasisMode`, for the same reason the tier
        // table is set by `setMarginTiers`: `addMarket` is already 14 args.
        basis_mode: crate::types::BASIS_MODE_WINDOW,
        // A brand-new market has no book and no positions, so no sweep has anything to defer.
        deferred_work: 0,
        cumulative_funding_index: 0,
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
    // ── NO `MarkPriceUpdated` HERE. DO NOT REINSTATE IT. ────────────────────────────────────────
    // The initial mark is published by `MarketAdded` above (`initialMarkPrice`), and a consumer has
    // to read `MarketAdded` anyway for symbol/decimals/tick, so the seed is free there.
    //
    // It used to emit one, on the theory that mark price should have a single event source for a
    // market's whole lifetime. It cannot, honestly: `addMarket` writes no `IndexPriceState`, so at
    // this point there is no index price, no `price1`/`price2` and no funding epoch — five of the
    // nine fields would be 0. They would be TRUE zeros, not placeholders, but that is not the
    // problem: it would make `MarkPriceUpdated` a type whose fields are only conditionally
    // meaningful, and a backend that handles every one of them uniformly would publish
    // `index = 0, rate = 0, nextFundingTime = 0` once per market with no error raised anywhere.
    // A silent special case on the hot path is worse than a second, obvious seeding source.
    //
    // Pinned by `risk::tests::add_market_emits_no_price_event`.

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

/// `setMarginTiers(uint64 marketId, uint64[] lowerBounds, uint32[] maxLeverages,
/// uint32[] maintMarginRatesPpm, uint64 notionalCap)`
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
    if args.maintMarginRatesPpm.len() != args.lowerBounds.len() {
        return Err(perp_err(
            "setMarginTiers: maintMarginRatesPpm and lowerBounds length mismatch",
        ));
    }

    let rows: Vec<MarginTier> = args
        .lowerBounds
        .iter()
        .zip(args.maxLeverages.iter())
        .zip(args.maintMarginRatesPpm.iter())
        .map(
            |((&lower_bound_notional, &max_leverage), &maint_margin_rate_ppm)| MarginTier {
                lower_bound_notional,
                max_leverage,
                maint_margin_rate_ppm,
            },
        )
        .collect();
    // Rates are checked on the EFFECTIVE value (an explicit ppm, or the derived `1/(2L)` when
    // the column is 0), exactly: `math::tier_mmr_*` cross-multiply fractions, never round.
    // IM > MM per tier (a derived rate always satisfies it) and non-decreasing across tiers
    // together give `MM(N) <= N · mmr(n) < N / L_n` inside every band.
    if !rows.iter().all(tier_mmr_below_initial) {
        return Err(perp_err(
            "setMarginTiers: maintMarginRatePpm must be below 1000000 / maxLeverage (IM > MM)",
        ));
    }
    if rows.windows(2).any(|w| !tier_mmr_not_above(&w[0], &w[1])) {
        return Err(perp_err(
            "setMarginTiers: maintenance margin rates must be non-decreasing",
        ));
    }
    // The last bracket's `notionalCap`. `0` and `type(uint64).max` both mean UNBOUNDED and are
    // stored as `0`: the second is what `getMarginTiers` reports for an unbounded last band, so a
    // table read back and written again is unchanged. Anything else must lie strictly above the
    // last floor, or the last band would be empty.
    let notional_cap = match args.notionalCap {
        u64::MAX => 0,
        cap => cap,
    };
    let last_floor = rows.last().map_or(0, |t| t.lower_bound_notional);
    if notional_cap != 0 && notional_cap <= last_floor {
        return Err(perp_err(
            "setMarginTiers: notionalCap must be above the last lowerBound (0 = unbounded)",
        ));
    }
    // Unreachable: the length bounds above are exactly `from_tiers`' precondition.
    let tiers = MarginTiers::from_tiers(&rows)
        .ok_or_else(|| perp_err("setMarginTiers: invalid tier count"))?
        .with_notional_cap(notional_cap);

    // ── APPLY (all rejects passed) ──
    market.tiers = tiers;
    storage::save_market(context, &market)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarginTiersUpdated {
            marketId: args.marketId,
            lowerBounds: args.lowerBounds,
            maxLeverages: args.maxLeverages,
            maintMarginRatesPpm: args.maintMarginRatesPpm,
            notionalCap: notional_cap,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `getMarginTiers(uint64 marketId) returns (uint64[] lowerBounds, uint32[] maxLeverages,
/// uint32[] maintMarginRatesPpm, uint32[] effectiveMaintMarginRatesPpm, uint64[] cums,
/// uint64[] notionalCaps)`
///
/// The stored table, then Binance's derived `leverageBracket` columns from
/// `math::leverage_brackets` — the one implementation of `cum`, next to the slice form it
/// describes. Zero extra loads: the table lives inside the `Market` blob.
pub fn run_get_margin_tiers<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getMarginTiersCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMarginTiers: invalid calldata"))?;

    let market = storage::load_market_ref(context, args.marketId)?
        .ok_or_else(|| perp_err("getMarginTiers: unknown market"))?;

    let table = market.tiers.as_slice();
    let brackets = leverage_brackets(&market.tiers);
    Ok(Bytes::from(getMarginTiersCall::abi_encode_returns(
        &getMarginTiersReturn {
            lowerBounds: table.iter().map(|t| t.lower_bound_notional).collect(),
            maxLeverages: table.iter().map(|t| t.max_leverage).collect(),
            maintMarginRatesPpm: table.iter().map(|t| t.maint_margin_rate_ppm).collect(),
            effectiveMaintMarginRatesPpm: brackets
                .iter()
                .map(|b| b.maint_margin_rate_ppm)
                .collect(),
            cums: brackets.iter().map(|b| b.cum).collect(),
            notionalCaps: brackets.iter().map(|b| b.notional_cap).collect(),
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

// ── Internal helpers ──────────────────────────────────────────────────────────

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
