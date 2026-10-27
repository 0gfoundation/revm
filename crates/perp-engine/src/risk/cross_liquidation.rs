//! Cross liquidation (cross margin P3): account-level detection and the liquidation EPISODE.
//!
//! A position CROSSED in force has no silo. Its solvency is the ACCOUNT's — liquidatable exactly
//! when the cross margin balance `E = cWB + Σ_C uPnL_k` is below the cross maintenance margin
//! `Σ_C MM_k` (`margin_view::CrossHealth::below_cross_maintenance`) — so it is never judged, closed
//! or charged per position the way `risk::liquidate_position` treats an ISOLATED one. Both callers
//! route a cross position here instead: the protocol sweep (`run_liquidation_sweep`, on every tick
//! of a market the account holds a position in) and the permissionless `liquidate()`.
//!
//! # Detection is complete
//!
//! `E − Σ MM` only moves when (a) the mark or the funding index of a market the account holds a
//! cross position in moves — which happens only in that market's tick, whose sweep scans its
//! registry, and the account is in it — or (b) the account itself (or a counterparty, through ADL
//! or a fill) writes, which the next tick of any of its markets then sees. So scanning
//! `registry(m)` on `m`'s tick and judging each cross candidate on its whole account catches every
//! account at the latest on the next tick of any market it holds (cross report §3.4); a keeper's
//! `liquidate()` closes even that window.
//!
//! # The episode ([`liquidate_cross_account`])
//!
//! 1. VALIDATE on the TOTAL health check, writing nothing: `E ≥ Σ MM` is healthy (a flag left over
//!    from an earlier episode is then cleared, and that is all).
//! 2. FLAG the account (`UserAccount::liq_flags`, Binance `-2023`: only reduce-only /
//!    closePosition orders until it ends) and emit `CrossLiquidationStarted`.
//! 3. CANCEL every open order in every market of the account — book orders and untriggered
//!    conditional / trailing / closePosition orders, ISOLATED markets included (owner decisions Q2,
//!    R3). One step per market that has anything to cancel.
//! 4. CLOSE market by market: the trigger market first (its mark is the fresh one, the band centred
//!    on it), then the other cross markets by `MM_k` descending, ties by market id ascending
//!    (deviation D2 — Binance publishes no order). Per market: settle its funding into the cross
//!    wallet; a LIMIT IOC for the whole `|amount_k|` at the MM-share bankruptcy price clamped into
//!    the band (Q4, Q5); the clearance fee on the notional closed, capped at `max(0, E_after)`
//!    (Q3); STOP once `E ≥ Σ MM` (Q6 — a partial liquidation); otherwise hand the unfilled rest to
//!    ADL at the mark while `E ≥ 0`, at the bankruptcy price clamped into the band otherwise (R5 —
//!    what the band's edge does not transfer stays in the wallet, for the account's silos and in
//!    the end the insurance fund, Q8), fee, stop check again. A market whose mark is `0` (J4) or
//!    older than `STALE_MULT` price intervals (Q10) is skipped — and while any leg is skipped so,
//!    no deficit sets an ADL price: the rest goes at the mark. One step per market.
//! 5. END: the flag is cleared (`CrossLiquidationEnded`) as soon as the account is healthy again or
//!    holds no cross position; otherwise it stays set until a later tick finishes the job. A last
//!    cross position closed by any OTHER route while the flag is set — the user's own reduce-only
//!    orders, a fired stop, an ADL fill against the account — ends it at the end of that call
//!    ([`end_flag_without_cross_position`]): the flag never outlives the last cross position.
//!
//! # Bounded, and resumable
//!
//! Every step spends the caller's step budget (the sweep's `MAX_SWEEP_ACTIONS_PER_UPDATE`, shared
//! with the trigger legs; a fresh `MANUAL_CROSS_STEP_BUDGET` for `liquidate()`) and every ADL fill
//! the shared ADL budget, so per-tick work keeps the bound it had. Out of budget, the episode
//! records a CONTINUATION through the existing deferred-work mechanism — `Market::deferred_work`
//! on the next market in its order that still holds a cross position — and the next tick or
//! `drainDeferredWork` of that market resumes it: the account is in that market's registry and
//! still flagged. Every phase is idempotent (a market with nothing left to cancel costs nothing, a
//! closed market is skipped), so resuming is simply running the episode again. Any tick of ANY
//! market the account still holds a cross position in resumes it just the same (the account is in
//! that registry too), and a flagged account found healthy there — a mark moved back, a deposit,
//! its own reduce-only closes — is un-flagged with that single write (`Recovered`). Work a same-
//! block retry could not finish (a stale mark, no ADL counterparty) records nothing: it waits for
//! the market's own next tick, or a keeper's `liquidate()` once that mark is fresh again.
//!
//! # TOTAL
//!
//! The sweep PANICS on any `Err` (a mid-apply failure under commit-only would leave a partial write
//! set). Everything here uses the total health check (`margin_view::cross_health`, the TOTAL
//! policy of `margin_view::cross_fold` on the account's stored cross legs — cross P4), skips what
//! it cannot evaluate (a leg naming a market that does not exist, a leg whose arithmetic faulted, a
//! mark of 0), clamps instead of failing (the bankruptcy price, the band, the fee), and saturates
//! the event fields. The only `Err` left is the storage layer's.

