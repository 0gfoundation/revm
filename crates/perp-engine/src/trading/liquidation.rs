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
    types::{AccountUpdateReason, Market, Order, OrderKind, OrderStatus, Side, StpMode},
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
        // Not a conditional order: a liquidation close is a protocol action, never triggered.
        trigger_price: 0,
        trigger_up: false,
        close_position: false,
        // Self-trade prevention never applies to a liquidation close (the liquidated user's own
        // orders in this market are all cancelled before it runs, so it cannot meet one). The
        // field is the default only because the record has to hold something.
        stp: StpMode::default(),
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
            // A liquidation close is a protocol action, not a user order — no modifiers apply. The
            // byte still goes through the one encoder; its STP bits (EXPIRE_MAKER, `00`) are
            // MEANINGLESS for this order.
            flags: super::encode_order_flags(false, StpMode::default()),
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
        // `margin` is never residue any more, and is deliberately NOT written off:
        // `apply_position_fill` releases it SIGNED, so a full close leaves exactly 0 — including a
        // funding DEBT, which the close netted against its PnL. A non-zero margin here would mean
        // that release regressed, and zeroing it would silently forgive a debt (minting money nobody
        // paid) or destroy collateral. Left alone, it stays visible in `Σ pos.margin`; the assert
        // makes the regression loud in every test build.
        debug_assert_eq!(
            pos.margin, 0,
            "liquidation full fill left margin on a flat position — the signed release regressed"
        );
        if pos.v_quote_balance != 0 {
            // A rounding-residue WRITE-OFF, not a close: the PnL of this liquidation's fills was
            // already realised (and accumulated into `cumulative_realized_pnl`) by
            // `settlement::apply_position_fill` on the taker path. So nothing is added to `cr`
            // here — and, just as importantly, nothing RESETS it: `pos` is the loaded position and
            // only this one field is overwritten.
            pos.v_quote_balance = 0;
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
/// unconditional invariant. No mint: the loser closes at a price where it realizes no bad
/// debt, and each opposite holder that can absorb at that price without going insolvent
/// gives up exactly its share of the shortfall. The fund is billed only for what no
/// representable price can push onto the opposite side (`p_b == 0`) and for fallback-tier
/// winners already short at the fill price. Both legs of every fill use the SAME
/// single-floored `calc_value(P_b, take)`, so Σ vQuote and Σ amount are conserved (a real
/// trade, not a synthetic close).
///
/// Runs inside the liquidation sweep, bounded by the shared `budget` (total ADL fills
/// for the whole `updateIndexPrice`). Any residual left unclosed (budget exhausted, or
/// not enough deeply-in-profit opposite holders) stays open and is re-swept next update.
///
/// Opposite holders with resting orders ARE eligible; their open orders in this market are
/// cancelled as part of the fill, which is both what Binance does and what removes the
/// need for any flip-aware reservation recompute (there is nothing left to re-price).
///
/// Opposite holders that cannot absorb bad-debt-free (bankrupt at the mark, or short at the
/// fill price) are not skipped: they form the FALLBACK tier, used only once every clean
/// candidate is spent, under `BothSides` (see the tier notes in the body). Cascades from
/// ADL'ing a thin winner resolve on a later sweep, not by in-`run_adl` recursion.
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
    let mut loser_policy = BadDebtPolicy::Forbid;
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
            // ── NO REPRESENTABLE BANKRUPTCY PRICE (a short only) ────────────────────────────────
            //
            // A long never lands here: on this branch its credit `vQuote + margin` is below
            // `−notional(mark) < 0`, so its `p_b` is ≥ 1. A SHORT gets 0 in two regimes:
            //
            //   * SUB-UNIT — REACHABLE, verified (`perp_core::math`'s
            //     `bankruptcy_price_floors_to_zero_for_a_sub_unit_short`):
            //     `0 ≤ credit < calc_value(1, |amount|)` — opened at the smallest representable
            //     price, or whittled down until the remaining notional floors `vQuote` to 0. The
            //     loser's shortfall at one price unit is at most `calc_value(1, take)`: dust.
            //   * FUNDING DEBT — `credit < 0`: a funding debt (negative `margin`, see
            //     `crate::funding`) has eaten the entry proceeds, so the short is underwater at
            //     EVERY price (`calc_bankruptcy_price` reports 0, not an error). NOT dust: the
            //     loser's shortfall is `|credit|` pro rata plus that dust. Reachable only through a
            //     liquidation deferred (ADL, or the sweep's budget) across many funding epochs, or
            //     one update that lifts the mark by more than ~`1 / MAX_FUNDING_RATE` (≈133×): an
            //     epoch moves credit by at most 0.75% of the notional at the very mark the sweep
            //     then checks, and the last healthy check had `credit > notional` at its own mark.
            //
            // No representable price closes it cleanly: at 0 the winner (a long) would give its
            // position away for nothing, and every price above leaves the loser short. So fill at
            // ONE price unit — the lowest price that is not a giveaway — under `LoserOnly`: scheme X
            // pushed as far as a representable price allows. The opposite side absorbs the
            // shortfall down to price 1; the fund takes the part below it, plus any fallback-tier
            // winner's own shortfall.
            //
            // ⚠️ The cost, plainly: the loser's shortfall at the mark, `notional(mark) + |credit|`,
            // is fixed; the price only splits it between the winners and the fund. At price 1 a
            // winner forfeits its slice's whole market value — more than its equity for any long
            // above 1× on its EFFECTIVE margin, so such a winner is fallback tier: its deficit is
            // drawn from its remaining margin, then billed to the fund. (A long credited the
            // funding this short paid can sit at or under 1× and stay clean — `funding_debt`'s C6.)
            // The fund's bill is therefore the smallest any representable price gives, and the
            // saving over a fill at the mark (winners neutral, the fund pays everything) is exactly
            // what the winners give up: each slice's value between the mark and price 1, capped at
            // that winner's equity at the mark. Kept because it is continuous with `credit > 0`
            // (where a `p_b` near 1 already wipes the same winners — switching to the mark at
            // `credit ≤ 0` would be a cliff in who pays), and because in this regime the opposite
            // side is the one credited the funding this short never paid. `realizedPnl` follows
            // the price: the loser books ≈ its entry proceeds, the winner ≈ −its entry cost.
            //
            // ⚠️ A QUOTE shortfall only: the position is TRANSFERRED to the winner, so Σ amount is
            // conserved exactly as on every other ADL path.
            loser_policy = BadDebtPolicy::LoserOnly;
            1
        } else {
            p_b
        }
    };
    let loser_is_long = loser_pos.amount > 0;

    // Enumerate + rank opposite-side candidates ONCE (rank stable within this call).
    //
    // ── Candidates are judged on their EFFECTIVE margin: stored margin + pending funding ──────────
    // A counterparty's stored margin is current only as of its last touch; if a funding epoch has
    // passed since, it is missing the funding it owes or is owed. The loser had its own settled before this
    // call; a winner has not. Judging it on the stored number would rank it on stale equity and,
    // now that a funding debt can run margin negative, could admit a winner whose real equity at
    // the fill price is below zero. The pending amount is read through the same function the
    // settlement applies (`funding::pending_funding_payment`), and a winner that is actually FILLED
    // has it settled for real first (the fill loop below).
    let funding_index = storage::load_funding_state(context, market.market_id)?
        .cumulative_funding_index;
    let registry = storage::load_position_registry(context, market.market_id)?;
    let mut cands: Vec<(Address, i128, i128)> = Vec::new(); // (addr, uPnL@mark, equity@mark)
    // Counterparties already bankrupt at the mark, held back until `cands` is spent. `(addr, eq)`.
    let mut fallback: Vec<(Address, i128)> = Vec::new();
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
        let margin = wp
            .margin
            .checked_add(crate::funding::pending_funding_payment(funding_index, &wp, market)?)
            .ok_or_else(|| perp_err("adl: effective margin overflow"))?;
        let eq_mark =
            calc_position_equity(mark_price, wp.amount, wp.v_quote_balance, margin, bd, pd)?;
        if eq_mark <= 0 {
            // ── FALLBACK TIER ───────────────────────────────────────────────────────────────────
            //
            // Everything that cannot absorb bad-debt-free lands here, and is used only after every
            // clean candidate is exhausted — then at the price this ADL is already using, with
            // BOTH sides' shortfalls going to the insurance fund.
            //
            // This arm is the easy half: the holder is ALREADY bankrupt at the mark, so its
            // deficit is already the fund's — the sweep is going to liquidate it and route its bad
            // debt there anyway. Filling it here does not create exposure, it realises exposure
            // that exists, and realising it NOW caps it, where deferring lets the loser's position
            // keep marking against a market with no solvent counterparty left. The `eq_fill < 0`
            // arm below is the one with a real cost; see its note.
            //
            fallback.push((user, eq_mark as i128));
            continue;
        }
        let eq_fill =
            calc_position_equity(fill_price, wp.amount, wp.v_quote_balance, margin, bd, pd)?;
        if eq_fill < 0 {
            // ⚠️ DELIBERATE, AND KNOWN TO BE THE WEAK PART — a temporary simplification, chosen
            // with the trade-off understood, to be revisited.
            //
            // This holder is SOLVENT at the mark: nobody would liquidate it, and the ADL price is
            // a HAIRCUT (an insolvent long's `p_b` sits ABOVE the mark, and a short's equity falls
            // as price rises), so conscripting it manufactures fund exposure that would not
            // otherwise exist, at the expense of someone who is not bankrupt. That is a real cost
            // and it is not being denied here.
            //
            // It buys the property that ADL now essentially CANNOT fail: `Σ amount == 0` holds
            // inside a market (every `apply_position_fill` call site is two-sided), so opposite
            // capacity always covers the residual, and with nobody excluded the only remaining
            // limit is the fill budget — which is progress, not a stall. Bankrupt positions stop
            // accumulating in the registry, which is what was burning liquidation slots and
            // re-arming `deferred_work` every tick.
            //
            // ⚠️ Note for whoever revisits this: there is NO "charge the fund only for the part
            // it could not absorb cleanly" refinement available. `apply_position_fill` prorates
            // BOTH `margin_release` and `vq_fraction` by the closed fraction, so the shortfall is
            // LINEAR in the quantity closed — if the whole position is short at this price, every
            // slice of it is short in the same proportion. Shrinking `take` buys nothing beyond
            // the dust `adl_fill` already handles. The real knob is the eligibility rule itself.
            //
            // `BadDebtPolicy` is the seam if the exclusion comes back.
            fallback.push((user, eq_mark as i128));
            continue;
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
    // The fallback tier cannot use ROE — its denominator may be <= 0 — so: HEALTHIEST first
    // (equity at mark DESC), Address ASC tie-break. Not arbitrary: the shortfall a holder
    // contributes grows with how far under it already is, so this hands the fund the smallest
    // bill, and it naturally serves the merely-thin before the already-bankrupt.
    fallback.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

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
    // Shortfalls this ADL could not avoid: the loser's when `p_b == 0` (sub-unit or funding-debt
    // regime), and a fallback-tier winner's under `BothSides`. Zero on every ordinary fill.
    // Absorbed once after the loop rather than per fill, so the fund takes one write and one event
    // for the whole ADL.
    let mut bad_debt: u64 = 0;

    // Clean candidates first, then — only if the residual outlives them — the already-bankrupt
    // fallback tier. One loop, so the budget, the cancels and the event shape are shared; the only
    // difference is which shortfalls the fill is allowed to leave behind.
    let mut queue: Vec<(Address, bool)> = cands
        .into_iter()
        .map(|(w, _, _)| (w, false))
        .chain(fallback.into_iter().map(|(w, _)| (w, true)))
        .collect();
    let mut qi = 0;
    while qi < queue.len() {
        let (winner, is_fallback) = queue[qi];
        qi += 1;
        if *budget == 0 || remaining == 0 {
            break;
        }
        let policy = if is_fallback {
            BadDebtPolicy::BothSides
        } else {
            loser_policy
        };
        let mut winner_pos = storage::load_position(context, winner, market.market_id)?;
        let mut winner_account = storage::load_account(context, winner)?;
        // Settle the winner's pending funding IN MEMORY before the trial, so the fill is tried on
        // the margin it really has and the close nets any funding debt. Committed (events only —
        // funding writes nothing of its own) only if the trial is accepted; a rejected candidate
        // drops it with `winner_pos`, which is reloaded if it is tried again.
        //
        // Without this the ADL'd slice's accrued funding was never settled at all: the later settle
        // charges only the REMAINING amount, and a full close re-anchors a flat position with no
        // payment — so whatever the closed quantity owed was forgiven and whatever it was owed was
        // lost.
        let winner_funding = crate::funding::compute_funding_settlement(
            context,
            winner,
            market,
            &mut winner_pos,
            Some(&winner_account),
        )?;
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
            policy,
            bd,
            pd,
        )?
        else {
            // A clean-tier candidate the bad-debt-free trial rejected. Do NOT drop it: re-queue it
            // behind the fallback tier, where `BothSides` accepts.
            //
            // Without this, one dust-level rounding on the LOSER's leg — the same leg for every
            // clean-tier candidate — rejects ALL of them at once, and if no holder happened to
            // land in the fallback tier the residual defers with capacity sitting right there.
            //
            // The WINNER's leg can reject too, on rounding alone: `apply_position_fill` releases a
            // negative margin with trunc0, which puts up to one extra unit of the debt on the
            // closing slice, and a remainder whose margin is ≤ 0 has nothing to draw it from. So a
            // clean-tier winner carrying a funding debt whose equity at the fill price is only a
            // few units can come out 1 unit short, be rejected, and be re-queued BEHIND the
            // fallback tier — where it may never be reached, the residual going to a bankrupt
            // holder under `BothSides` instead (`funding_debt`'s C7 rejected-trial tests). Narrow:
            // near-zero winner equity AND a negative margin. trunc0 is kept because it is what
            // keeps a solvent SHORT's remainder at credit ≥ 0 (a floor could leave it at −1, i.e.
            // in the `p_b == 0` regime — `settlement`'s `isolated_margin_tests`).
            //
            // Terminates: only a non-fallback entry is ever pushed, so each address is re-queued at
            // most once, and a `BothSides` trial cannot reject (its `accepts` is unconditional and
            // `take >= 1` under the loop guards).
            if !is_fallback {
                queue.push((winner, true));
            }
            continue;
        };
        // Accepted: publish the winner's funding group now, ahead of the cancels and the fill's own
        // events, so the stream reads funding → ADL like every other settled path. Its margin move
        // is already in `winner_pos` and lands with the `save_position` below.
        if let Some(p) = winner_funding {
            crate::funding::apply_funding_settlement(context, p)?;
        }
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
        bad_debt = bad_debt
            .saturating_add(fill.loser_bad_debt)
            .saturating_add(fill.winner_bad_debt);
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

    // The `p_b == 0` loser shortfall and any fallback-tier winner shortfall, absorbed ONCE for the
    // whole ADL rather than per fill, so the fund takes one write and emits one row. Zero on every
    // fill accepted under `Forbid` (`adl_fill` reports bad debt only where `BadDebtPolicy` allows
    // it), and this is a QUOTE claim only: the position itself went to the winner, so Σ amount is
    // untouched. It is routed AFTER the loser's `clear_account_snapshot_mark` above on purpose — it
    // moves no wallet, so it cannot re-mark the loser, and the row reads as the fund absorbing, not
    // as a balance change.
    super::settlement::absorb_bad_debt_into_insurance_fund(context, market.market_id, bad_debt)?;

    // ── reduce-only: ADL shrank (or flipped) the loser's position ────────────────────────────
    //
    // ADL is the ONE position-shrinking path that does not cancel the owner's orders — liquidation
    // clears their whole book in the market, and a match-driven shrink is handled at the registry
    // flush. So without this the loser's reduce-only orders would be left over-committed against a
    // position that no longer covers them, with nothing to restore the prefix condition.
    //
    // The LOSER only: a winner's whole book in this market is cancelled as part of its fill, so by
    // the time the position moved it had no reduce-only order left to evict. (This used to hold for
    // a different reason — order-holding candidates were excluded from the winner set outright.)
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
    /// Non-zero only where [`BadDebtPolicy`] allows it. The caller routes both to the fund.
    loser_bad_debt: u64,
    winner_bad_debt: u64,
}

