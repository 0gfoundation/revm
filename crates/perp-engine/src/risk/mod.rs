//! Risk management: markets, leverage, mark price, positions, liquidation.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use crate::host::PerpHost;
use primitives::{keccak256, Address, Bytes, FixedBytes, Log};

use crate::{
    account::signing::{
        check_api_key_expiry, check_recv_window, gc_seen_buckets_best_effort, verify_ed25519,
    },
        errors::perp_err,
    events::emit_position_changed,
    risk::funding::{apply_funding_settlement, compute_funding_settlement},
    interface::IPerpDex::{
        self, addMarketCall, addPositionMarginCall, depositInsuranceFundCall, getAdminCall,
        getIndexModeCall, getIndexModeReturn, getIndexPriceCall,
        getIndexPriceReturn, getInsuranceFundCall,
        getMarginTiersCall, getMarginTiersReturn, getMarkPriceCall, getMarketCall,
        getMarketManagerAddressCall, getMarketReturn, getOracleAddressCall, getPositionCall,
        getPositionReturn, getSymbolConfigCall, getSymbolConfigReturn, initAdminCall,
        liquidateCall, removePositionMarginCall, setLeverageCall, setLeverageSignedCall,
        setBasisModeCall, setMarginTiersCall, setMarketManagerAddressCall, setOracleAddressCall, transferAdminCall,
        updateIndexPriceCall, updateIndexPriceFromBookCall, updateMarketCall,
        withdrawInsuranceFundCall,
    },
    math::{
        calc_funding_rate, calc_value, calc_value_i64, checked_u64_to_i64,
        is_above_maintenance_margin, leverage_brackets, max_leverage_for_notional,
        max_notional_for_leverage, tier_mmr_below_initial, tier_mmr_not_above, FUNDING_RATE_ONE,
    },
    storage,
    trading::{execute_liquidation_market_order, run_adl},
    types::{
        AccountUpdateReason, CancelReason, FundingState, IndexModeState, IndexPriceState,
        MarginTier, MarginTiers,
        Market, OrderStatus, PremiumIndexAccumulator, Side, MAX_LEVERAGE_HARD_CAP,
        MAX_MARGIN_TIERS,
    },
    PERP_DEX_ADDRESS,
    PerpError,
};

pub(crate) mod cross_liquidation;
pub mod funding;
pub(crate) mod negative_balance;

mod admin;
mod band_expiry;
mod index_price;
mod insurance;
mod leverage;
mod liquidation;
mod market;
mod position_margin;

// One file per section. The globs keep every `crate::risk::X` path resolving; the
// `pub(super)` items they carry are what the sibling sections and `risk::tests` (through
// `super::*`) reach.
pub use admin::*;
use band_expiry::*;
pub use funding::{run_drain_deferred_work, run_get_average_premium_index, run_get_funding_state};
pub use index_price::*;
pub use insurance::*;
pub use leverage::*;
pub use liquidation::*;
pub use market::*;
pub use position_margin::*;

// ── Internal helpers ──────────────────────────────────────────────────────────

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

/// Cancel every open order for `user` in `market`. Moves no money — a resting order escrows
/// nothing — so this only drops the owner's derived open-order requirement to zero.
///
/// `reason` rides every `OrderCancelled` this emits, and the two callers are NOT interchangeable:
/// a liquidation clears the LIQUIDATED owner's book (`Liquidation`), and an ADL fill clears the
/// selected COUNTERPARTY's book (`Adl`) — that owner was profitable, not liquidated.
pub(crate) fn cancel_all_orders_for_market<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    _market: &Market,
    reason: CancelReason,
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
                // The owner did not ask; see this function's `reason` doc.
                reason: reason as u8,
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
                // The owner did not ask; see this function's `reason` doc.
                reason: reason as u8,
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

    // ── Conditional (algo) orders ────────────────────────────────────────────────────────────
    //
    // They are NOT in the book, so everything above misses them entirely — the loops walk the
    // per-user BUY/SELL entry lists and the price levels, and an untriggered conditional appears
    // in neither. Without this a liquidated (or ADL'd) account keeps live stop-losses pointing at
    // a position that no longer exists, and they would fire later against whatever position the
    // owner opens next.
    //
    // Bounded by `algo::MAX_ALGO_ORDERS_PER_MARKET`, which is exactly why that cap exists: this
    // walk runs inside an already-capped sweep.
    for algo_id in storage::load_user_algo_orders(context, user, market_id)? {
        if let Some(order) = storage::load_order(context, &algo_id)? {
            crate::trading::algo::remove_algo_order(
                context, user, market_id, &algo_id, &order, reason,
            )?;
        }
    }

    // Both order lists were cleared → `Bid = Ask = 0`, so this position's derived open-order
    // requirement drops to 0 by itself. NOTHING is credited back to the wallet: a resting order
    // escrows nothing (neither margin nor fee), so a cancel — including this cancel-all — moves
    // no money at all. It only shrinks `Σ ooIM`, i.e. frees AVAILABLE, not balance.
    // Aggregates-only (`pos.amount` untouched), so the reservation-only route: it skips
    // `save_position`'s dead `amount` zero-crossing hooks and, per
    // `storage::mark_account_snapshot_dirty`, does not by itself mark the user for an
    // `AccountBalanceChanged` snapshot — cancelling publishes nothing. The liquidation that calls
    // this marks the user anyway, at its own position and wallet writes. Zeroed aggregates need no
    // read of the position: the route sets them on the stored one.
    storage::save_position_reservation_only(
        context,
        user,
        market_id,
        storage::SideAggregates::default(),
    )?;

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