use alloy_primitives::IntoLogData;
use primitives::{Address, Log};

use crate::{
    host::PerpHost,
    interface::IPerpDex,
    margin_view::{cross_health, CrossHealth, CrossHealthLeg},
    storage,
    trading::{execute_liquidation_market_order, run_adl},
    types::{AccountUpdateReason, CancelReason, MarginType, Market, PerpPosition, Side},
    PerpError, PERP_DEX_ADDRESS,
};

/// Owner decision Q10: an episode does not close market `k` while `k`'s mark is older than
/// `STALE_MULT × k.price_update_interval` — it would be closing at a price the market may have long
/// left. `k` is then left to its own next tick (whose sweep sees the account in its registry) or to
/// a keeper. The market whose oracle tick is running the sweep is exempt: its mark was written by
/// this very transaction.
pub(crate) const STALE_MULT: u64 = 3;

/// The step budget a manual `liquidate()` of a CROSSED position runs its episode with (cross
/// report §3.4), on top of a fresh ADL budget. A cancel and a close are a step each, so it finishes
/// an account with orders in up to 8 markets, or one at the 16-market cap whose orders are already
/// gone; what it leaves is recorded as a continuation, exactly like the sweep's.
pub(crate) const MANUAL_CROSS_STEP_BUDGET: u32 = 16;

/// What [`liquidate_cross_account`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrossLiquidationOutcome {
    /// At or above cross maintenance and not flagged: NOTHING written. The sweep reverts its
    /// checkpoint; `liquidate()` refuses.
    Healthy,
    /// Flagged by an earlier episode, healthy again: the flag was cleared (`CrossLiquidationEnded`)
    /// and nothing else was written.
    Recovered,
    /// The episode ran — flagged, cancelled, closed what its budget and the market state allowed.
    /// `ended`: the flag was cleared in this call (healthy again, or no cross position left).
    Liquidated { ended: bool },
}

/// Who is running an episode, which decides two things.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EpisodeCaller {
    /// The market whose SWEEP runs the episode (its oracle tick or its drain) — a continuation on
    /// it is reported through the sweep's own deferral result instead of a second write of its
    /// `deferred_work`. `None` for `liquidate()`.
    pub(crate) sweep_market: Option<u64>,
    /// The market whose mark this very transaction just published (an oracle tick), exempt from
    /// the stale-mark guard. `None` on a drain and on `liquidate()`, where every mark is the stored
    /// one and is judged by its age.
    pub(crate) fresh_market: Option<u64>,
    /// Recorded verbatim in each `Liquidation` row (the caller for `liquidate()`, `Address::ZERO`
    /// for the sweep).
    pub(crate) liquidator: Address,
}