/// Which side, if either, an ADL fill may leave short. The fill is always TRIED bad-debt-free
/// first (`adl_fill` shrinks `take` before giving up), so a permission is a fallback, never a
/// preference.
#[derive(Clone, Copy, PartialEq)]
enum BadDebtPolicy {
    /// Every ordinary ADL fill. Neither side may end up short — "scheme X: no IF".
    Forbid,
    /// `p_b == 0` — no representable price closes the LOSER cleanly (the sub-unit regime, or a
    /// short whose funding debt exceeds its entry proceeds), so its gap goes to the fund: dust
    /// below `calc_value(1, take)` in the first case, `|credit|` pro rata plus that dust in the
    /// second. The WINNER is still held to zero bad debt.
    LoserOnly,
    /// Last resort, and only against a counterparty that is ALREADY bankrupt at the mark. See
    /// `run_adl`'s fallback tier for why that restriction is the whole argument.
    BothSides,
}

impl BadDebtPolicy {
    fn accepts(self, loser: u64, winner: u64) -> bool {
        match self {
            Self::Forbid => loser == 0 && winner == 0,
            Self::LoserOnly => winner == 0,
            Self::BothSides => true,
        }
    }
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
    policy: BadDebtPolicy,
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
        if policy.accepts(loser_outcome.bad_debt, winner_outcome.bad_debt) {
            *loser_pos = lp;
            *loser_wallet = lw;
            *winner_pos = wp;
            *winner_wallet = ww;
            return Ok(Some(AdlFillOutcome {
                quantity: t,
                loser_realized_pnl: loser_outcome.realized_pnl,
                winner_realized_pnl: winner_outcome.realized_pnl,
                loser_bad_debt: loser_outcome.bad_debt,
                winner_bad_debt: winner_outcome.bad_debt,
            }));
        }
        t -= 1;
    }
    Ok(None)
}

// `emit_position_changed` lives in `crate::events` — one derivation for all seven emit sites.
