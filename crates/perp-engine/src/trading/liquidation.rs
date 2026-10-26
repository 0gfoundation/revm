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

///
/// `limit`: `None` for an ISOLATED liquidation — a MARKET order, bounded by the price band alone.
/// `Some(p)` for a CROSS episode's close (cross P3, owner decision Q4): a LIMIT IOC at `p`, the
/// account's MM-share bankruptcy price clamped into the band — Binance's "one large Immediate or
/// Cancel order" whose "limit price" is the bankruptcy price. Both never rest.
pub(crate) fn execute_liquidation_market_order<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    side: Side,
    quantity: u64,
    limit: Option<u64>,
) -> Result<LiquidationCloseOutcome, PerpError> {
    let (order_id, bumped_nonce) = super::peek_order_id(context, user)?;
    // A liquidation close is immediate-or-cancel, bounded by the price band, never resting: a
    // market order, or (cross) a limit IOC. One `OrderKind` says which, and the record/event's two
    // wire fields are derived from it.
    let (kind, price) = match limit {
        None => (OrderKind::Market, 0),
        Some(p) => (OrderKind::Limit(crate::types::TimeInForce::Ioc), p),
    };
    let mut order = Order {
        owner: user.0 .0,
        market_id: market.market_id,
        side,
        price,
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
        // Not a conditional order, so bound to no position.
        position_generation: 0,
        trail_bps: 0,
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
            price,
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
        price,
        quantity,
        kind, // market order or limit IOC: never rests, never all-or-nothing
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
    // Structurally, not by convention: this close is `OrderKind::Market` or a limit IOC, and neither
    // has `rests_remainder() == true`, so `match_order` never builds a `rest_basis`, no caller of this
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
///
/// # CROSSED winners (cross P3, R5)
///
/// A winner CROSSED in force has no silo, so its position equity is not a number ADL can rank or
/// screen it by. It is judged on its ACCOUNT instead — `E` and this market's leg from the TOTAL
/// cross health check (`margin_view::cross_health`, priced off the account's stored cross legs —
/// cross P4; no `Err` but the storage layer's; J4):
///
/// * **Rank** — Binance's `PnL% × EffLev` when `PnL% ≥ 0` and `PnL% / EffLev` when `< 0`, with
///   `PnL% = uPnL_k / |N_k|` (symbol level) and `EffLev = |N_k| / E` (account level — cross report
///   §1.7, F25), which reduce to `uPnL_k / E` and `uPnL_k · E / N_k²`. An ISOLATED winner gets the
///   SAME sign split with its position equity (silo + pending funding + uPnL) in `E`'s place —
///   F25's split is confirmed for every position; only the isolated denominator is unpublished
///   (F26), and position equity is the one it already used for a gain. (Until the P3 review an
///   isolated loss ranked `uPnL / equity`, which orders losses the other way round and could not
///   be compared with a cross loss on one scale.) Both kinds are ordered together EXACTLY: every
///   rank is a ratio, compared by cross-multiplication in 512 bits ([`AdlRank`]), so the order is
///   total.
/// * **Tier** — rankable needs `E > 0`; an account at or below zero is fallback tier outright. A
///   rankable one is then clean PER FILL: `E − take · |fill − mark| ≥ 0` for the `take` this fill
///   would close (the worst its equity can do at the fill price instead of the mark). A fill that
///   fails it re-queues the winner behind the fallback tier, like a rejected trial.
///
/// Its fill settles into its cross wallet, either sign (`settlement::apply_position_fill` under
/// `Cross`). In the clean tier the per-fill test keeps that within its equity, so the fill policy
/// binds only the loser's leg. In the FALLBACK tier — `E ≤ 0`, a leg the fold could not price, or
/// a fill it could not carry — it is held to `E_after ≥ min(E, 0)`, and what the fill price would
/// take past that is its bad debt, absorbed by the fund like an isolated fallback winner's silo
/// shortfall (`adl_fill`): ADL never drives a cross account deeper than its own equity (P3
/// review). Like every winner, it loses only its orders in THIS market (deviation D7).
///
/// # A CROSSED loser
///
/// Comes from a cross liquidation episode (`risk::cross_liquidation`) with its fill price decided
/// on the ACCOUNT (`cross_fill_price`): the mark while `E ≥ 0`, the MM-share bankruptcy price
/// clamped into the market's band otherwise (the deficit the band's edge does not transfer stays
/// in the loser's wallet). Its leg settles into its cross wallet like any cross fill; the episode
/// charges the clearance fee on what this closed.
pub(crate) fn run_adl<H: PerpHost>(
    context: &mut H,
    loser: Address,
    market: &Market,
    mark_price: u64,
    budget: &mut u32,
    // The fill price for a CROSSED loser, decided by its liquidation episode
    // (`risk::cross_liquidation`): the mark while the ACCOUNT's `E ≥ 0`, its MM-share bankruptcy
    // price clamped into the band otherwise (R5) — a cross position has no silo, so its own
    // position equity cannot pick the price the way an isolated one does below. `None` for an
    // ISOLATED loser.
    cross_fill_price: Option<u64>,
) -> Result<(), PerpError> {
    let mut loser_pos = storage::load_position(context, loser, market.market_id)?;
    if loser_pos.amount == 0 || *budget == 0 {
        return Ok(());
    }
    let bd = market.base_decimals;
    let pd = market.price_decimals;
    let loser_cross =
        crate::margin_type::effective_margin_type(&loser_pos) == crate::types::MarginType::Crossed;
    let (fill_price, loser_policy) = if loser_cross {
        // ── A CROSSED loser: the episode's price ─────────────────────────────────────────────────
        //
        // Its leg settles into the cross wallet either sign and reports NO bad debt
        // (`settlement::apply_position_fill` under `Cross`), so the policy binds only the WINNER's
        // leg — `Forbid` and `LoserOnly` are the same thing here. A bankruptcy price of 0 (a short
        // with no representable price) fills at ONE unit, the isolated rule's lowest non-giveaway
        // price. Without a price (no caller does that) nothing is done — totally, this runs inside
        // the sweep.
        let Some(p) = cross_fill_price else {
            debug_assert!(false, "run_adl: a CROSSED loser needs its episode's fill price");
            return Ok(());
        };
        (p.max(1), BadDebtPolicy::Forbid)
    } else {
        isolated_adl_price(&loser_pos, mark_price, bd, pd)?
    };
    run_adl_fills(
        context,
        loser,
        &mut loser_pos,
        market,
        mark_price,
        budget,
        fill_price,
        loser_policy,
    )
}

/// The fill price and loser policy of an ISOLATED loser's ADL: the mark for a solvent residual,
/// its bankruptcy price for an insolvent one (see the body).
fn isolated_adl_price(
    loser_pos: &crate::types::PerpPosition,
    mark_price: u64,
    bd: u32,
    pd: u32,
) -> Result<(u64, BadDebtPolicy), PerpError> {
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
    Ok((fill_price, loser_policy))
}

/// The ADL fill loop proper, at a price and loser policy already decided ([`run_adl`]).
#[allow(clippy::too_many_arguments)]
fn run_adl_fills<H: PerpHost>(
    context: &mut H,
    loser: Address,
    loser_pos: &mut crate::types::PerpPosition,
    market: &Market,
    mark_price: u64,
    budget: &mut u32,
    fill_price: u64,
    loser_policy: BadDebtPolicy,
) -> Result<(), PerpError> {
    let bd = market.base_decimals;
    let pd = market.price_decimals;
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
    // Clean candidates: `(addr, rank, Some(E) for a CROSSED one — its per-fill tier test)`.
    let mut cands: Vec<(Address, AdlRank, Option<i128>)> = Vec::new();
    // Counterparties already bankrupt at the mark, held back until `cands` is spent.
    // `(addr, eq, Some(E) for a CROSSED one — the floor its fallback fill is held to)`.
    let mut fallback: Vec<(Address, i128, Option<i128>)> = Vec::new();
    // One scratch buffer for every CROSSED candidate's health legs. The call's health memo
    // (`cross_health_for_adl`) from its second ranking on — the first one has nothing to reuse and
    // pays nothing — with room for every holder.
    let mut legs = Vec::new();
    let memoize = storage::note_adl_ranking(context) > 1;
    if memoize {
        storage::reserve_cross_health_memo(context, registry.len());
    }
    for user in registry {
        if user == loser {
            continue;
        }
        let wp = storage::load_position(context, user, market.market_id)?;
        if wp.amount == 0 || (wp.amount > 0) == loser_is_long {
            continue; // flat or same side as the loser
        }
        // ── A CROSSED winner: judged and ranked on its ACCOUNT (R5 — see the doc) ───────────────
        //
        // TOTAL (J4): `E` and this market's leg come from the total health check (the account
        // blob's cross legs and their markets — no position walk, cross P4), nothing here divides
        // (the rank is a ratio compared by cross-multiplication), and a leg the check could not
        // evaluate leaves the winner unrankable — fallback tier, at its `E`. `E = cWB + Σ_C uPnL`:
        // pending funding of every cross leg included (`cWB`), this market priced at its stored
        // mark, which is `mark_price` on both callers.
        //
        // Through the call's memo (`cross_health_for_adl`): every loser of a sweep ranks the same
        // holders again, and between two losers only the accounts a fill or a liquidation wrote
        // change — the memo re-prices exactly those (and every account, once any `Market` moved).
        if crate::margin_type::effective_margin_type(&wp) == crate::types::MarginType::Crossed {
            let (health, leg) = crate::margin_view::cross_health_for_adl(
                context,
                user,
                market.market_id,
                memoize,
                &mut legs,
            )?;
            let e = health.e;
            match leg {
                Some(leg) if e > 0 => {
                    // `N_k`: the leg's `uPnL` less the entry basis (`uPnL = N + vQuote`).
                    let notional = leg.upnl - wp.v_quote_balance as i128;
                    cands.push((user, AdlRank::sign_split(leg.upnl, e, notional), Some(e)));
                }
                _ => fallback.push((user, e, Some(e))),
            }
            continue;
        }
        // ── TOTAL (J4): an ISOLATED holder that cannot be valued is fallback tier, served last ──
        //
        // Every number below is arithmetic on this holder's own position, and any of it can fail
        // to fit an `i64` — the notional at the FILL price above all: a CROSSED loser's price is
        // clamped into the band, but a wide band still leaves room for a huge holder's value there
        // to overflow. This runs inside the liquidation sweep, which panics on an `Err`, so a value
        // that does not fit decides the TIER instead: fallback (at `i128::MIN` — after every
        // holder that could be valued — when even its equity at the mark is unknown), where
        // `adl_fill` skips it outright if the fill itself cannot be valued either.
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
        let Some(margin) = crate::funding::pending_funding_payment(funding_index, &wp, market)
            .ok()
            .and_then(|pf| wp.margin.checked_add(pf))
        else {
            fallback.push((user, i128::MIN, None));
            continue;
        };
        let Ok(eq_mark) =
            calc_position_equity(mark_price, wp.amount, wp.v_quote_balance, margin, bd, pd)
        else {
            fallback.push((user, i128::MIN, None));
            continue;
        };
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
            fallback.push((user, eq_mark as i128, None));
            continue;
        }
        // Unrepresentable at the fill price counts as short there (J4, above).
        let eq_fill =
            calc_position_equity(fill_price, wp.amount, wp.v_quote_balance, margin, bd, pd)
                .unwrap_or(-1);
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
            fallback.push((user, eq_mark as i128, None));
            continue;
        }
        let Ok(notional) = calc_value_i64(mark_price, wp.amount, bd, pd) else {
            fallback.push((user, eq_mark as i128, None));
            continue;
        };
        let notional = notional as i128;
        let upnl = notional + wp.v_quote_balance as i128;
        cands.push((user, AdlRank::sign_split(upnl, eq_mark as i128, notional), None));
    }
    // Rank DESC, Address ASC tie-break. Every candidate's rank is Binance's sign split (R5): over
    // its ACCOUNT equity for a CROSSED one, its position equity for an ISOLATED one (> 0 on this
    // tier); both are exact ratios, so this is a TOTAL order — cross-multiplied in 512 bits, never
    // saturated (a saturated product could make the comparator inconsistent, and `sort_by` may
    // panic on one).
    cands.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    // The fallback tier cannot use ROE — its denominator may be <= 0 — so: HEALTHIEST first
    // (equity at mark DESC — a CROSSED holder's account `E`), Address ASC tie-break. Not
    // arbitrary: the shortfall a holder
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
    //
    // A CROSSED candidate carries its `E` — in the clean tier for the per-fill tier test (R5), in
    // the fallback tier for the floor its fill is held to (see `adl_fill`); `None` otherwise.
    let mut queue: Vec<(Address, bool, Option<i128>)> = cands
        .into_iter()
        .map(|(w, _, e)| (w, false, e))
        .chain(fallback.into_iter().map(|(w, _, e)| (w, true, e)))
        .collect();
    let mut qi = 0;
    while qi < queue.len() {
        let (winner, is_fallback, cross_e) = queue[qi];
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
        // ── R5: a CROSSED winner is clean only for a fill its account can carry ──────────────────
        //
        // `E − take · |fill − mark| ≥ 0`: closing `take` at the fill price instead of the mark
        // moves its equity by at most that much. A fill it cannot carry would push the account's
        // whole wallet under zero to absorb this loser's price, so it goes behind the fallback
        // tier instead, exactly like a rejected trial — where the fund carries what its equity
        // cannot (`adl_fill`). TOTAL: an unrepresentable slippage value is "cannot carry".
        if let (false, Some(e)) = (is_fallback, cross_e) {
            let take = winner_pos.amount.unsigned_abs().min(remaining);
            let slippage = calc_value(fill_price.abs_diff(mark_price), take, bd, pd)
                .map_or(i128::MAX, |v| v as i128);
            if e.saturating_sub(slippage) < 0 {
                queue.push((winner, true, cross_e));
                continue;
            }
        }
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
        // `&mut`: a CROSSED winner's funding lands on this copy's wallet, which is saved with the
        // fill below — or dropped with it, on a rejected trial.
        let winner_funding = crate::funding::compute_funding_settlement(
            context,
            winner,
            market,
            &mut winner_pos,
            Some(&mut winner_account),
        )?;
        let take = winner_pos.amount.unsigned_abs().min(remaining);
        let Some(fill) = adl_fill(
            loser_pos,
            &mut loser_account.perp_wallet_balance,
            loser_close_is_buy,
            &mut winner_pos,
            &mut winner_account.perp_wallet_balance,
            winner_close_is_buy,
            take,
            fill_price,
            mark_price,
            policy,
            // The fallback tier's floor for a CROSSED winner; the clean tier needs none (R5 above
            // already held the fill within its equity).
            if is_fallback { cross_e } else { None },
            bd,
            pd,
        ) else {
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
            // most once. A `BothSides` trial rejects only a fill it cannot value at all (its
            // `accepts` is unconditional and `take >= 1` under the loop guards; J4 — see
            // `adl_fill`), and that holder is simply passed over.
            if !is_fallback {
                queue.push((winner, true, cross_e));
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
            loser_pos,
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
            loser_pos,
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

/// An ADL winner's rank — a ratio `num / den` with `den > 0`, compared EXACTLY (cross report §1.7,
/// R5; see `run_adl`'s doc for which ratio each kind of winner gets).
///
/// Held as a sign and two magnitudes so that the comparison is a cross-multiplication of
/// magnitudes. Every input is an `i128`, so a numerator is below 2^254 (`|uPnL| · E`) and a
/// denominator at most 2^254 (`N²`): both fit a `U256`, every product is below 2^508 — nothing
/// saturates, and the order is total for ANY inputs. (The previous comparator multiplied in
/// `i128`, saturating, which could tie unequal ranks — or compare inconsistently, which `sort_by`
/// may panic on — at the joint extremes. Wherever it did not saturate, it is this order.)
///
/// The products are taken in 256 bits when all four magnitudes fit a `u128` — every rank of a
/// gain, and every loss rank whose `|uPnL| · E` does — and in `U512` otherwise: the same exact
/// products, so the same order. A `U512` product cost ~50 ns, and ADL sorts every candidate
/// set once per loser (`M log M` comparisons — half of an ADL's cost at M = 1000 before this).
#[derive(Clone, Copy, Debug)]
struct AdlRank {
    /// `−1`, `0` or `1`: the sign of the ratio.
    sign: i8,
    /// `|num|`.
    num: alloy_primitives::U256,
    /// `den > 0`.
    den: alloy_primitives::U256,
}

impl AdlRank {
    /// `num / den` for `den > 0` (a `den ≤ 0` is clamped to 1 — no caller passes one).
    fn ratio(num: i128, den: i128) -> Self {
        Self {
            sign: num.signum() as i8,
            num: alloy_primitives::U256::from(num.unsigned_abs()),
            den: alloy_primitives::U256::from(den.max(1) as u128),
        }
    }

    /// Binance's sign-split rank with equity `e > 0` — the ACCOUNT's `E` for a CROSSED winner, the
    /// position's equity for an ISOLATED one: `PnL% × EffLev = uPnL / e` for `uPnL ≥ 0`,
    /// `PnL% / EffLev = uPnL · e / N²` for `uPnL < 0` (`N` the leg's notional; an `N` that floored
    /// to 0 counts as 1).
    fn sign_split(upnl: i128, e: i128, notional: i128) -> Self {
        if upnl >= 0 {
            return Self::ratio(upnl, e);
        }
        let n = alloy_primitives::U256::from(notional.unsigned_abs().max(1));
        Self {
            sign: -1,
            // Both products of two values below 2^128: below 2^256, no overflow.
            num: alloy_primitives::U256::from(upnl.unsigned_abs())
                * alloy_primitives::U256::from(e.max(1) as u128),
            den: n * n,
        }
    }

    /// `|self|` against `|other|`: `self.num · other.den` against `other.num · self.den`, exact.
    fn cmp_magnitude(&self, other: &Self) -> core::cmp::Ordering {
        match (
            u128_of(&self.num),
            u128_of(&other.den),
            u128_of(&other.num),
            u128_of(&self.den),
        ) {
            (Some(a), Some(d), Some(b), Some(c)) => mul_wide(a, d).cmp(&mul_wide(b, c)),
            _ => {
                use alloy_primitives::U512;
                let wide = |v: &alloy_primitives::U256| U512::from(*v);
                (wide(&self.num) * wide(&other.den)).cmp(&(wide(&other.num) * wide(&self.den)))
            }
        }
    }
}

/// `v` as a `u128` when it fits.
#[inline]
fn u128_of(v: &alloy_primitives::U256) -> Option<u128> {
    match v.as_limbs() {
        [lo, hi, 0, 0] => Some(((*hi as u128) << 64) | *lo as u128),
        _ => None,
    }
}

/// The exact 256-bit product `a · b` as `(high, low)` 128-bit halves — ordered as the product is.
#[inline]
fn mul_wide(a: u128, b: u128) -> (u128, u128) {
    const LO: u128 = u64::MAX as u128;
    let (a1, a0, b1, b0) = (a >> 64, a & LO, b >> 64, b & LO);
    let (p00, p01, p10, p11) = (a0 * b0, a0 * b1, a1 * b0, a1 * b1);
    // The middle column: three terms below 2^64 each, so no overflow in 128 bits.
    let mid = (p00 >> 64) + (p01 & LO) + (p10 & LO);
    let low = (p00 & LO) | (mid << 64);
    let high = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    (high, low)
}

impl Ord for AdlRank {
    /// Ascending by value.
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        match self.sign.cmp(&other.sign) {
            core::cmp::Ordering::Equal => {}
            unequal => return unequal,
        }
        // Same sign: compare |a| = a.num/a.den with |b| by cross-multiplication.
        let magnitude = self.cmp_magnitude(other);
        if self.sign < 0 {
            magnitude.reverse()
        } else {
            magnitude
        }
    }
}

impl PartialOrd for AdlRank {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Equality is equality of VALUE (`1/2 == 2/4`), consistent with [`Ord`].
impl PartialEq for AdlRank {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == core::cmp::Ordering::Equal
    }
}

impl Eq for AdlRank {}

/// One ADL forced trade: close `take` of both the loser and one opposite holder at
/// `fill_price`, shrinking `take` (bounded) so NEITHER side realizes bad debt from sub-unit
/// flooring. Both legs use the SAME single-floored `calc_value(fill_price, take)`. Returns the
/// the committed quantity and each participant's realized PnL, or `None` if no
/// clean fill is possible near `take` — or no fill can be VALUED (a value past the integer
/// widths: never an `Err`, this runs inside the liquidation sweep).
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
    mark_price: u64,
    policy: BadDebtPolicy,
    // A CROSSED winner in the FALLBACK tier: its account's `E` before this fill (see below).
    winner_cross_e: Option<i128>,
    bd: u32,
    pd: u32,
) -> Option<AdlFillOutcome> {
    use super::settlement::{apply_position_fill, OpeningMarginFunding};
    let floor = take.saturating_sub(4); // try take, take-1, .., take-4 (dust is <=1-2)
    let mut t = take;
    while t > 0 && t > floor {
        // TOTAL (J4): this runs inside the liquidation sweep, which panics on an `Err`. A trial
        // whose arithmetic does not fit — the fill's value, or either leg's settlement — is a
        // trial that cannot fill (`None`: the caller moves on to the next candidate); nothing has
        // been written, both legs are clones.
        let v = calc_value(fill_price, t, bd, pd).ok()?;
        // Trial on clones; commit only if BOTH sides are bad-debt free.
        // Both legs are PURE CLOSES (`opening_qty == 0`), so the ISOLATED funding rule is inert —
        // ADL never opens, and therefore never short-funds, a silo. The mode still matters for a
        // CROSSED leg (`for_position` → `Cross`): its realized PnL settles into the cross wallet,
        // either sign, and it reports NO bad debt — so on a cross winner the policy below binds
        // only the loser's leg, and the winner's account absorbs the price.
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
            OpeningMarginFunding::for_position(loser_pos, OpeningMarginFunding::Requirement),
        )
        .ok()?;
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
            OpeningMarginFunding::for_position(winner_pos, OpeningMarginFunding::Requirement),
        )
        .ok()?;
        // ── A CROSSED winner in the FALLBACK tier is backed by the fund, like an isolated one ──
        //
        // An isolated fallback winner loses at most its silo: the shortfall its close realizes
        // past it is `bad_debt` (`apply_position_fill`), which `BothSides` routes to the fund. A
        // cross fill has no silo and reports none — the whole off-mark loss would land on the
        // account's wallet, beyond its equity, and a solvent winner conscripted that way would
        // carry this loser's deficit into its OWN next liquidation (P3 review). So it is held to
        // the same bound: its `E` may fall to `min(E, 0)` and no further — a solvent account down
        // to zero, one already under water not one unit deeper (its deficit is the fund's already,
        // the isolated tier's "already bankrupt at the mark" argument) — and the rest is credited
        // back to its wallet and reported as ITS bad debt, for the fund to absorb with the
        // loser's. `E` moves by exactly the wallet's move plus this leg's `uPnL` at the mark
        // (`uPnL = N + vQuote`, the fold's own leg formula); its other legs do not move.
        let mut winner_bad_debt = winner_outcome.bad_debt;
        if let Some(e) = winner_cross_e {
            let upnl = |p: &crate::types::PerpPosition| -> Option<i128> {
                Some(calc_value_i64(mark_price, p.amount, bd, pd).ok()? as i128
                    + p.v_quote_balance as i128)
            };
            let e_after =
                e + (ww as i128 - *winner_wallet as i128) + upnl(&wp)? - upnl(winner_pos)?;
            let cover = (e.min(0) - e_after).max(0);
            if cover > 0 {
                let cover = i64::try_from(cover).ok()?;
                ww = ww.checked_add(cover)?;
                winner_bad_debt = winner_bad_debt.checked_add(cover as u64)?;
            }
        }
        if policy.accepts(loser_outcome.bad_debt, winner_bad_debt) {
            *loser_pos = lp;
            *loser_wallet = lw;
            *winner_pos = wp;
            *winner_wallet = ww;
            return Some(AdlFillOutcome {
                quantity: t,
                loser_realized_pnl: loser_outcome.realized_pnl,
                winner_realized_pnl: winner_outcome.realized_pnl,
                loser_bad_debt: loser_outcome.bad_debt,
                winner_bad_debt,
            });
        }
        t -= 1;
    }
    None
}