/// One cross liquidation EPISODE for `user`, starting at `start_market` — see the module docs.
///
/// `budget` / `adl_budget` are the caller's step and ADL-fill budgets, spent here. `deferred` is
/// OR-ed with `true` when work this call could not finish belongs to `caller.sweep_market` (the
/// sweep reports it as its own deferral); work belonging to any other market sets that market's
/// `deferred_work` directly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn liquidate_cross_account<H: PerpHost>(
    context: &mut H,
    user: Address,
    start_market: u64,
    caller: EpisodeCaller,
    budget: &mut u32,
    adl_budget: &mut u32,
    deferred: &mut bool,
) -> Result<CrossLiquidationOutcome, PerpError> {
    // ── (a) VALIDATE — the account-level predicate on the TOTAL health check; NO write before a
    // refusal ──
    //
    // The health check prices the account from its stored cross legs (cross P4): the account,
    // its cross-leg blob and its legs' markets, no index, no position — the whole cost of a
    // healthy candidate, which is what almost every sweep candidate is. No per-leg terms on this
    // path either.
    let health = cross_health(context, user, None)?;
    let flagged = storage::load_account_ref(context, user)?.in_liquidation();
    if !health.below_cross_maintenance() {
        if !flagged {
            return Ok(CrossLiquidationOutcome::Healthy);
        }
        // An episode an earlier call left unfinished, and the account recovered in between (a
        // mark moved back, a deposit, the user's own reduce-only closes): one write ends it.
        end_episode(context, user, start_market, health.e, health.cross_mm)?;
        return Ok(CrossLiquidationOutcome::Recovered);
    }
    // Under maintenance: the per-leg terms the close order and the priced check read, off the
    // same state (nothing has been written since the check above).
    let mut legs = Vec::new();
    let _same = cross_health(context, user, Some(&mut legs))?;
    debug_assert_eq!(_same, health, "cross_health is a pure read");

    // ── (b) FLAG — Binance's "liquidation mode", on the transition only ──
    if !flagged {
        // `mutate_account`: the flag's dedicated, SILENT writer (`save_account` carries the stored
        // flag over its caller's copy, and no balance moves here, so nothing is published).
        storage::mutate_account(context, user, |a| {
            a.liq_flags |= crate::types::LIQ_FLAG_IN_LIQUIDATION
        })?;
        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::CrossLiquidationStarted {
                user,
                triggerMarketId: start_market,
                crossMarginBalance: saturate_i64(health.e),
                totalCrossMaintMargin: saturate_u64(health.cross_mm),
            }
            .to_log_data(),
        });
    }
    // Decided ONCE, from the detection legs: a close moves `MM_k` only of the market it closes.
    let order = close_order(&legs, start_market);

    // ── (c) CANCEL ALL — every market of the account, isolated ones included (Q2, R3) ──
    //
    // Cancelling moves neither `E` nor `Σ MM` (no order escrows anything), so it cannot change the
    // stop condition; it runs first so the closes below never meet the account's own orders. The
    // per-user market index is sorted, so the walk is deterministic. Out of budget here, the flag
    // already freezes what is left (a flagged maker is cancelled when the book reaches it, R1
    // exempting only what can only reduce).
    let markets = storage::load_user_markets_ref(context, user)?;
    for &k in markets.iter() {
        if !has_open_orders(context, user, k)? {
            continue;
        }
        if *budget == 0 {
            record_continuation(context, user, &order, caller, deferred)?;
            return Ok(CrossLiquidationOutcome::Liquidated { ended: false });
        }
        // J4: an index entry naming no market is corrupt state, left out rather than failed on.
        let Some(market) = storage::load_market_ref(context, k)? else {
            continue;
        };
        crate::risk::cancel_all_orders_for_market(
            context,
            user,
            k,
            &market,
            CancelReason::Liquidation,
        )?;
        *budget -= 1;
    }

    // ── (d) / (e) CLOSE — market by market, re-checking after each ──
    //
    // Whether the deficit may set an ADL price at all: only while every cross leg is priced at a
    // mark this call trusts (see `close_market`). Decided once — no mark but this transaction's own
    // moves within it, and the block clock does not.
    let priced = every_cross_leg_is_priced(context, &health, &legs, caller)?;
    let mut continuation: Option<u64> = None;
    for (i, &k) in order.iter().enumerate() {
        let pos = storage::load_position_ref(context, user, k)?;
        if pos.amount == 0 || !is_crossed(&pos) {
            continue;
        }
        let Some(market) = storage::load_market_ref(context, k)? else {
            continue;
        };
        // J4: no price, nothing to close at (the health check already counts the leg as
        // `uPnL = MM = 0`).
        if market.mark_price == 0 {
            continue;
        }
        // Q10: a stale mark is not a price to liquidate at.
        if caller.fresh_market != Some(k) && mark_is_stale(context, &market)? {
            continue;
        }
        if *budget == 0 {
            record_continuation(context, user, &order[i..], caller, deferred)?;
            return Ok(CrossLiquidationOutcome::Liquidated { ended: false });
        }
        let Some(step) =
            close_market(context, user, &market, caller.liquidator, adl_budget, priced)?
        else {
            // The leg faulted in the health check (J4): nothing evaluable to close it at, no step
            // spent.
            continue;
        };
        *budget -= 1;
        if step.leftover && continuation.is_none() {
            continuation = Some(k);
        }
        if step.healthy {
            // Q6: back at or above cross maintenance — the rest of the account is the user's
            // again (a PARTIAL liquidation).
            end_episode(context, user, k, step.e, step.cross_mm)?;
            return Ok(CrossLiquidationOutcome::Liquidated { ended: true });
        }
    }

    // Every market this call could reach has been tried. Healthy now (or no cross position left)
    // ends the episode; otherwise the flag stays, and the work that a same-block retry could
    // finish (an ADL budget or a maker cap that ran out) is recorded on its market.
    let health = cross_health(context, user, None)?;
    if !health.below_cross_maintenance() {
        end_episode(context, user, start_market, health.e, health.cross_mm)?;
        return Ok(CrossLiquidationOutcome::Liquidated { ended: true });
    }
    if let Some(k) = continuation {
        mark_deferred(context, k, caller, deferred)?;
    }
    Ok(CrossLiquidationOutcome::Liquidated { ended: false })
}

