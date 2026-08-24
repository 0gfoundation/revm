//! Event payloads whose fields are DERIVED rather than stored.
//!
//! Exactly one lives here today: [`PositionChanged`]. It has SEVEN emit sites (the taker
//! settlement, the deferred maker settlement replayed at match flush, the liquidation residual
//! close, both ADL legs, `addPositionMargin` / `removePositionMargin`, and the funding settle),
//! and two of its fields — `entryPrice` and `unrealizedProfit` — are DERIVED from
//! `(amount, vQuoteBalance, mark)` rather than read off the position. Seven hand-rolled copies of
//! an entry-price division is the drift this codebase keeps deleting, so the derivation exists
//! once, here, and every site calls [`emit_position_changed`].
//!
//! Nothing in this module writes storage. Both derived fields come from `perp_core::math`
//! (`calc_entry_price`, `calc_value_i64`) so the log agrees digit-for-digit with what
//! `getPosition` / `getMarginInfo` report for the same state — there is no second definition of
//! either quantity in the engine.
//!
//! [`PositionChanged`]: crate::interface::IPerpDex::PositionChanged

use alloy_primitives::IntoLogData;
use primitives::{Address, Log};

use crate::{
    errors::perp_err,
    host::PerpHost,
    interface::IPerpDex,
    math::{calc_entry_price, calc_value_i64},
    types::{Market, PerpPosition},
    PerpError, PERP_DEX_ADDRESS,
};

/// Emit `PositionChanged` for `pos`, valued at `market`'s CURRENT mark.
///
/// Every caller that holds the threaded `Market` uses this form. `market.mark_price` IS the mark
/// each of those sites is operating at (the settlement paths read `let mark = market.mark_price`
/// verbatim, and `run_update_index_price` re-loads the `Market` after `save_mark_price` precisely
/// so the sweep's band centre and maintenance check are the same number — see the
/// `debug_assert_eq!(market.mark_price, mark_price)` there), so this never re-reads a mark that
/// could differ from the one the caller decided against.
#[inline]
pub(crate) fn emit_position_changed<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    pos: &PerpPosition,
    realized_pnl: i64,
    closed_quantity: u64,
) -> Result<(), PerpError> {
    emit_position_changed_at_mark(
        context,
        user,
        market.market_id,
        pos,
        market.mark_price,
        market.base_decimals,
        market.price_decimals,
        realized_pnl,
        closed_quantity,
    )
}

/// [`emit_position_changed`] for the one caller that does not hold a `Market`:
/// `funding::apply_funding_settlement`, which has already resolved the mark for its own
/// `FundingSettled` payload and passes THAT one, so both logs it emits agree.
///
/// # The two derived fields
///
/// * `entryPrice` = `-vQuoteBalance / amount`, in the market's `priceDecimals` fixed-point units
///   (the same scale every `price` field on this ABI uses), and **0 when flat** as the
///   `ACCOUNT_UPDATE.a.P[].ep` spec requires. `math::calc_entry_price` is the exact algebraic
///   inverse of `calc_value` — `v_quote_balance` is accumulated as `-calc_value(fill, qty)` at
///   fill time, so dividing it back out recovers the size-weighted AVERAGE entry, never the last
///   fill price. Its `amount == 0` early return is what makes the flat case a hard 0 rather than
///   a division.
/// * `unrealizedProfit` = `signedNotional + vQuoteBalance` — the SAME definition, byte for byte,
///   that `margin_view::position_margin_info` reports as `unrealizedProfit` on `getMarginInfo`
///   (see its "── unrealizedProfit ──" block: Binance's `positionAmt × (markPrice − entryPrice)`
///   with the only rounding being the truncation already inside `signedNotional`). It is NOT
///   re-derived from `entryPrice`, which would round twice. Sign follows from that: a long
///   (`amount > 0`) with `mark` above entry has `signedNotional > |vQuoteBalance|` and reports
///   positive; a short (`amount < 0`) has a negative `signedNotional` shrinking in magnitude as
///   the mark falls, so a falling mark reports positive there too.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_position_changed_at_mark<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    pos: &PerpPosition,
    mark_price: u64,
    base_decimals: u32,
    price_decimals: u32,
    realized_pnl: i64,
    closed_quantity: u64,
) -> Result<(), PerpError> {
    let entry_price = calc_entry_price(
        pos.amount,
        pos.v_quote_balance,
        base_decimals,
        price_decimals,
    )?;
    let unrealized_profit = calc_value_i64(mark_price, pos.amount, base_decimals, price_decimals)?
        .checked_add(pos.v_quote_balance)
        .ok_or_else(|| perp_err("PositionChanged: unrealized profit overflow"))?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user,
            marketId: market_id,
            amount: pos.amount,
            vQuoteBalance: pos.v_quote_balance,
            margin: pos.margin,
            leverage: pos.leverage,
            realizedPnl: realized_pnl,
            closedQuantity: closed_quantity,
            entryPrice: entry_price,
            unrealizedProfit: unrealized_profit,
            // PLACEHOLDERS — always zero. See the ABI comment on `PositionChanged`; do not read
            // these as data and do not populate them from here without adding the state they need.
            cumulativeRealizedPnl: 0,
            breakevenPrice: 0,
        }
        .to_log_data(),
    });
    Ok(())
}
