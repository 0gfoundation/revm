use alloy_primitives::IntoLogData;
use context::{ContextTr, JournalTr};
use primitives::{Address, FixedBytes, Log};

use super::match_order;
use crate::{
    perp_dex::{
        errors::{perp_err, perp_invariant_err},
        interface::IPerpDex,
        math::{
            calc_bankruptcy_price, calc_position_equity, calc_value, calc_value_i64,
            checked_u64_to_i64,
        },
        storage,
        types::{Market, Order, OrderStatus, OrderType, Side, TimeInForce},
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};

/// Execute the liquidation close as an internal market IOC order.
///
/// Runs the IOC against the book and returns the unfilled quantity. If the
/// book absorbs the entire position (`remaining == 0`) the position storage is
/// cleaned up here. If the book can only partially fill, the caller is
/// responsible for settling the residual (see `settle_liquidation_residual_at_mark_price`).
pub(crate) fn execute_liquidation_market_order<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market: &Market,
    side: Side,
    quantity: u64,
) -> Result<u64, PrecompileError> {
    let (order_id, bumped_nonce) = super::peek_order_id(context, user)?;
    let mut order = Order {
        owner: user.0 .0,
        market_id: market.market_id,
        side,
        price: 0,
        quantity,
        filled: 0,
        order_type: OrderType::Market,
        tif: TimeInForce::Ioc,
        status: OrderStatus::Open,
    };
    // commit-only #23: the close order is persisted ONCE after matching (below); the
    // OrderPlaced log keeps its original position (logs are EVM-journaled).
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderPlaced {
            user,
            marketId: market.market_id,
            orderId: FixedBytes(order_id),
            side: side as u8,
            price: 0,
            quantity,
            orderType: OrderType::Market as u8,
            tif: TimeInForce::Ioc as u8,
            clientOrderId: FixedBytes::default(),
        }
        .to_log_data(),
    });

    let remaining = match_order(
        context,
        user,
        &order_id,
        market.market_id,
        side,
        0,
        quantity,
        OrderType::Market,
        TimeInForce::Ioc,
        market,
        // Liquidation close: waive the taker trading fee (the liquidated user pays
        // the clearance fee to the IF instead). Also prevents the close from
        // reverting when the underwater user cannot cover a taker fee.
        true,
        false, // liquidation close (IOC): never rests
        &mut order,
        // The liquidation close emits its OrderPlaced itself (above), so there is nothing buffered
        // for the match apply to flush — its log behavior is unchanged.
        &mut None,
    )?;
    // delete-on-terminal: the liquidation close is an IOC that never rests — it exists only to
    // drive the match + emit OrderPlaced/Trade. Its record is dropped (never a live/queryable
    // resting order; any residual is settled at mark price by the caller).
    storage::delete_order(context, &order_id)?;
    super::commit_order_nonce(context, user, bumped_nonce)?;

    if remaining == 0 {
        // Full fill: clean up any rounding residuals left in the position.
        let mut pos = storage::load_position(context, user, market.market_id)?;
        if pos.amount != 0 {
            return Err(perp_invariant_err(
                "liquidation market order: full fill but position not zero",
            ));
        }
        pos.v_quote_balance = 0;
        pos.margin = 0;
        storage::save_position(context, user, market.market_id, &pos)?;
    }

    Ok(remaining)
}

/// Closes the residual position (the part the orderbook could not absorb) at mark price.
///
/// Called when `execute_liquidation_market_order` returns `remaining > 0`. The position
/// still holds the proportional `margin` and `v_quote_balance` for the residual. This
/// function applies those to the wallet and zeroes the position.
///
/// Isolated margin: the loss is contained to the position's margin; any shortfall
/// beyond it is bad debt routed directly to the Insurance Fund here. The wallet is
/// never debited by the residual loss (only credited if the residual is solvent).
pub(crate) fn settle_liquidation_residual_at_mark_price<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market: &Market,
    liquidation_side: Side,
    mark_price: u64,
) -> Result<(), PrecompileError> {
    let mut pos = storage::load_position(context, user, market.market_id)?;
    let mut account = storage::load_account(context, user)?;

    let residual_value = calc_value(
        mark_price,
        pos.amount.unsigned_abs(),
        market.base_decimals,
        market.price_decimals,
    )?;
    let residual_value_i64 = checked_u64_to_i64(residual_value, "liquidation: residual value")?;

    // Selling a long → receive quote (+); buying a short → pay quote (-).
    let close_quote_delta = if liquidation_side == Side::Sell {
        residual_value_i64
    } else {
        -residual_value_i64
    };

    let closed_quantity = pos.amount.unsigned_abs();
    let realized_pnl = pos
        .v_quote_balance
        .checked_add(close_quote_delta)
        .ok_or_else(|| perp_err("liquidation: residual realized PnL overflow"))?;
    let settlement_credit = pos
        .margin
        .checked_add(realized_pnl)
        .ok_or_else(|| perp_err("liquidation: residual close credit overflow"))?;

    // Isolated margin: a profitable/solvent residual returns equity to the wallet; an
    // insolvent residual (loss exceeds the position's remaining margin) does NOT debit
    // the wallet — the shortfall is bad debt routed directly to the Insurance Fund.
    let bad_debt = if settlement_credit >= 0 {
        account.perp_wallet_balance = account
            .perp_wallet_balance
            .saturating_add(settlement_credit);
        0u64
    } else {
        settlement_credit.unsigned_abs()
    };

    pos.amount = 0;
    pos.v_quote_balance = 0;
    pos.margin = 0;

    storage::save_position(context, user, market.market_id, &pos)?;
    storage::save_account(context, user, account)?;

    super::settlement::absorb_bad_debt_into_insurance_fund(context, market.market_id, bad_debt)?;
    emit_position_changed(
        context,
        user,
        market.market_id,
        &pos,
        realized_pnl,
        closed_quantity,
    );

    Ok(())
}