/// The episode's close order (deviation D2): `start_market` first when it holds a cross position
/// — the market whose tick caught the account, with the fresh mark and the band centred on it —
/// then every other cross market holding a position by `MM_k` DESCENDING (the largest share of the
/// requirement first, so the stop condition is reached in the fewest closes), ties by market id
/// ASCENDING. Built from the detection's health legs — every one a position CROSSED in force and
/// non-zero; a faulted leg is not among them (J4).
fn close_order(legs: &[CrossHealthLeg], start_market: u64) -> Vec<u64> {
    let mut rest: Vec<(u64, i128)> = legs
        .iter()
        .filter(|l| l.market_id != start_market)
        .map(|l| (l.market_id, l.mm))
        .collect();
    rest.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut order = Vec::with_capacity(rest.len() + 1);
    if legs.iter().any(|l| l.market_id == start_market) {
        order.push(start_market);
    }
    order.extend(rest.into_iter().map(|(k, _)| k));
    order
}

/// What one market's close left behind.
struct CloseStep {
    /// The account is at or above cross maintenance after the close and its fees (or holds no
    /// cross position any more) — the stop condition.
    healthy: bool,
    /// `E` and `Σ_C MM` after the close and its fees.
    e: i128,
    cross_mm: i128,
    /// Same-block-recoverable work was left in this market: the book leg stopped on the
    /// distinct-maker cap, or the ADL leg on the shared ADL budget with a residual still open.
    leftover: bool,
}

