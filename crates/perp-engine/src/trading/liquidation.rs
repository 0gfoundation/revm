use alloy_primitives::IntoLogData;
use crate::host::PerpHost;
use primitives::{Address, FixedBytes, Log};

use super::match_order;
use crate::types::CancelReason;
use crate::{
        errors::{perp_err, perp_invariant_err},
    events::emit_position_changed,
    interface::IPerpDex,
    math::{
        calc_bankruptcy_price, calc_position_equity, calc_value, calc_value_i64,
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
/// What a liquidation's book leg left behind.
pub(crate) struct LiquidationCloseOutcome {
    /// Unfilled quantity — the residual that goes to ADL.
    pub(crate) remaining: u64,
    /// The walk stopped on the distinct-maker cap, NOT on the book or the band. The cap is per
    /// match, so retrying gets a fresh allowance and can absorb more from the SAME book: this is
    /// same-block recoverable and must set `Market::deferred_work`.
    pub(crate) maker_cap_deferred: bool,
}

pub(crate) fn execute_liquidation_market_order<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    side: Side,
    quantity: u64,
) -> Result<LiquidationCloseOutcome, PerpError> {
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
        // A liquidation close is a protocol action, not a user order: it never rests (Market/IOC),
        // so no admission predicate applies and the flag would carry no meaning.
        reduce_only: false,
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
            // A liquidation close is a protocol action, not a user order — no modifiers apply.
            flags: 0,
        }
        .to_log_data(),
    });

    let outcome = match_order(
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
    )?;
    // ── ⚠️ A LIQUIDATION MUST NEVER BECOME REJECTABLE ──────────────────────────────────────────
    // A liquidation that cannot complete is a LIVENESS failure, not a user error, so the write
    // barrier is hoisted by exactly ZERO statements here: `apply` is the next thing that runs.
    //
    // Structurally, not by convention: this close is `OrderKind::Market`, and `Market` has
    // `rests_remainder() == false`, so `match_order` never builds a `rest_basis`, no caller of this
    // function ever calls `rest_in_book`, and neither `RestOutcome` refusal is reachable from this
    // path at all. The set of rejects between the match and the flush is empty, which is the same
    // set it was before the hoist — the liquidation path's reject surface is unchanged.
    let remaining = outcome.remaining;
    // Read BEFORE `apply` consumes the outcome.
    let maker_cap_deferred = outcome.maker_cap_deferred();
    outcome.apply(context, side, market, &mut None)?;
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
            // A rounding-residue WRITE-OFF, not a close: the PnL of this liquidation's fills was
            // already realised (and accumulated into `cumulative_realized_pnl`) by
            // `settlement::apply_position_fill` on the taker path. So nothing is added to `cr`
            // here — and, just as importantly, nothing RESETS it: `pos` is the loaded position and
            // only these two fields are overwritten.
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

    Ok(LiquidationCloseOutcome {
        remaining,
        maker_cap_deferred,
    })
}

