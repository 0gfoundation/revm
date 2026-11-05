//! Index price and mark price: the oracle and book-derived index updates, the basis mode, the
//! index / mark reads, and the oracle math helpers (including the mid-price sampler the book
//! calls on every best-quote change).

use super::*;

// ── Index price & mark price computation ──────────────────────────────────────

/// `updateIndexPrice(uint64 marketId, uint64 indexPrice, uint64 timestamp)`
///
/// Callable by admin or the configured oracle address.
///
/// Steps:
/// 1. Compute Price1, Price2, ContractPrice and take their median as mark price.
/// 2. Save the new index price state.
/// 4. Snap mark price to tick_size and persist it.
/// 5. Emit the single merged `MarkPriceUpdated` (index + mark + components + funding state).
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
    let prev_index_state = storage::load_index_price_state(context, args.marketId)?;
    if effective_timestamp <= prev_index_state.timestamp {
        return Ok(Bytes::new());
    }

    // ── Handover from the book-derived index ─────────────────────────────────
    // The external feed is authoritative again, but jumping straight to it would move the
    // mark — and therefore every maintenance check — by the whole accumulated difference in
    // one update. Blend linearly to it over INDEX_RAMP_SECS instead. The ramp always LANDS on
    // the external price (see `index_ramp_blend`), so this delays the handover, it does not
    // dilute it.
    let mut mode = storage::load_index_mode_state(context, args.marketId)?;
    if mode.from_book {
        // First external update after a book-driven stretch: anchor the ramp where the book
        // left the index.
        mode.ramp_from = prev_index_state.index_price;
        mode.ramp_start_ts = effective_timestamp;
        mode.from_book = false;
    }
    let index_price = if mode.ramp_from != 0 {
        let elapsed = effective_timestamp.saturating_sub(mode.ramp_start_ts);
        let blended = crate::math::index_ramp_blend(
            mode.ramp_from,
            args.indexPrice,
            elapsed,
            crate::math::INDEX_RAMP_SECS,
        );
        if elapsed >= crate::math::INDEX_RAMP_SECS {
            mode.ramp_from = 0;
            mode.ramp_start_ts = 0;
        }
        blended.clamp(1, market.max_price)
    } else {
        args.indexPrice
    };
    // Keep the accumulator in step with the exposed index on THIS path too, so the book path
    // never needs a seeding special case.
    mode.index_scaled = crate::math::scaled_from_index(index_price);

    apply_index_price_update(
        context,
        args.marketId,
        market,
        index_price,
        effective_timestamp,
        caller,
        prev_index_state,
        &mode,
    )
}

/// `updateIndexPriceFromBook(uint64 marketId)` — advance the index from this market's own book
/// while the external feed has nothing to publish.
///
/// Takes no timestamp on purpose. There is no external observation to stamp, and a
/// caller-supplied one would be doubly unsafe here: it reopens the future-dated freeze (the
/// staleness gate is the only bound on it), and because `alpha` is derived from the elapsed
/// time, a caller could submit many updates in ONE block with hand-rolled timestamps and walk
/// the EWMA to convergence — the per-update cap bounds the step, not the number of steps.
pub fn run_update_index_price_from_book<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = updateIndexPriceFromBookCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("updateIndexPriceFromBook: invalid calldata"))?;

    require_admin_or_oracle(caller, context)?;

    let market = storage::load_market_ref(context, args.marketId)?
        .ok_or_else(|| perp_err("updateIndexPriceFromBook: unknown market"))?;

    let effective_timestamp =
        align_price_update_timestamp(context.timestamp(), market.price_update_interval);
    let prev_index_state = storage::load_index_price_state(context, args.marketId)?;
    if effective_timestamp <= prev_index_state.timestamp {
        return Ok(Bytes::new());
    }
    // A closed-session oracle needs the session to have been open once: with no prior index
    // there is nothing to walk away from, and the mark would derive from the book alone.
    if prev_index_state.index_price == 0 {
        return Err(perp_err(
            "updateIndexPriceFromBook: market has no index price yet",
        ));
    }

    let mut mode = storage::load_index_mode_state(context, args.marketId)?;
    if mode.index_scaled == 0 {
        // Market last updated before this field existed, or never through the external path.
        mode.index_scaled = crate::math::scaled_from_index(prev_index_state.index_price);
    }

    // Everything below is pure arithmetic on values already loaded — no fallible step sits
    // between here and the first write, which is what the commit-only model requires.
    let dt = effective_timestamp.saturating_sub(prev_index_state.timestamp);
    let alpha = crate::math::ewma_alpha(dt);
    let hot = storage::load_market_hot(context, args.marketId)?;
    let ipd =
        crate::math::book_impact_delta(hot.best_bid, hot.best_ask, prev_index_state.index_price);
    mode.index_scaled =
        crate::math::ewma_index_step(mode.index_scaled, alpha, ipd, market.max_price);
    mode.from_book = true;
    // A book update supersedes any ramp still in flight (the feed went away again).
    mode.ramp_from = 0;
    mode.ramp_start_ts = 0;

    let index_price = crate::math::index_from_scaled(mode.index_scaled).clamp(1, market.max_price);

    apply_index_price_update(
        context,
        args.marketId,
        market,
        index_price,
        effective_timestamp,
        caller,
        prev_index_state,
        &mode,
    )
}

