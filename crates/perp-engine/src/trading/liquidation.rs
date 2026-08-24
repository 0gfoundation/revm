use alloy_primitives::IntoLogData;
use crate::host::PerpHost;
use primitives::{Address, FixedBytes, Log};

use super::match_order;
use crate::{
        errors::{perp_err, perp_invariant_err},
    events::emit_position_changed,
    interface::IPerpDex,
    math::{
        calc_bankruptcy_price, calc_position_equity, calc_value, calc_value_i64,
        checked_u64_to_i64,
    },
    storage,
    types::{AccountUpdateReason, Market, Order, OrderKind, OrderStatus, Side},
    PERP_DEX_ADDRESS,
    PerpError,
};

/// Execute the liquidation close as an internal market IOC order.
///
/// Runs the IOC against the book and returns the unfilled quantity. If the
/// book absorbs the entire position (`remaining == 0`) the position storage is
/// cleaned up here. If the book can only partially fill, the caller is
/// responsible for settling the residual (see `settle_liquidation_residual_at_mark_price`).
pub(crate) fn execute_liquidation_market_order<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    side: Side,
    quantity: u64,
) -> Result<u64, PerpError> {
    let (order_id, bumped_nonce) = super::peek_order_id(context, user)?;
    // A liquidation close IS a market order: immediate-or-cancel, bounded by the price band, never
    // resting. One `OrderKind` says so, and the record/event's two wire fields are derived from it.
    let kind = OrderKind::Market;
    let mut order = Order {
        owner: user.0 .0,
        market_id: market.market_id,
        side,
        price: 0,
        quantity,
        filled: 0,
        order_type: kind.order_type(),
        tif: kind.tif(),
        status: OrderStatus::Open,
    };
    // commit-only #23: the close order is persisted ONCE after matching (below); the
    // OrderPlaced log keeps its original position (logs are EVM-journaled).
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderPlaced {
            user,
            marketId: market.market_id,
            orderId: FixedBytes(order_id),
            side: side as u8,
            price: 0,
            quantity,
            orderType: kind.order_type() as u8,
            tif: kind.tif() as u8,
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
        kind, // market order: never rests, never all-or-nothing
        market,
        // Liquidation close: waive the taker trading fee (the liquidated user pays
        // the clearance fee to the IF instead). Also prevents the close from
        // reverting when the underwater user cannot cover a taker fee.
        true,
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
        // ⚠️ Only when there is really a residual to clear. The common case — the fill zeroed both
        // legs exactly — used to take this write anyway, and the write MARKS the user, so the
        // end-of-call drain published a second `AccountBalanceChanged` for the liquidated user that
        // repeated the close group's payload with zero position rows: a duplicate with no news in
        // it. (The delta is unaffected: the match flush already wrote this position key with these
        // same bytes, which is why the block commitment does not move.)
        //
        // When a residual IS cleared the write stands, the user stays marked, and the drain
        // publishes a legitimate 0-position group whose `totalWalletBalance` differs by the residual.
        if pos.v_quote_balance != 0 || pos.margin != 0 {
            pos.v_quote_balance = 0;
            pos.margin = 0;
            // `Adjustment`: a protocol-side WRITE-OFF, not the order's fill. The fills themselves
            // were published by the taker path (`Order`) and that emit cleared the mark; what is left
            // here is rounding residue going nowhere, so the drain's 0-position group for it is a
            // forced balance change with no order behind it. When the clearance fee also fires this
            // merges with its `InsuranceClear` to `Multiple` — see
            // `storage::mark_account_snapshot_dirty`.
            storage::save_position(
                context,
                user,
                market.market_id,
                &pos,
                AccountUpdateReason::Adjustment,
            )?;
        }
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
pub(crate) fn settle_liquidation_residual_at_mark_price<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    liquidation_side: Side,
    mark_price: u64,
) -> Result<(), PerpError> {
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

    // `Adjustment`: the residual is closed at MARK with no counterparty and no order — nothing was
    // matched, the protocol simply valued what the book could not absorb and zeroed the position.
    // (Not `InsuranceClear`, even though the insolvent branch routes a shortfall to the fund: the
    // solvent branch returns equity to the wallet and touches no fund at all, and one publish site
    // reporting two different reasons for two branches of the same mechanism would make the field
    // harder to read, not easier. The `InsuranceFundChanged` / `InsuranceFundDepleted` rows
    // immediately before this group already say whether the fund was involved.)
    storage::save_position(
        context,
        user,
        market.market_id,
        &pos,
        AccountUpdateReason::Adjustment,
    )?;
    storage::save_account(context, user, account, AccountUpdateReason::Adjustment)?;

    super::settlement::absorb_bad_debt_into_insurance_fund(context, market.market_id, bad_debt)?;
    // ── The residual close's `ACCOUNT_UPDATE` group: header, then the one position row ──────────
    //
    // It has to come AFTER `absorb_bad_debt_into_insurance_fund`, whose `InsuranceFundChanged` /
    // `InsuranceFundDepleted` rows would otherwise terminate the group and orphan the row below
    // (`crate::events`). Off the SETTLED store: both writes above have landed, so this is literally
    // the post-close account.
    //
    // The mark is cleared here, and `liquidate_position` may legitimately re-mark this user
    // afterwards by charging its clearance fee (`save_account`) — the drain then publishes a second,
    // 0-position group for the settled end state. Two pushes for two economic events; that is the
    // fail-safe direction and it is what Binance does for a liquidation too.
    storage::publish_account_snapshot_now(context, user, AccountUpdateReason::Adjustment)?;
    // `market.mark_price == mark_price` here: `liquidate_position` is the only caller and it takes
    // both from the same re-loaded `Market` (its own `debug_assert_eq!` pins that), so valuing the
    // log at `market`'s mark is valuing it at the mark this close settled against.
    emit_position_changed(context, user, market, &pos, realized_pnl, closed_quantity)?;

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
/// v1 simplifications: (a) opposite holders holding ANY resting order (asked of the order
/// lists directly) are excluded, so no flip-aware reservation recompute / order
/// auto-cancel is needed — the residual's natural counterparties are the off-book
/// holders anyway; (b) opposite holders that are themselves below water are skipped
/// (the sweep liquidates them), never forced into bad debt; cascades from ADL'ing a
/// thin winner resolve on a later sweep, not by in-`run_adl` recursion.
pub(crate) fn run_adl<H: PerpHost>(
    context: &mut H,
    loser: Address,
    market: &Market,
    mark_price: u64,
    budget: &mut u32,
) -> Result<(), PerpError> {
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
        // v1: skip holders with ANY resting order — an ADL fill moves their position, which
        // re-prices every order they have resting, and v1 does not want to reason about that.
        // Asked DIRECTLY of the order lists, never proxied through "their open-order requirement
        // is non-zero": a PURE-REDUCE order (fully absorbed by the position) requires ZERO margin,
        // so a requirement-based proxy would let such a holder through.
        if !storage::load_buy_orders_ref(context, user, market.market_id)?.is_empty()
            || !storage::load_sell_orders_ref(context, user, market.market_id)?.is_empty()
        {
            continue;
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
    // `Σ pos.margin` over the loser's OTHER markets, captured in the one breath where both terms are
    // in hand and both are still pre-ADL. The loser's position/account are written ONCE after the
    // fill loop, so each per-fill header for the loser has to be derived from the working copies —
    // `margin_view::wallet_balances_from_parts` documents the shape and why the difference is the
    // right thing to hold. (The WINNER needs none of this: its two writes land inside the loop,
    // before its own header.)
    let loser_other_market_margin = loser_account
        .total_position_margin
        .checked_sub(loser_pos.margin)
        .ok_or_else(|| perp_err("adl: Σ position margin underflow"))?;
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
        // `Adjustment` for BOTH ADL legs: a forced trade at the bankruptcy price with no order on
        // either side. The winner in particular never placed one — candidates holding ANY resting
        // order are excluded above — and there is no `Trade` row for them either, only `Adl`, so
        // labelling this `Order` would send a consumer looking for an order id that does not exist.
        storage::save_position(
            context,
            winner,
            market.market_id,
            &winner_pos,
            AccountUpdateReason::Adjustment,
        )?;
        storage::save_account(context, winner, winner_account, AccountUpdateReason::Adjustment)?;
        context.log(Log {
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
        // ── TWO `ACCOUNT_UPDATE` groups, one per party ───────────────────────────────────────────
        //
        // An ADL fill is one economic event for the loser and a different one for the winner, so it
        // publishes two headers, each immediately followed by its own party's position row. Merging
        // them is not an option and never was: the adjacency rule (`crate::events`) makes the second
        // row belong to the first header, i.e. an indexer would book the winner's position onto the
        // loser's account. The `Adl` row above sits outside both groups.
        //
        // Both legs are valued at the market's CURRENT mark, not at the ADL price `p_b` the fill
        // executed at: `unrealizedProfit` is by definition mark-to-market on what is LEFT open,
        // and the fill's realised part is reported separately as `realizedPnl`.
        //
        // The loser's header comes off the WORKING copies (its writes are after the loop); the
        // winner's off the settled store, which its two writes just above made current. The trailing
        // `did_any` writes re-mark the loser, so the drain adds one 0-position group for its settled
        // end state — correct, and the fail-safe direction.
        let loser_balances = crate::margin_view::wallet_balances_from_parts(
            &loser_account,
            loser_other_market_margin,
            loser_pos.margin,
        )?;
        storage::log_account_snapshot(context, loser, &loser_balances, AccountUpdateReason::Adjustment);
        emit_position_changed(
            context,
            loser,
            market,
            &loser_pos,
            fill.loser_realized_pnl,
            fill.quantity,
        )?;
        storage::publish_account_snapshot_now(context, winner, AccountUpdateReason::Adjustment)?;
        emit_position_changed(
            context,
            winner,
            market,
            &winner_pos,
            fill.winner_realized_pnl,
            fill.quantity,
        )?;
        remaining -= fill.quantity;
        *budget -= 1;
        did_any = true;
    }

    if did_any {
        // Loser residual reduced by the ADL'd quantity. If fully closed the registry
        // hook drops it; otherwise it stays open and is re-swept next update.
        storage::save_position(
            context,
            loser,
            market.market_id,
            &loser_pos,
            AccountUpdateReason::Adjustment,
        )?;
        storage::save_account(context, loser, loser_account, AccountUpdateReason::Adjustment)?;
        // The two writes above MARK the loser — but they persist EXACTLY the working copies the
        // LAST per-fill header was derived from, so the end-of-call drain would repeat that payload
        // as a 0-position group with no news in it. Clear it. Unconditional is safe here in the way
        // the flush's gate is not: `loser_pos` / `loser_account` are not touched between the last
        // header and these writes, so equality is structural rather than incidental. A later
        // liquidation leg (the clearance fee) writes the account again, re-marks, and the settled
        // end state still gets published.
        storage::clear_account_snapshot_mark(context, loser);
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
    loser_pos: &mut crate::types::PerpPosition,
    loser_wallet: &mut i64,
    loser_close_is_buy: bool,
    winner_pos: &mut crate::types::PerpPosition,
    winner_wallet: &mut i64,
    winner_close_is_buy: bool,
    take: u64,
    p_b: u64,
    bd: u32,
    pd: u32,
) -> Result<Option<AdlFillOutcome>, PerpError> {
    use super::settlement::{apply_position_fill, OpeningMarginFunding};
    let floor = take.saturating_sub(4); // try take, take-1, .., take-4 (dust is <=1-2)
    let mut t = take;
    while t > 0 && t > floor {
        let v = calc_value(p_b, t, bd, pd)?;
        // Trial on clones; commit only if BOTH sides are bad-debt free.
        // Both legs are PURE CLOSES (`opening_qty == 0`), so the funding mode is inert — ADL never
        // opens, and therefore never short-funds, a silo.
        let mut lp = loser_pos.clone();
        let mut lw = *loser_wallet;
        let loser_outcome = apply_position_fill(
            &mut lp,
            &mut lw,
            t,
            v,
            0,
            0,
            loser_close_is_buy,
            OpeningMarginFunding::Requirement,
        )?;
        let mut wp = winner_pos.clone();
        let mut ww = *winner_wallet;
        let winner_outcome = apply_position_fill(
            &mut wp,
            &mut ww,
            t,
            v,
            0,
            0,
            winner_close_is_buy,
            OpeningMarginFunding::Requirement,
        )?;
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

// `emit_position_changed` lives in `crate::events` — one derivation for all seven emit sites.