// `emit_position_changed` lives in `crate::events` — one derivation for all seven emit sites.

#[cfg(test)]
mod adl_rank_tests {
    use super::AdlRank;
    use core::cmp::Ordering;

    /// Value order, not representation: `1/2 == 2/4`, signs order before magnitudes, and a loss
    /// ranks below a zero below a gain.
    #[test]
    fn an_adl_rank_compares_ratios_by_value() {
        assert_eq!(AdlRank::ratio(1, 2), AdlRank::ratio(2, 4));
        assert!(AdlRank::ratio(1, 3) < AdlRank::ratio(1, 2));
        assert!(AdlRank::ratio(-1, 2) < AdlRank::ratio(-1, 3));
        assert!(AdlRank::ratio(-1, 1_000_000) < AdlRank::ratio(0, 7));
        assert_eq!(AdlRank::ratio(0, 7), AdlRank::ratio(0, 1));
        assert!(AdlRank::ratio(0, 1) < AdlRank::ratio(1, i128::MAX));
        // The sign split: a gain is `uPnL / E`; a loss `uPnL · E / N²` — more equity ranks LOWER
        // there.
        assert_eq!(AdlRank::sign_split(10, 40, 600), AdlRank::ratio(1, 4));
        assert!(AdlRank::sign_split(-20, 9_980, 120) < AdlRank::sign_split(-20, 80, 120));
        assert!(AdlRank::ratio(-20, 9_980) > AdlRank::ratio(-20, 80), "the rule R5 replaces");
        // A notional that floored to 0 counts as 1.
        assert_eq!(AdlRank::sign_split(-1, 1, 0), AdlRank::ratio(-1, 1));
    }