/// Close `user`'s cross position in `market` — one episode step. `None` when the leg cannot be
/// evaluated (it faulted in the health check, J4), with nothing written. `priced`: every cross leg
/// of the account is priced at a mark the episode trusts ([`every_cross_leg_is_priced`]) — without
/// it no deficit sets an ADL price (see the ADL leg below).
fn close_market<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    liquidator: Address,
    adl_budget: &mut u32,
    priced: bool,
) -> Result<Option<CloseStep>, PerpError> {
    let k = market.market_id;
    let mark = market.mark_price;

    // The bankruptcy price off the health check BEFORE anything moves. Pending funding is already
    // in `E` (`cWB`), and settling it below moves `W` by exactly what `cWB` held for it, so the
    // price is the same before and after the settle.
    let mut legs = Vec::new();
    let health = cross_health(context, user, Some(&mut legs))?;
    if !legs.iter().any(|l| l.market_id == k) {
        return Ok(None);
    }

    // ── Funding first, into the cross wallet ──
    //
    // `compute_funding_settlement` refuses a CROSSED position settled without its account (the
    // payment belongs to the cross wallet), so the account goes with it. Settled here rather than
    // left to the close's match: a book leg that fills nothing would never touch the taker, and
    // ADL would then close the position with its accrued funding never settled — forgiven or lost.
    settle_cross_funding(context, user, market)?;

    let pos = storage::load_position(context, user, k)?;
    let liq_amount = pos.amount;
    let qty = liq_amount.unsigned_abs();
    let side = if liq_amount > 0 { Side::Sell } else { Side::Buy };

    // ── (d) the IOC at the MM-share bankruptcy price, clamped into the band (Q4) ──
    let bankruptcy = mm_share_bankruptcy_price(market, &pos, &health, &legs);
    let limit = clamp_into_band(bankruptcy, market);
    let close = execute_liquidation_market_order(context, user, market, side, qty, Some(limit))?;
    let mut leftover = close.maker_cap_deferred;
    let (mut health, mut reward) =
        charge_fee_and_check(context, user, market, qty - close.remaining)?;

    // ── (e) the unfilled rest → ADL, unless the IOC already restored the account (Q6) ──
    if health.below_cross_maintenance() && close.remaining > 0 {
        // R5: at the MARK while the account still has equity (the winners lose exposure, not
        // money), at the MM-share bankruptcy price once it has none (the winners absorb this
        // market's share of the deficit). Recomputed on the post-IOC state.
        //
        // ── …CLAMPED INTO THE BAND, like the IOC's limit (P3 review) ──
        //
        // `share_k = E · MM_k / Σ MM` is the whole deficit when the account's other legs are
        // gone: a deficit already REALIZED into the wallet (a reduce-only close through
        // bankruptcy, a funding run, an earlier fallback fill) next to one small leg makes `Σ MM ≈
        // MM_k`, and the unclamped price lands at any multiple of the mark — the J2 concentration
        // the MM split exists to prevent, arriving through `W` instead of through other legs'
        // uPnL, and past `i64` for a large holder at that price. The band is the farthest from
        // the mark this episode trades at, book or ADL: the winners pay at most the band's edge,
        // and what the clamped price does not transfer STAYS IN THE WALLET as a receivable — the
        // account's isolated silos net it as they close, and once it holds no position the
        // insurance fund clears the rest (owner decision Q8), the same fund an isolated close's
        // bad debt reaches. (A market whose band is set wide open accepts the wider price.)
        //
        // ── …and only off a health check every leg of which is priced (P3 review) ──
        //
        // The deficit `E` — and so `share_k` — sums EVERY cross leg at its stored mark, a leg the
        // episode skips (stale, Q10; mark 0 or unpriceable, J4) included. A leg it will not close
        // at its mark is not a price to charge this market's winners a share of a deficit by
        // either: a stale wick would make them pay a deficit that exists only at the stale price
        // (and the stale leg, left open, would hand the difference back to the account when its
        // oracle prints again). So while any leg is unpriced the ADL fills at the MARK — the
        // winners neutral, as at `E ≥ 0` — and the deficit waits on the account until that leg's
        // own tick prices it. The IOC's limit is left as it is: the book trades by choice.
        let price = if health.e >= 0 || !priced {
            mark
        } else {
            let pos = storage::load_position(context, user, k)?;
            let mut legs = Vec::new();
            let h = cross_health(context, user, Some(&mut legs))?;
            clamp_into_band(mm_share_bankruptcy_price(market, &pos, &h, &legs), market)
        };
        run_adl(context, user, market, mark, adl_budget, Some(price))?;
        let after = storage::load_position_ref(context, user, k)?.amount.unsigned_abs();
        let (h, fee) =
            charge_fee_and_check(context, user, market, close.remaining.saturating_sub(after))?;
        health = h;
        reward = reward.saturating_add(fee);
        leftover |= after > 0 && *adl_budget == 0;
    }

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Liquidation {
            user,
            marketId: k,
            liquidator,
            amount: liq_amount,
            reward,
            markPrice: mark,
            marginType: MarginType::Crossed.code(),
        }
        .to_log_data(),
    });
    Ok(Some(CloseStep {
        healthy: !health.below_cross_maintenance(),
        e: health.e,
        cross_mm: health.cross_mm,
        leftover,
    }))
}