/// Auto-deleveraging (ADL, scheme X): close a liquidated position's book-unfillable
/// residual as a forced trade against opposite-side holders. **Every** residual comes
/// here, solvent or not — only the price differs (mark vs the bankruptcy price `P_b`),
/// which is what makes "a position is only ever closed against another position" an
/// unconditional invariant. No Insurance Fund, no mint: the loser closes at a price
/// where it realizes no bad debt, and each opposite holder that can absorb at that
/// price without going insolvent gives up exactly its share of the
/// shortfall. Both legs of every fill use the SAME single-floored `calc_value(P_b,
/// take)`, so Σ vQuote and Σ amount are conserved (a real trade, not a synthetic close).
///
/// Runs inside the liquidation sweep, bounded by the shared `budget` (total ADL fills
/// for the whole `updateIndexPrice`). Any residual left unclosed (budget exhausted, or
/// not enough deeply-in-profit opposite holders) stays open and is re-swept next update.
///
/// Opposite holders with resting orders ARE eligible; their open orders in this market are
/// cancelled as part of the fill, which is both what Binance does and what removes the
/// need for any flip-aware reservation recompute (there is nothing left to re-price).
///
/// v1 simplification: opposite holders that are themselves below water are skipped
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

    // ── The fill price: MARK for a solvent residual, BANKRUPTCY PRICE for an insolvent one ──────
    //
    // Both kinds of residual come here, and the only thing that differs is the price. Using `p_b`
    // for a solvent residual would CONFISCATE the equity it still has (`p_b` is below mark for a
    // long, above it for a short — by construction the price at which equity reaches zero), which
    // is strictly harsher than the synthetic mark-price close this replaced. Using `mark` for an
    // insolvent one would hand the winner a position already past bankruptcy and route the
    // difference to bad debt — which `adl_fill` refuses outright, so it would simply never fill.
    //
    // At mark the fill is economically NEUTRAL for the winner: its position is closed at exactly
    // the price it is already marked at, so its equity does not move. It loses the exposure, not
    // money. That is the whole reason routing solvent residuals here is acceptable at all.
    let residual_equity = calc_position_equity(
        mark_price,
        loser_pos.amount,
        loser_pos.v_quote_balance,
        loser_pos.margin,
        bd,
        pd,
    )?;
    let fill_price = if residual_equity >= 0 {
        // `mark_price > 0` is a market invariant (`addMarket` rejects 0 and every mark component
        // is floored at 1), so the solvent branch needs no zero guard.
        mark_price
    } else {
        let p_b = calc_bankruptcy_price(
            loser_pos.amount,
            loser_pos.v_quote_balance,
            loser_pos.margin,
            bd,
            pd,
        )?;
        if p_b == 0 {
            // Only reachable for a short whose bankruptcy price rounds below one price unit
            // (`calc_bankruptcy_price` floors for shorts). Filling at 0 would hand the winner the
            // position for free, so defer instead — the position stays open and registered.
            return Ok(());
        }
        p_b
    };
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
        // ⚠️ Holders with resting orders are NOT excluded. v1 skipped them — an ADL fill moves the
        // position, which re-prices every order resting against it, and v1 did not want to reason
        // about that. The exclusion turned out to be the binding constraint on ADL in practice:
        // the deeply-in-profit holders ADL ranks first are precisely the ones most likely to be
        // sitting on a take-profit, so in a real cascade the candidate list came back empty and
        // every residual deferred. The golden scenario demonstrated exactly this.
        //
        // Binance does not exclude them either — it selects them and CANCELS their open orders.
        // That is what the fill loop below does, and it sidesteps the re-pricing question entirely
        // rather than solving it: after the cancel there are no orders left to re-price.
        let eq_mark =
            calc_position_equity(mark_price, wp.amount, wp.v_quote_balance, wp.margin, bd, pd)?;
        if eq_mark <= 0 {
            continue; // itself liquidatable — leave to the sweep
        }
        let eq_fill =
            calc_position_equity(fill_price, wp.amount, wp.v_quote_balance, wp.margin, bd, pd)?;
        if eq_fill < 0 {
            continue; // cannot absorb at the fill price without going insolvent
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
            fill_price,
            bd,
            pd,
        )?
        else {
            continue; // no clean (bad-debt-free) fill possible — skip this winner
        };
        // ── Binance parity: an ADL'd account's open orders are cancelled ─────────────────────────
        //
        // AFTER the trial fill, never before: `adl_fill` works on clones and commits only if both
        // legs come out bad-debt-free, so a winner it rejects must not lose their book for nothing.
        //
        // The call reloads the winner's STORED (pre-fill) position to clear its side aggregates and
        // writes it; the `save_position` below then overwrites that with the post-fill position,
        // whose aggregates we clear here to match. Two writes to one key — the second is the one
        // that lands, and under #16d a repeated key is still a single commitment entry.
        //
        // Reason `Adl`, not `Liquidation`: this owner was profitable and was selected for that
        // reason. They are not being liquidated.
        crate::risk::cancel_all_orders_for_market(
            context,
            winner,
            market.market_id,
            market,
            CancelReason::Adl,
        )?;
        winner_pos.clear_side_aggregates();
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
                price: fill_price,
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
        // Both legs are valued at the market's CURRENT mark, not at the `fill_price` the fill
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

    // ── reduce-only: ADL shrank (or flipped) the loser's position ────────────────────────────
    //
    // ADL is the ONE position-shrinking path that does not cancel the owner's orders — liquidation
    // clears their whole book in the market, and a match-driven shrink is handled at the registry
    // flush. So without this the loser's reduce-only orders would be left over-committed against a
    // position that no longer covers them, with nothing to restore the prefix condition.
    //
    // The LOSER only: ADL candidates holding ANY resting order are excluded from the winner set
    // above, so a winner cannot be holding a reduce-only order to evict.
    crate::reduce_only::restore_both_sides(
        context,
        loser,
        market,
        crate::types::CancelReason::ReduceOnlyPositionShrank,
    )?;
    Ok(())
}

/// One ADL forced trade: close `take` of both the loser and one opposite holder at
/// `fill_price`, shrinking `take` (bounded) so NEITHER side realizes bad debt from sub-unit
/// flooring. Both legs use the SAME single-floored `calc_value(fill_price, take)`. Returns the
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
    fill_price: u64,
    bd: u32,
    pd: u32,
) -> Result<Option<AdlFillOutcome>, PerpError> {
    use super::settlement::{apply_position_fill, OpeningMarginFunding};
    let floor = take.saturating_sub(4); // try take, take-1, .., take-4 (dust is <=1-2)
    let mut t = take;
    while t > 0 && t > floor {
        let v = calc_value(fill_price, t, bd, pd)?;
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