    /// TOTAL at the joint extremes, where the previous saturating `i128` products would tie or
    /// invert: the comparison is exact, antisymmetric and transitive over a mixed set.
    #[test]
    fn an_adl_rank_is_a_total_order_at_the_extremes() {
        let big = i128::MAX;
        let ranks = [
            AdlRank::ratio(big, 1),
            AdlRank::ratio(big, big),
            AdlRank::ratio(big - 1, big),
            AdlRank::ratio(1, big),
            AdlRank::ratio(0, big),
            AdlRank::ratio(-1, big),
            AdlRank::ratio(-big, 1),
            AdlRank::ratio(i128::MIN, 1),
            AdlRank::sign_split(-(1 << 66), 1 << 71, i64::MAX as i128),
            AdlRank::sign_split(-(1 << 66), (1 << 71) - 1, i64::MAX as i128),
            AdlRank::sign_split(i128::MIN, big, 1),
            AdlRank::sign_split(big, big, big),
        ];
        assert!(AdlRank::ratio(big - 1, big) < AdlRank::ratio(big, big));
        assert!(
            AdlRank::sign_split(-(1 << 66), 1 << 71, i64::MAX as i128)
                < AdlRank::sign_split(-(1 << 66), (1 << 71) - 1, i64::MAX as i128)
        );
        for a in &ranks {
            assert_eq!(a.cmp(a), Ordering::Equal);
            for b in &ranks {
                assert_eq!(a.cmp(b), b.cmp(a).reverse(), "antisymmetric");
                for c in &ranks {
                    if a <= b && b <= c {
                        assert!(a <= c, "transitive");
                    }
                }
            }
        }
        let mut sorted = ranks.to_vec();
        sorted.sort();
        assert!(sorted.windows(2).all(|w| w[0] <= w[1]));
    }