/// `price` clamped into `market`'s band around its mark (`math::mark_band_bounds`), the farthest a
/// cross episode trades from the mark — its IOC's limit (Q4) and its ADL price alike. Total: the
/// bounds are `u128`, the result saturates to `u64`.
fn clamp_into_band(price: u64, market: &Market) -> u64 {
    let (upper, lower) = crate::math::mark_band_bounds(market.mark_price, market.price_band_bps);
    (price as u128).max(lower).min(upper).min(u64::MAX as u128) as u64
}

/// The clearance fee on `closed` (Q3: the notional closed × the market's rate, capped at
/// `max(0, E_after)`), then the health check the stop condition reads, fee included ("after
/// deducting realized losses and the Liquidation Clearance Fee").
///
/// ⚠️ A bankrupt account pays (almost) nothing — up to ROUNDING DUST, not exactly 0. Closed at its
/// MM-share bankruptcy price ([`mm_share_bankruptcy_price`], rounded in the account's favour),
/// market `k` moves `E` by `−share_k + ov_k`, `ov_k` being that price's overshoot over its target
/// notional. For a leg of `q` contracts `0 ≤ ov_k ≤ calc_value(1, q)` (one more on a short), and
/// `ov_k ≤ calc_value(1, q) − 1` wherever `pd + bd ≤ 6`, as on every devnet market (the bound is
/// `perp_core::math::calc_bankruptcy_price`'s). The share truncates toward zero, so
/// `E_after ≤ ov_k` — with equality on the last bankrupt leg, whose share is all of `E` — and the
/// fee is at most `min(due, ov_k)`: the dust goes to the insurance fund, not to the account (on the
/// last leg its wallet ends at `ov_k − fee`, 0 whenever the due covers it). A short split across
/// `n` ADL fills where `pd + bd > 6` adds up to `n − 1` quote units, an IOC that filled part of the
/// leg at its limit up to one. The devnet gate's case — long 0.01 BTC, `E = −26_359_001`: fee 99
/// of 3_163_005 due — is pinned by `cross_liquidation_tests`'s
/// `a_bankrupt_cross_account_pays_its_bankruptcy_price_rounding_dust_as_the_clearance_fee`. A close
/// the band clamps, or one ADL'd at the mark because a leg is unpriced, leaves `E_after < 0` and
/// pays 0; an IOC fill BETTER than its limit is no dust — a real surplus, charged up to the due.
fn charge_fee_and_check<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    closed: u64,
) -> Result<(CrossHealth, u64), PerpError> {
    let health = cross_health(context, user, None)?;
    let fee = crate::risk::charge_clearance_fee(
        context,
        user,
        market,
        market.mark_price,
        closed,
        health.e.max(0),
    )?;
    if fee == 0 {
        return Ok((health, 0));
    }
    // The fee left the wallet, so `E` fell by exactly it and `Σ MM` did not move — but re-price
    // rather than patch: the health is never adjusted by a delta (R13).
    Ok((cross_health(context, user, None)?, fee))
}

/// The MM-share bankruptcy price of `pos` (cross report §3.2, owner decision Q5, deviation D4):
/// the price at which closing the whole position realizes `uPnL_k(mark) − share_k`, with
/// `share_k = E · MM_k / Σ MM` — this market's proportional share of the account's equity, or of
/// its deficit when `E < 0`. Closing every market at its share leaves the ratio `E / Σ MM`
/// unchanged at each step and ends at `E = 0`: the deficit is split across the account's cross
/// markets by maintenance margin, not dumped on whichever market ticked first (J2).
///
/// In `calc_bankruptcy_price`'s terms the target is a closing notional of `N_k(mark) − share_k`,
/// rounded in the ACCOUNT's favour (a long sells at no less, a short buys at no more). `0` where no
/// positive price exists (a long may then sell at any price; a short can buy back at none).
///
/// TOTAL: an `i128` product that would overflow falls back to dividing first; a target outside
/// `i64` saturates to the price bound it points at; an arithmetic failure returns the mark (the
/// band clamp follows). A leg missing from `legs` — faulted — returns the mark too.
fn mm_share_bankruptcy_price(
    market: &Market,
    pos: &PerpPosition,
    health: &CrossHealth,
    legs: &[CrossHealthLeg],
) -> u64 {
    let mark = market.mark_price;
    let Some(leg) = legs.iter().find(|l| l.market_id == market.market_id) else {
        return mark;
    };
    let share = if health.cross_mm > 0 {
        match health.e.checked_mul(leg.mm) {
            Some(p) => p / health.cross_mm,
            None => (health.e / health.cross_mm).saturating_mul(leg.mm),
        }
    } else {
        // `Σ MM == 0` with the account below it means `E < 0` and every leg priced at `MM = 0`:
        // the whole deficit is this market's to clear.
        health.e
    };
    // `N_k(mark)`: the leg's `uPnL` less the entry basis (`uPnL = N + vQuote`, mark ≠ 0 here).
    let target = leg.upnl - pos.v_quote_balance as i128 - share;
    // A long's price rises with the target, a short's falls (its notional is negative).
    let bound = if (target > 0) == (pos.amount > 0) { u64::MAX } else { 0 };
    let Some(neg_target) = i64::try_from(target).ok().and_then(i64::checked_neg) else {
        return bound;
    };
    crate::math::calc_bankruptcy_price(
        pos.amount,
        neg_target,
        0,
        market.base_decimals,
        market.price_decimals,
    )
    .unwrap_or(mark)
}