/// The half of an index-price update that does not care where the price came from.
///
/// Split out so `updateIndexPrice` and `updateIndexPriceFromBook` cannot drift: mark
/// recomputation, the index-history checkpoints, the premium accumulator, funding epoch
/// rollover, the out-of-band expiry sweep, the liquidation sweep and the events are one code
/// path with one write set, reached by two different ways of deriving `index_price`.
///
/// CONTRACT — the caller owns all of this, because it is selector-specific: authorisation,
/// calldata decoding, loading `market`, bounding `index_price` into `1..=market.max_price`,
/// flooring the timestamp, and the `effective_timestamp <= prev_index_state.timestamp`
/// staleness gate. `prev_index_state` is the state that gate read — it is consumed here as the
/// history checkpoint closing the previous interval, so re-loading it would cost a second
/// probe for a value the caller already holds.
///
/// Everything above the first `storage::save_*` is read-only, which is what lets a caller
/// validate-then-apply under the commit-only write model (#23).
#[allow(clippy::too_many_arguments)]
fn apply_index_price_update<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: std::sync::Arc<Market>,
    index_price: u64,
    effective_timestamp: u64,
    updater: Address,
    prev_index_state: IndexPriceState,
    mode: &IndexModeState,
) -> Result<Bytes, PerpError> {
    // Enforced once, for both paths. NOT a debug_assert: the premium index at
    // `(mark - index) * FUNDING_RATE_ONE / index` below divides with a plain `/`, and the
    // release profile is `panic = "abort"` — a zero here would take the NODE down rather than
    // revert the transaction.
    if index_price == 0 {
        return Err(crate::errors::perp_invariant_err(
            "index price update: index price must be > 0",
        ));
    }

    // ── 1. Compute mark price components ─────────────────────────────────────
    let mut funding = storage::load_funding_state(context, market_id)?;
    // Price1: index adjusted by funding basis.
    let price1 = compute_price1(index_price, &funding, &market, effective_timestamp);

    // Price2: index plus the top-of-book basis. WHICH basis is per market — see
    // `Market::basis_mode`. The USD-M markets stay on the window form and are not touched by
    // the EWMA branch at all; the two mechanisms coexist rather than one replacing the other.
    let mut legacy_history: Option<(crate::types::IndexPriceHistory, usize)> = None;
    let mut ewma_basis: Option<crate::types::PriceBasisEwma> = None;
    let ma_basis = if market.basis_mode == crate::types::BASIS_MODE_EWMA {
        // Sampled HERE — at oracle cadence, against the index being published — rather than on
        // every best-quote change against a historical index. Measuring contemporaneously is
        // what keeps this a basis rather than a lagged mid, and this cadence means a quote has
        // to survive an oracle tick before it can reach the mark at all.
        let mut basis = storage::load_price_basis_ewma(context, market_id)?;
        let hot = storage::load_market_hot(context, market_id)?;
        let mid = crate::math::top_of_book_mid(hot.best_bid, hot.best_ask);
        if mid != 0 {
            if basis.last_sample_ts == 0 {
                // Seed, rather than letting the first sample decay in from a basis of zero.
                basis.basis_scaled = (mid as i128 - index_price as i128)
                    .saturating_mul(crate::math::INDEX_SCALE as i128);
            } else {
                let dt = effective_timestamp.saturating_sub(basis.last_sample_ts);
                basis.basis_scaled =
                    crate::math::step_price_basis(basis.basis_scaled, dt, mid, index_price);
            }
            basis.last_sample_ts = effective_timestamp;
        }
        // Clamp price2's INPUT, never the mark's output: `contract_price` stays free, so the
        // mark still follows real trades at the real price.
        let b = crate::math::effective_basis(basis.basis_scaled, index_price);
        ewma_basis = Some(basis);
        b
    } else {
        let window = storage::load_price_basis_window(context, market_id)?;
        let max_index_checkpoints = max_index_price_checkpoints(market.price_update_interval);
        let mut index_history = storage::load_index_price_history(context, market_id)?;
        index_history.push(prev_index_state, max_index_checkpoints);
        let b = window.moving_average_basis(&index_history, effective_timestamp)?;
        legacy_history = Some((index_history, max_index_checkpoints));
        b
    };
    let index_price_i64 = checked_u64_to_i64(index_price, "index price update: indexPrice")?;
    let price2 = index_price_i64.saturating_add(ma_basis).max(1) as u64;

    // Contract price: latest traded price, falling back to index before any trade.
    let last_traded = storage::load_last_traded_price(context, market_id)?;
    let contract_price = if last_traded == 0 {
        index_price
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
        market_id,
        &IndexPriceState {
            index_price: index_price,
            timestamp: effective_timestamp,
        },
    )?;
    if let Some((mut index_history, max_index_checkpoints)) = legacy_history {
        index_history.push(
            IndexPriceState {
                index_price: index_price,
                timestamp: effective_timestamp,
            },
            max_index_checkpoints,
        );
        storage::save_index_price_history(context, market_id, &index_history)?;
    }
    if let Some(basis) = &ewma_basis {
        storage::save_price_basis_ewma(context, market_id, basis)?;
    }
    storage::save_index_mode_state(context, market_id, mode)?;
    storage::save_mark_price(context, market_id, mark_price)?;

    // ── 5. Accumulate premium index for funding rate calculation ──────────────
    let mut computed_rate: Option<(i64, i64, u64)> = None; // (rate, avg_pi, sample_count)
    // What `MarkPriceUpdated` publishes: the rate THIS epoch would settle at if it ended now.
    // Seeded with the last settled rate so a market with funding disabled (`funding_interval == 0`,
    // the block below never runs) still reports something meaningful rather than zero.
    let mut predicted_rate = funding.last_funding_rate;
    if market.funding_interval > 0 {
        let mut acc = storage::load_premium_accumulator(context, market_id)?;

        // PI = (mark_price − index_price) × FUNDING_RATE_ONE / index_price
        let pi = ((mark_price as i128 - index_price as i128)
            .checked_mul(FUNDING_RATE_ONE as i128)
            .ok_or_else(|| perp_err("index price update: premium index overflow"))?
            / index_price as i128)
            .try_into()
            .map_err(|_| perp_err("index price update: premium index exceeds i64::MAX"))?;

        // Initialize epoch on first oracle update. The first observed PI is the
        // first theoretical sample slot for this funding epoch.
        if acc.epoch_start_ts == 0 {
            acc.start_epoch(effective_timestamp, effective_timestamp, pi)?;
            if funding.next_funding_ts == 0 {
                funding.next_funding_ts = effective_timestamp
                    .checked_add(market.funding_interval)
                    .ok_or_else(|| perp_err("index price update: next funding timestamp overflow"))?;
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
                .ok_or_else(|| perp_err("index price update: funding index step overflow"))?;
            funding.cumulative_funding_index = funding
                .cumulative_funding_index
                .checked_add(index_step)
                .ok_or_else(|| perp_err("index price update: funding index overflow"))?;
            while funding.next_funding_ts <= effective_timestamp {
                funding.next_funding_ts = funding
                    .next_funding_ts
                    .checked_add(market.funding_interval)
                    .ok_or_else(|| perp_err("index price update: next funding timestamp overflow"))?;
            }
            storage::save_funding_state(context, market_id, &funding)?;

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
            storage::save_funding_state(context, market_id, &funding)?;
        }

        // Recomputed on EVERY push, from the accumulator as it now stands — after this sample was
        // folded in (`fill_slots_until`), or after the boundary reset re-seeded it with this sample.
        // One i128 division; no new storage, and `market.interest_rate` is already loaded.
        predicted_rate = calc_funding_rate(acc.average()?, market.interest_rate);

        storage::save_premium_accumulator(context, market_id, &acc)?;
    }

    // ── 5b. Protocol-automatic liquidation sweep ────────────────────────────────
    // The new mark + funding are now persisted. Liquidate any position that fell
    // below maintenance, synchronously inside this oracle tx (so Liquidation logs
    // attach to this receipt, and there is zero window before the sweep runs).
    //
    // RE-LOAD, do not reuse the `market` captured at the top of this function: that Arc predates
    // `save_mark_price` above, so its `mark_price` is the OLD one. The sweep threads this `Market`
    // down to `match_order`, which centres the fill-time price band on `market.mark_price` — so
    // reusing it would check maintenance against the NEW mark while banding the close around the
    // OLD one, disagreeing by exactly the move that triggered the sweep. At the default band that
    // puts the whole live book out of range after any large move: the close absorbs nothing, the
    // residual is insolvent, and it routes to ADL instead of the book and the insurance fund —
    // inverting the intended precedence precisely when the sweep matters most.
    let market = storage::load_market_ref(context, market_id)?
        .ok_or_else(|| crate::errors::perp_invariant_err("index price update: market vanished before sweep"))?;
    debug_assert_eq!(
        market.mark_price, mark_price,
        "liquidation sweep: the band centre must be the mark the maintenance check uses"
    );

    // ── 5c. Out-of-band resting-order expiry, BEFORE the sweep ──────────────────
    // The band just moved, which is the only thing that strands a level at the touch, so this is
    // where those levels are created and where they are collected. It runs FIRST because the levels
    // it removes are unmatchable to the sweep anyway (same band, same mark), so it cannot change
    // what the close absorbs — while the BBO cache is still provably live, which is what lets the
    // removal use the cheap cancel-path variant and get its top-of-book invariant check for free.
    // Capped; see MAX_BAND_EXPIRIES_PER_UPDATE.
    run_deferred_sweeps(context, market_id, &market, effective_timestamp, mark_price, true)?;

    // ── 6. Emit events ────────────────────────────────────────────────────────
    // ONE price event per update. This used to be `IndexPriceUpdated` immediately followed by
    // `MarkPriceUpdated` describing the same mark under a different field name, which every
    // consumer then had to de-duplicate; the two are merged.
    //
    // `fundingRate`/`nextFundingTime` are read off the `funding` local, which is the SAME
    // `FundingState` loaded at the top of this function for `compute_price1` and is post-update
    // (the epoch branch above assigns and saves through it; nothing between here and there writes
    // funding state). So it costs zero extra reads and it agrees with `getFundingState` — including
    // BETWEEN epochs, where no `FundingRateComputed` fires and a consumer previously had to cache
    // the last rate across pushes.
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarkPriceUpdated {
            marketId: market_id,
            markPrice: mark_price,
            indexPrice: index_price,
            fundingRate: predicted_rate,
            estimatedSettlePrice: crate::math::calc_estimated_settle_price(
                mark_price,
                predicted_rate,
            ),
            nextFundingTime: funding.next_funding_ts,
            price1,
            price2,
            // The FLOORED oracle timestamp: which price window this update belongs to, NOT when it
            // was delivered. A `@markPrice` stream's `E` is block time, not this.
            priceWindowTs: effective_timestamp,
            updater,
        }
        .to_log_data(),
    });
    if let Some((rate, avg_pi, sample_count)) = computed_rate {
        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::FundingRateComputed {
                marketId: market_id,
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


/// `setBasisMode(uint64 marketId, uint8 mode)`
pub fn run_set_basis_mode<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setBasisModeCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setBasisMode: invalid calldata"))?;

    require_admin_or_market_manager(caller, context)?;

    if args.mode > crate::types::BASIS_MODE_EWMA {
        return Err(perp_err("setBasisMode: unknown mode"));
    }
    let mut market = storage::load_market(context, args.marketId)?
        .ok_or_else(|| perp_err("setBasisMode: unknown market"))?;
    market.basis_mode = args.mode;
    storage::save_market(context, &market)?;
    Ok(Bytes::new())
}

/// `getIndexMode(uint64 marketId) returns (bool fromBook, uint128 indexScaled, uint64 rampFrom, uint64 rampStartTs)`
pub fn run_get_index_mode<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getIndexModeCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getIndexMode: invalid calldata"))?;
    let mode = storage::load_index_mode_state(context, args.marketId)?;
    Ok(Bytes::from(getIndexModeCall::abi_encode_returns(
        &getIndexModeReturn {
            fromBook: mode.from_book,
            indexScaled: mode.index_scaled,
            rampFrom: mode.ramp_from,
            rampStartTs: mode.ramp_start_ts,
            basisMode: storage::load_market_ref(context, args.marketId)?
                .map(|m| m.basis_mode)
                .unwrap_or(0),
        },
    )))
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