    /// The comparison the products were always taken in: `U512` cross-multiplication.
    fn reference(a: &AdlRank, b: &AdlRank) -> Ordering {
        use alloy_primitives::U512;
        let w = |v: alloy_primitives::U256| U512::from(v);
        a.sign.cmp(&b.sign).then_with(|| {
            let m = (w(a.num) * w(b.den)).cmp(&(w(b.num) * w(a.den)));
            if a.sign < 0 {
                m.reverse()
            } else {
                m
            }
        })
    }

    /// `mul_wide` is the exact 256-bit product, at the edges of `u128`.
    #[test]
    fn the_wide_product_is_exact() {
        use alloy_primitives::U256;
        let edges =
            [0u128, 1, 2, u64::MAX as u128, 1 << 64, (1 << 64) + 1, u128::MAX - 1, u128::MAX];
        for &a in &edges {
            for &b in &edges {
                let (hi, lo) = super::mul_wide(a, b);
                let want = U256::from(a) * U256::from(b);
                assert_eq!((U256::from(hi) << 128) | U256::from(lo), want, "{a} · {b}");
            }
        }
    }

    /// The 256-bit fast path orders every pair exactly as the `U512` products do — across
    /// magnitudes on both sides of 2^64 and 2^128 (the fast path's edge), both signs and both
    /// rank shapes, ties included.
    #[test]
    fn the_fast_comparison_is_the_u512_order() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut ranks = Vec::new();
        for i in 0..4000 {
            let bits = [8u32, 40, 63, 64, 65, 100, 126, 127][i % 8];
            let mut val = |sign: bool| {
                let v = ((next() as u128) << 64 | next() as u128) >> (128 - bits);
                let v = v.min(i128::MAX as u128) as i128;
                if sign && next() % 2 == 0 {
                    -v
                } else {
                    v
                }
            };
            let (u, e, n) = (val(true), val(false).max(1), val(true));
            ranks.push(if i % 3 == 0 {
                AdlRank::ratio(u, e)
            } else {
                AdlRank::sign_split(u, e, n)
            });
            if i % 50 == 0 {
                // Equal values in different terms: `k·u / k·e`.
                ranks.push(AdlRank::ratio(u / 2 * 2, e.max(2) / 2 * 2));
                ranks.push(AdlRank::ratio(u / 2, e.max(2) / 2));
            }
        }
        let mut fast_paths = 0;
        for (i, a) in ranks.iter().enumerate() {
            for b in ranks.iter().skip(i % 7).step_by(7) {
                assert_eq!(a.cmp(b), reference(a, b), "{a:?} vs {b:?}");
                if [a.num, a.den, b.num, b.den].iter().all(|v| super::u128_of(v).is_some()) {
                    fast_paths += 1;
                }
            }
        }
        assert!(fast_paths > 100_000, "the fast path was exercised ({fast_paths})");
    }
}