/// Every cross position of `user` is priced at a mark the episode trusts: a leg the detection's
/// health check evaluated (`legs`; one it could not — a leg naming no market, or one whose
/// arithmetic faulted, J4 — is counted in `health.faulted_legs` instead), with a mark that is
/// neither `0` (J4) nor stale (Q10, judged as the close loop judges it — the market whose tick runs
/// the episode is fresh). The legs the close loop would skip are exactly the ones that fail it.
///
/// The account's cross legs ARE its non-zero positions CROSSED in force (`types::CrossLegs`), so
/// the evaluated legs and the faulted count cover every one of them without a position walk.
/// Total: market and index-state loads.
fn every_cross_leg_is_priced<H: PerpHost>(
    context: &mut H,
    health: &CrossHealth,
    legs: &[CrossHealthLeg],
    caller: EpisodeCaller,
) -> Result<bool, PerpError> {
    if health.faulted_legs != 0 {
        return Ok(false);
    }
    for leg in legs {
        let k = leg.market_id;
        let Some(market) = storage::load_market_ref(context, k)? else {
            return Ok(false);
        };
        if market.mark_price == 0
            || (caller.fresh_market != Some(k) && mark_is_stale(context, &market)?)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Settle `user`'s pending funding in `market` into the cross wallet, persisted and published the
/// way every settled path publishes it (`FundingSettled` + a B-only `FUNDING_FEE` group). Nothing
/// written when nothing is owed.
fn settle_cross_funding<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
) -> Result<(), PerpError> {
    let mut pos = storage::load_position(context, user, market.market_id)?;
    let mut account = storage::load_account(context, user)?;
    let Some(pending) = crate::funding::compute_funding_settlement(
        context,
        user,
        market,
        &mut pos,
        Some(&mut account),
    )?
    else {
        // No payment: the re-anchor alone is left to the close's own legs, which settle (to 0)
        // and write the position anyway.
        return Ok(());
    };
    crate::funding::apply_funding_settlement(context, pending)?;
    // Position BEFORE account (`run_add_position_margin`'s order). The group just published is
    // derived from exactly these two copies, so the end-of-call drain would only repeat it — the
    // same clear `liquidate_position` makes after its own funding write.
    storage::save_position(context, user, market.market_id, &pos, AccountUpdateReason::FundingFee)?;
    storage::save_account(context, user, account, AccountUpdateReason::FundingFee)?;
    storage::clear_account_snapshot_mark(context, user);
    Ok(())
}

/// End `user`'s liquidation episode if the account is flagged but holds no cross position any more
/// — run by the end of a call in which the account closed a CROSSED position while flagged, or
/// closed its last position (`storage::flush_account_snapshots`; recorded by `save_position`).
///
/// The episode cannot see this itself when the close is not its own: the user's reduce-only orders
/// (accepted while flagged, Q11, and filled as a maker, R1), a fired stop, the account ADL'd as a
/// winner, an isolated liquidation of its last position. Without it the flag would outlive the
/// account's last cross position — and no sweep would ever clear it, since only a market whose
/// registry holds the account visits it. `marketId` on `CrossLiquidationEnded` is the market of
/// that last close; the balances are the account's then (`E` its cross wallet, `Σ MM` 0).
///
/// Nothing to do — not flagged, or still holding a cross position (its markets' ticks resume the
/// episode, R6) — writes and emits nothing. Total: the account and the health check.
pub(crate) fn end_flag_without_cross_position<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<(), PerpError> {
    if !storage::load_account_ref(context, user)?.in_liquidation() {
        return Ok(());
    }
    // The stored cross legs rather than `CrossHealth::has_cross`: the health check leaves a leg it
    // cannot evaluate OUT of every sum (J4), and an unpriceable cross position is still a cross
    // position. The legs ARE the non-zero positions CROSSED in force — under the gate, which with
    // cross margin off (only a test can turn it off) makes none of them CROSSED.
    if crate::margin_type::cross_margin_enabled()
        && !storage::load_cross_legs_ref(context, user)?.is_empty()
    {
        return Ok(());
    }
    let health = cross_health(context, user, None)?;
    end_episode(context, user, market_id, health.e, health.cross_mm)
}

/// Clear the liquidation flag and emit `CrossLiquidationEnded` with the values that ended it.
fn end_episode<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    e: i128,
    cross_mm: i128,
) -> Result<(), PerpError> {
    storage::mutate_account(context, user, |a| {
        a.liq_flags &= !crate::types::LIQ_FLAG_IN_LIQUIDATION
    })?;
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::CrossLiquidationEnded {
            user,
            marketId: market_id,
            crossMarginBalance: saturate_i64(e),
            totalCrossMaintMargin: saturate_u64(cross_mm),
        }
        .to_log_data(),
    });
    Ok(())
}