/// Auto-deleveraging (ADL, scheme X): close a liquidated position's INSOLVENT
/// book-unfillable residual as a forced trade against opposite-side holders at the
/// residual's bankruptcy price `P_b`. No Insurance Fund, no mint: the loser closes at
/// the price where its own equity is exactly 0 (no bad debt), and each opposite holder
/// that can absorb at `P_b` without going insolvent gives up exactly its share of the
/// shortfall. Both legs of every fill use the SAME single-floored `calc_value(P_b,
/// take)`, so Σ vQuote and Σ amount are conserved (a real trade, not a synthetic close).
///
/// Runs inside the liquidation sweep, bounded by the shared `budget` (total ADL fills
/// for the whole `updateIndexPrice`). Any residual left unclosed (budget exhausted, or
/// not enough deeply-in-profit opposite holders) stays open and is re-swept next update.
///
/// v1 simplifications: (a) opposite holders with open orders (`margin_reserved` /
/// `fee_reserved` > 0) are excluded, so no flip-aware reservation recompute / order
/// auto-cancel is needed — the residual's natural counterparties are the off-book
/// holders anyway; (b) opposite holders that are themselves below water are skipped
/// (the sweep liquidates them), never forced into bad debt; cascades from ADL'ing a
/// thin winner resolve on a later sweep, not by in-`run_adl` recursion.
pub(crate) fn run_adl<CTX: ContextTr>(
    context: &mut CTX,
    loser: Address,
    market: &Market,
    mark_price: u64,
    budget: &mut u32,
) -> Result<(), PrecompileError> {
    let mut loser_pos = storage::load_position(context, loser, market.market_id)?;
    if loser_pos.amount == 0 || *budget == 0 {
        return Ok(());
    }
    let bd = market.base_decimals;
    let pd = market.price_decimals;
    let p_b = calc_bankruptcy_price(
        loser_pos.amount,
        loser_pos.v_quote_balance,
        loser_pos.margin,
        bd,
        pd,
    )?;
    if p_b == 0 {
        return Ok(()); // <=1x residual is never insolvent — nothing to ADL
    }
    let loser_is_long = loser_pos.amount > 0;

    // Enumerate + rank opposite-side candidates ONCE (rank stable within this call).
    let registry = storage::load_position_registry(context, market.market_id)?;
    let mut cands: Vec<(Address, i128, i128)> = Vec::new(); // (addr, uPnL@mark, equity@mark)
    for user in registry {
        if user == loser {
            continue;
        }
        let wp = storage::load_position(context, user, market.market_id)?;
        if wp.amount == 0 || (wp.amount > 0) == loser_is_long {
            continue; // flat or same side as the loser
        }
        if wp.margin_reserved != 0 || wp.fee_reserved != 0 {
            continue; // v1: has open orders — skip (avoid reservation recompute)
        }
        let eq_mark =
            calc_position_equity(mark_price, wp.amount, wp.v_quote_balance, wp.margin, bd, pd)?;
        if eq_mark <= 0 {
            continue; // itself liquidatable — leave to the sweep
        }
        let eq_pb = calc_position_equity(p_b, wp.amount, wp.v_quote_balance, wp.margin, bd, pd)?;
        if eq_pb < 0 {
            continue; // cannot absorb at P_b without going insolvent
        }
        let notional = calc_value_i64(mark_price, wp.amount, bd, pd)? as i128;
        cands.push((user, notional + wp.v_quote_balance as i128, eq_mark as i128));
    }
    // ROE = uPnL/equity DESC (equity>0), Address ASC tie-break; cross-multiply, no div.
    // `saturating_mul` is deterministic and cannot panic — both products only approach
    // i128::MAX at the joint i64 extremes, where a saturated tie deterministically falls
    // through to the Address tie-break (equal ROE ranking either way).
    cands.sort_by(|a, b| {
        let lhs = a.1.saturating_mul(b.2); // uPnL_A * eq_B
        let rhs = b.1.saturating_mul(a.2); // uPnL_B * eq_A
        rhs.cmp(&lhs).then_with(|| a.0.cmp(&b.0))
    });

    let mut remaining = loser_pos.amount.unsigned_abs();
    let loser_close_is_buy = !loser_is_long; // long closes by selling, short by buying
    let winner_close_is_buy = loser_is_long; // opposite side
    let mut loser_account = storage::load_account(context, loser)?;
    let mut did_any = false;

    for (winner, _, _) in cands {
        if *budget == 0 || remaining == 0 {
            break;
        }
        let mut winner_pos = storage::load_position(context, winner, market.market_id)?;
        let mut winner_account = storage::load_account(context, winner)?;
        let take = winner_pos.amount.unsigned_abs().min(remaining);
        let Some(fill) = adl_fill(
            &mut loser_pos,
            &mut loser_account.perp_wallet_balance,
            loser_close_is_buy,
            &mut winner_pos,
            &mut winner_account.perp_wallet_balance,
            winner_close_is_buy,
            take,
            p_b,
            bd,
            pd,
        )?
        else {
            continue; // no clean (bad-debt-free) fill possible — skip this winner
        };
        storage::save_position(context, winner, market.market_id, &winner_pos)?;
        storage::save_account(context, winner, winner_account)?;
        context.journal_mut().log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::Adl {
                liquidatedUser: loser,
                adlUser: winner,
                marketId: market.market_id,
                qty: fill.quantity,
                price: p_b,
            }
            .to_log_data(),
        });
        emit_position_changed(
            context,
            loser,
            market.market_id,
            &loser_pos,
            fill.loser_realized_pnl,
            fill.quantity,
        );
        emit_position_changed(
            context,
            winner,
            market.market_id,
            &winner_pos,
            fill.winner_realized_pnl,
            fill.quantity,
        );
        remaining -= fill.quantity;
        *budget -= 1;
        did_any = true;
    }

    if did_any {
        // Loser residual reduced by the ADL'd quantity. If fully closed the registry
        // hook drops it; otherwise it stays open and is re-swept next update.
        storage::save_position(context, loser, market.market_id, &loser_pos)?;
        storage::save_account(context, loser, loser_account)?;
    }
    Ok(())
}