/// Record the continuation of an unfinished episode on the first market of `candidates` (a suffix
/// of the episode order) that still holds a cross position. Nothing to record when none does.
fn record_continuation<H: PerpHost>(
    context: &mut H,
    user: Address,
    candidates: &[u64],
    caller: EpisodeCaller,
    deferred: &mut bool,
) -> Result<(), PerpError> {
    for &k in candidates {
        let pos = storage::load_position_ref(context, user, k)?;
        if pos.amount != 0 && is_crossed(&pos) {
            return mark_deferred(context, k, caller, deferred);
        }
    }
    Ok(())
}

/// Flag `market_id` as holding same-block-recoverable work: through the sweep's own result when
/// it is the market the sweep runs for, through its `deferred_work` bit otherwise (written only on
/// the transition, like `run_deferred_sweeps` does).
fn mark_deferred<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    caller: EpisodeCaller,
    deferred: &mut bool,
) -> Result<(), PerpError> {
    if caller.sweep_market == Some(market_id) {
        *deferred = true;
        return Ok(());
    }
    if let Some(m) = storage::load_market_ref(context, market_id)? {
        if m.deferred_work == 0 {
            storage::save_deferred_work(context, market_id, true)?;
        }
    }
    Ok(())
}

/// Q10: `market`'s stored mark is older than `STALE_MULT × price_update_interval`, judged by the
/// index timestamp it was published at against the block time. A timestamp ahead of the block
/// (an oracle that stamps its own clock) is fresh.
fn mark_is_stale<H: PerpHost>(context: &mut H, market: &Market) -> Result<bool, PerpError> {
    let published = storage::load_index_price_state(context, market.market_id)?.timestamp;
    let max_age = STALE_MULT.saturating_mul(market.price_update_interval);
    Ok(context.timestamp().saturating_sub(published) > max_age)
}

/// Book orders or untriggered conditional orders of `user` in `market_id`.
fn has_open_orders<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<bool, PerpError> {
    Ok(storage::book_order_count(context, user, market_id)? != 0
        || !storage::load_user_algo_orders(context, user, market_id)?.is_empty())
}

fn is_crossed(pos: &PerpPosition) -> bool {
    crate::margin_type::effective_margin_type(pos) == MarginType::Crossed
}

fn saturate_i64(v: i128) -> i64 {
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn saturate_u64(v: i128) -> u64 {
    v.clamp(0, u64::MAX as i128) as u64
}