/// One ADL forced trade: close `take` of both the loser and one opposite holder at
/// `p_b`, shrinking `take` (bounded) so NEITHER side realizes bad debt from sub-unit
/// flooring. Both legs use the SAME single-floored `calc_value(p_b, take)`. Returns the
/// the committed quantity and each participant's realized PnL, or `None` if no
/// clean fill is possible near `take`.
struct AdlFillOutcome {
    quantity: u64,
    loser_realized_pnl: i64,
    winner_realized_pnl: i64,
}

#[allow(clippy::too_many_arguments)]
fn adl_fill(
    loser_pos: &mut crate::perp_dex::types::PerpPosition,
    loser_wallet: &mut i64,
    loser_close_is_buy: bool,
    winner_pos: &mut crate::perp_dex::types::PerpPosition,
    winner_wallet: &mut i64,
    winner_close_is_buy: bool,
    take: u64,
    p_b: u64,
    bd: u32,
    pd: u32,
) -> Result<Option<AdlFillOutcome>, PrecompileError> {
    use super::settlement::apply_position_fill;
    let floor = take.saturating_sub(4); // try take, take-1, .., take-4 (dust is <=1-2)
    let mut t = take;
    while t > 0 && t > floor {
        let v = calc_value(p_b, t, bd, pd)?;
        // Trial on clones; commit only if BOTH sides are bad-debt free.
        let mut lp = loser_pos.clone();
        let mut lw = *loser_wallet;
        let loser_outcome = apply_position_fill(&mut lp, &mut lw, t, v, 0, 0, loser_close_is_buy)?;
        let mut wp = winner_pos.clone();
        let mut ww = *winner_wallet;
        let winner_outcome =
            apply_position_fill(&mut wp, &mut ww, t, v, 0, 0, winner_close_is_buy)?;
        if loser_outcome.bad_debt == 0 && winner_outcome.bad_debt == 0 {
            *loser_pos = lp;
            *loser_wallet = lw;
            *winner_pos = wp;
            *winner_wallet = ww;
            return Ok(Some(AdlFillOutcome {
                quantity: t,
                loser_realized_pnl: loser_outcome.realized_pnl,
                winner_realized_pnl: winner_outcome.realized_pnl,
            }));
        }
        t -= 1;
    }
    Ok(None)
}

fn emit_position_changed<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    pos: &crate::perp_dex::types::PerpPosition,
    realized_pnl: i64,
    closed_quantity: u64,
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
            realizedPnl: realized_pnl,
            closedQuantity: closed_quantity,
        }
        .to_log_data(),
    });
}
