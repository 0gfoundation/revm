//! Liquidation: the ISOLATED close, the clearance fee, `liquidate`, and the per-update
//! liquidation sweep with the deferred-work driver that runs it.

use super::*;

// ── Liquidation ───────────────────────────────────────────────────────────────

/// Result of a single `liquidate_position` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiquidationOutcome {
    /// Position was closed; carries the pre-close signed amount and the clearance
    /// fee credited to the Insurance Fund.
    Liquidated { amount: i64, reward: u64 },
    /// No open position (`amount == 0`) — nothing to do.
    NoPosition,
    /// Position is at or above the maintenance-margin threshold — not liquidatable.
    AboveMaintenance,
}

/// Core ISOLATED liquidation logic, shared by the manual `liquidate` entry point
/// (`run_liquidate`) and the protocol-automatic sweep (inside
/// `run_update_index_price`). The caller must already hold the loaded `market`
/// and a non-zero `mark_price` (the mark==0 degenerate case is rejected by the
/// wrapper). A position CROSSED in force is never brought here: both callers route it to the
/// account-level episode (`cross_liquidation::liquidate_cross_account`). Settles funding on the
/// position first (so the charge counts toward insolvency), then:
/// - `amount == 0`  → `NoPosition` (no state change beyond the funding settle);
/// - above maintenance → `AboveMaintenance`;
/// - otherwise closes through the orderbook, settles any residual at mark, charges
///   the clearance fee to the IF, emits `Liquidation`, and returns `Liquidated`.
///
/// `liquidator` is recorded verbatim in the `Liquidation` event (the caller for a
/// manual liquidation; a system address for the sweep). Bad debt from the close
/// legs is routed to the IF inside the close paths, not here.
pub(crate) fn liquidate_position<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    market: &crate::types::Market,
    mark_price: u64,
    liquidator: Address,
    adl_budget: &mut u32,
    // `deferred`: OR-ed with `true` when this liquidation left work a LATER ATTEMPT IN THE SAME
    // BLOCK could finish — i.e. it stopped on a per-call budget, not on the market's actual state.
    // Threaded like `adl_budget` rather than returned, because `LiquidationOutcome` is also the
    // AboveMaintenance/NoPosition answer and neither of those says anything about deferral.
    deferred: &mut bool,
) -> Result<LiquidationOutcome, PerpError> {
    let mut pos = storage::load_position(context, user, market_id)?;

    if pos.amount == 0 {
        return Ok(LiquidationOutcome::NoPosition);
    }
    // ── ISOLATED only ─────────────────────────────────────────────────────────────────────────
    //
    // Everything below is the ISOLATED liquidation: a per-position maintenance test on the silo,
    // a close of this one market, a clearance fee capped at what this close returns. None of it is
    // defined for a position CROSSED in force — it has no silo (`pos.margin == 0`), so the
    // predicate would call it insolvent on its first unit of loss, and its losses settle against
    // the shared wallet. Both callers route such a position to the account-level episode instead
    // (`cross_liquidation`), so this is unreachable; answered with NO write — before the funding
    // compute, which for a cross position needs the account this path does not load — rather than
    // with an `Err`, because the sweep panics on any `Err` from this function.
    if crate::margin::margin_type::effective_margin_type(&pos)
        == crate::types::MarginType::Crossed
    {
        debug_assert!(false, "liquidate_position: a CROSSED position must be routed to the episode");
        return Ok(LiquidationOutcome::AboveMaintenance);
    }
    // commit-only #23: compute accrued funding IN MEMORY first (funding writes nothing), so the
    // maintenance check sees the post-funding position — including a funding DEBT, which is what
    // lets funding alone liquidate a position held up by profit — but an AboveMaintenance outcome leaves
    // ZERO writes. This is what makes the liquidation sweep's healthy-candidate scan write-free
    // (previously every scanned healthy account wrote a funding settle that only checkpoint_revert
    // discarded) and makes manual liquidate reject cleanly without relying on undo.
    //
    // Funding settles against `pos.margin` only, so this path no longer loads/writes the account at
    // all: the load existed solely to hand the wallet to the settlement, and the matching
    // `save_account` below would have re-written a byte-identical blob (and emitted a spurious
    // `AccountBalanceChanged` for a balance that did not move). The liquidation's real wallet
    // movements happen inside the close legs, which write the account themselves.
    // `None`: this path deliberately does NOT load the account (the load was removed when funding
    // stopped touching the wallet — see the note above), and the header the funding group needs is
    // only built when funding actually moved money, so the write-free healthy-candidate scan still
    // reads no account blob.
    let pending_funding = compute_funding_settlement(context, user, market, &mut pos, None)?;
    if is_above_maintenance_margin(
        &market.tiers,
        mark_price,
        pos.amount,
        pos.v_quote_balance,
        pos.margin,
        market.base_decimals,
        market.price_decimals,
    )? {
        return Ok(LiquidationOutcome::AboveMaintenance);
    }

    let liq_amount = pos.amount;
    let liquidation_side = if pos.amount > 0 {
        Side::Sell
    } else {
        Side::Buy
    };
    let liquidation_quantity = pos.amount.unsigned_abs();

    // ── APPLY (liquidatable — commit the funding settle, then close) ──
    let funding_published = pending_funding.is_some();
    if let Some(p) = pending_funding {
        apply_funding_settlement(context, p)?;
    }
    // `FundingFee`: the ONLY thing this write persists is what `compute_funding_settlement` did in
    // memory — the funding credit/charge on `pos.margin` plus the `last_funding_index` re-anchor.
    // The close's own legs mark with their own reasons below. Normally superseded: the close
    // publishes for this user and clears the mark. It survives to the drain only when the close
    // publishes nothing at all (empty book AND an insolvent residual AND no eligible ADL holder),
    // where it then meets the clearance fee's `InsuranceClear` and merges to `Multiple`.
    storage::save_position(context, user, market_id, &pos, AccountUpdateReason::FundingFee)?;
    // That write MARKS the user, but it persists EXACTLY the `pos` the group `apply_funding_settlement`
    // just published was derived from — nothing touches it in between — so the end-of-call drain
    // would repeat that payload as a 0-position group with no news in it. Clear it, on the same
    // structural-equality argument as `run_adl`'s loser clear, and only when something really was
    // published.
    //
    // ⚠️ This used to be invisible because the clearance fee ALWAYS wrote the account a few lines
    // down and so always gave the drained row something to say. Now that the fee is levied on
    // what the call actually closed, a liquidation that closes nothing charges nothing — and without
    // this clear it would emit a duplicate. Any later leg that moves money (a fill, ADL, the fee)
    // re-marks the user and the settled end state is still published.
    if funding_published {
        storage::clear_account_snapshot_mark(context, user);
    }

    // Cancel all open orders for this user/market (emits OrderCancelled events).
    cancel_all_orders_for_market(context, user, market_id, market, CancelReason::Liquidation)?;

    // The wallet BEFORE the close legs — the base of the clearance fee's J5 cap (the close credit
    // this close returns to it). After the funding settle, which moves no wallet for an isolated
    // position (it settles against `pos.margin`), and after the cancel-all, which moves no money.
    let pre_close_wallet = storage::load_account_ref(context, user)?.perp_wallet_balance;

    // Try to close through the orderbook; ADL any residual.
    let close = execute_liquidation_market_order(
        context,
        user,
        market,
        liquidation_side,
        liquidation_quantity,
        None,
    )?;
    let remaining = close.remaining;
    // The book leg stopped on the distinct-maker cap, which is per MATCH: a retry gets a fresh
    // allowance against the same book. Same-block recoverable, so the drain must see it.
    *deferred |= close.maker_cap_deferred;
    if remaining > 0 {
        // EVERY residual — solvent or not — is closed as a REAL forced trade against opposite-side
        // holders via ADL, which picks its own price (mark when the residual is still solvent, the
        // bankruptcy price when it is not). ADL leftover (budget exhausted / no eligible opposite
        // holder) stays open and is re-swept next update.
        //
        // ⚠️ This used to fork: a solvent residual was closed at mark against NO COUNTERPARTY — the
        // protocol simply valued it and zeroed it. That was the one and only place `Σ amount`
        // stopped being conserved, and it leaked permanently: after such a close `Σ long != Σ
        // short`, so every subsequent price move creates or destroys money system-wide with no
        // position on the other side to absorb it, and nothing even recorded the imbalance
        // (`open_interest` has no production writer). Routing it through ADL makes "a position is
        // only ever closed against another position" an UNCONDITIONAL invariant — see
        // `liquidation_conserves_sigma_amount`.
        //
        // The accepted cost is that ADL now fires in any move fast enough to put the book outside
        // the fill-time band, i.e. routinely rather than exceptionally, and that a residual with no
        // eligible counterparty stays open below maintenance instead of being force-closed. Both
        // are deliberate: the frequency is held down by capping leverage and shortening the price
        // update interval, and a deferred position was already the behaviour for insolvent
        // residuals.
        run_adl(context, user, market, mark_price, adl_budget, None)?;
    }

    // ── THE CLEARANCE FEE: a rate on the NOTIONAL this call closed (Q3), capped at the CLOSE
    // CREDIT it returned to the wallet (J5) ───────────────────────────────────────────────────────
    //
    // Binance's Liquidation Clearance Fee is `Position Nominal Value × Liquidation Fee Rate`
    // (cross report F21), and owner decision Q3 makes it that for BOTH margin types:
    // `notional(mark, closed) × liquidation_fee_rate_bps / 10_000`. It used to be the same rate on
    // the silo margin the close CONSUMED, which only an isolated position has — a cross position's
    // `margin` is always 0 — so one rule for both modes needs the notional basis. See
    // [`charge_clearance_fee`] for the arithmetic and the rate's semantics (R7).
    //
    // ── ON WHAT THIS CALL ACTUALLY CLOSED ──────────────────────────────────────────────────────
    //
    // `closed` is the quantity THIS call took off the position (book leg + ADL), never the entry
    // size. A liquidation does NOT always finish in one call: the book leg can stop on the band or
    // the distinct-maker cap, and the ADL leg on the shared fill budget or on too little eligible
    // opposite interest. The position then stays open and is re-swept — and a fee on the size at
    // ENTRY would charge the user again every round, a geometric series paid for OUR gas caps. On
    // the closed quantity the rounds sum to one fee (up to one unit of flooring per round), with no
    // state to remember that this victim was already charged, and a call that closed NOTHING —
    // empty book, no ADL budget — charges nothing.
    //
    // ── J5: AT MOST WHAT THE CLOSE RETURNED TO THE WALLET ──────────────────────────────────────
    //
    // The fee is debited from `perp_wallet_balance`, and since cross margin (P2) that is the CROSS
    // wallet: uncapped, an isolated liquidation's fee would quietly draw on the collateral of the
    // account's cross positions. Binance deducts it "from the assets made available for the
    // position", i.e. from what the close releases. So the cap is the close CREDIT — the wallet's
    // rise across the two close legs (`apply_position_fill` releases the silo plus the realized
    // PnL into it; a fill worse than the bankruptcy price returns nothing, its shortfall being bad
    // debt) — and never more than the wallet then holds: a close credit that only netted a
    // pre-existing negative wallet (a receivable) toward zero is not cash to charge, and a fee must
    // never create or deepen a deficit.
    //
    // ⚠️ So a BANKRUPT isolated liquidation pays (almost) nothing — Binance's "no clearance fee for
    // a bankrupt position" holds up to ROUNDING DUST, not exactly. Closed through the book worse
    // than its bankruptcy price, it returns nothing and pays 0. But its book-unfillable residual is
    // ADL'd AT its bankruptcy price `P_b`, which `calc_bankruptcy_price` rounds in the position's
    // favour, so that close returns the equity surplus `S` left at `P_b`: up to one price unit's
    // worth of the residual `q` — `0 ≤ S ≤ calc_value(1, q)` (one more on a short), and
    // `≤ calc_value(1, q) − 1` wherever `pd + bd ≤ 6`, as on every devnet market — plus at most a
    // quote unit per ADL fill of flooring where `pd + bd > 6` or the close is partial. That dust IS
    // the close credit, so the fee is `min(due, dust)`: it goes to the insurance fund, not to the
    // victim, whose wallet nets `dust − fee` (0 whenever the due covers it). The devnet gate's
    // case — long 0.01 BTC @ $100,000 on a 332_333_333 silo, mark $40,000: `P_b = 6_676_667`,
    // `S = 33`, fee 33 of the 2_000_000 due — is pinned by
    // `a_bankrupt_isolated_adl_pays_its_bankruptcy_price_rounding_dust_as_the_clearance_fee`. A
    // book fill BETTER than `P_b` is no dust: it realizes a real surplus, charged up to the due.
    let post_amount = storage::load_position_ref(context, user, market_id)?.amount;
    let closed_quantity = liquidation_quantity.saturating_sub(post_amount.unsigned_abs());
    let post_close_wallet = storage::load_account_ref(context, user)?.perp_wallet_balance;
    let close_credit = (post_close_wallet as i128 - pre_close_wallet as i128).max(0);
    let fee_cap = close_credit.min(post_close_wallet.max(0) as i128);
    let clearance_fee = charge_clearance_fee(
        context,
        user,
        market,
        mark_price,
        closed_quantity,
        fee_cap,
    )?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Liquidation {
            user,
            marketId: market_id,
            liquidator,
            amount: liq_amount,
            reward: clearance_fee,
            markPrice: mark_price,
            marginType: crate::types::MarginType::Isolated.code(),
        }
        .to_log_data(),
    });

    Ok(LiquidationOutcome::Liquidated {
        amount: liq_amount,
        reward: clearance_fee,
    })
}

/// The liquidation CLEARANCE FEE due on `closed_quantity` closed in `market` at `mark_price` —
/// `notional × liquidation_fee_rate_bps / 10_000`, floored — before any cap.
///
/// # The rate (R7)
///
/// `Market::liquidation_fee_rate_bps` is a rate on the closed NOTIONAL, for both margin types —
/// Binance's `exchangeInfo.liquidationFee` (`0.0125` = 125 bps on BTCUSDT / ETHUSDT / BNBUSDT,
/// `0.015` on SOLUSDT; cross report F22). It used to be a rate on the isolated silo margin the
/// close consumed. The engine has no default of its own (every market states its rate at
/// `addMarket` / `updateMarket`), and the devnet markets were never calibrated on the old basis:
/// they all run at 0 (`perpdex-perf` `LIQUIDATION_FEE_RATE_BPS = 0n`, the US-equity
/// `markets.json`), which means "no fee" under both readings. So nothing is re-scaled here; a market
/// that wants Binance's fee states Binance's number, 125.
///
/// # Total
///
/// The notional is `calc_value`, saturated to `u64::MAX` where it would not fit (the cap binds long
/// before); the product is `u128`, saturating. No `Err` — this runs on the sweep path.
pub(crate) fn clearance_fee_due(market: &Market, mark_price: u64, closed_quantity: u64) -> u128 {
    let notional =
        calc_value(mark_price, closed_quantity, market.base_decimals, market.price_decimals)
            .unwrap_or(u64::MAX);
    (notional as u128).saturating_mul(market.liquidation_fee_rate_bps as u128) / 10_000
}

/// Charge the liquidation clearance fee on `closed_quantity` ([`clearance_fee_due`]) to `user`'s
/// perp wallet, at most `cap`, and credit it to the insurance fund. Returns the fee charged.
///
/// The CAP is the caller's, because it is the one thing the two margin types differ in (owner
/// decision Q3): an ISOLATED liquidation passes the close credit its close returned to the wallet
/// (J5 — see `liquidate_position`), a CROSS one `max(0, E_after)`. Either way a bankrupt
/// liquidation passes at most the ROUNDING DUST its close at the bankruptcy price leaves — that
/// price is rounded in the account's favour, so the close keeps a surplus of up to one price unit's
/// worth of the closed size (`≤ calc_value(1, |amount|) − 1` wherever `pd + bd ≤ 6`; the exact
/// bound is `perp_core::math::calc_bankruptcy_price`'s) — and pays `min(due, dust)` into the fund:
/// Binance's "no clearance fee for a bankrupt position" up to that dust (devnet: 33 and 99 on
/// 0.01 BTC, against 2_000_000 and 3_163_005 due).
///
/// TOTAL (J4) beyond the storage layer: the fee is further bounded so the wallet debit stays an
/// `i64` and the fund a `u64`, so neither `debit_perp` nor the fund credit can fail. A fee of `0`
/// writes nothing and emits nothing.
pub(crate) fn charge_clearance_fee<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    mark_price: u64,
    closed_quantity: u64,
    cap: i128,
) -> Result<u64, PerpError> {
    if closed_quantity == 0 || cap <= 0 || market.liquidation_fee_rate_bps == 0 {
        return Ok(0);
    }
    let old_if = storage::load_insurance_fund(context)?;
    let mut account = storage::load_account(context, user)?;
    // `W − fee ≥ i64::MIN` (a cross wallet may already be negative) and `IF + fee ≤ u64::MAX`.
    let wallet_room = (account.perp_wallet_balance as i128 - i64::MIN as i128) as u128;
    let fee = clearance_fee_due(market, mark_price, closed_quantity)
        .min(cap as u128)
        .min(wallet_room)
        .min(i64::MAX as u128)
        .min((u64::MAX - old_if) as u128) as u64;
    if fee == 0 {
        return Ok(0);
    }
    account.debit_perp(fee)?;
    // `InsuranceClear`: the counterparty of this wallet move IS the insurance fund — the clearance
    // fee is debited here and credited to the fund just below.
    storage::save_account(context, user, account, AccountUpdateReason::InsuranceClear)?;
    let new_if = old_if + fee;
    storage::save_insurance_fund(context, new_if)?;
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::InsuranceFundChanged {
            // `fee ≤ i64::MAX` by the bound above.
            delta: fee as i64,
            newBalance: new_if,
        }
        .to_log_data(),
    });
    Ok(fee)
}

/// `liquidate(address user, uint64 marketId)`
///
/// Anyone can call this to liquidate an under-margined position. Thin wrapper over
/// [`liquidate_position`]: decodes calldata, loads the market + mark price (refusing
/// a market with no oracle price), and maps the core outcome to the caller-facing
/// result. `caller` is recorded as the liquidator.
///
/// A position CROSSED in force is judged on its ACCOUNT instead (cross P3): the account-level
/// check, then — below cross maintenance — the liquidation episode starting at `marketId`, with a
/// fresh [`cross_liquidation::MANUAL_CROSS_STEP_BUDGET`] steps and [`ADL_BUDGET_PER_UPDATE`] ADL
/// fills (what it leaves becomes a continuation, like the sweep's). A healthy account is refused
/// before any write; a flagged one that has recovered is un-flagged, and that is the call's effect
/// — reachable through ANY market for a flagged account: one where it holds nothing, a CROSSED
/// one, or an ISOLATED one whose position is not itself liquidatable (P3 review — one that is, is
/// liquidated by the call like any isolated position, and the keeper calls again). A market whose
/// mark is stale is skipped here too (Q10: a keeper cannot force a close at a price the oracle has
/// stopped publishing); once it ticks again, this call closes it. Gas stays flat.
pub fn run_liquidate<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = liquidateCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("liquidate: invalid calldata"))?;

    let market = storage::load_market_ref(context, args.marketId)?
        .ok_or_else(|| perp_err("liquidate: unknown market"))?;
    let mark_price = storage::load_mark_price(context, args.marketId)?;
    if mark_price == 0 {
        // A market that never received an oracle price would make the
        // maintenance-margin gate degenerate (notional and threshold both 0),
        // so solvency would be judged on the sign of `v_quote + margin` alone.
        // Refuse to liquidate without a real mark price.
        return Err(perp_err("liquidate: mark price unavailable"));
    }

    // The account-level episode for a position CROSSED in force — and for a FLAGGED account with
    // no position here at all: an episode an earlier call left unfinished resumes (or, the account
    // having recovered, ends) from any market, so a keeper need not know which markets it holds.
    // (A flag cannot outlive the account's last cross position — the end of the call that closed
    // it ends the episode, `cross_liquidation::end_flag_without_cross_position` — so this route
    // is a resume, not the only way out.)
    let route_to_episode = {
        let pos = storage::load_position_ref(context, args.user, args.marketId)?;
        if pos.amount != 0 {
            crate::margin::margin_type::effective_margin_type(&pos)
                == crate::types::MarginType::Crossed
        } else {
            storage::load_account_ref(context, args.user)?.in_liquidation()
        }
    };
    if route_to_episode {
        return run_liquidate_episode(context, args.user, args.marketId, caller);
    }

    let mut adl_budget = ADL_BUDGET_PER_UPDATE;
    // The manual entrypoint resolves one position on demand; it is not a sweep and does not own
    // the market's drain bit, so its deferral signal is discarded. Anything it leaves behind is
    // picked up by the next sweep exactly as before.
    let mut deferred = false;
    match liquidate_position(
        context,
        args.user,
        args.marketId,
        &market,
        mark_price,
        caller,
        &mut adl_budget,
        &mut deferred,
    )? {
        LiquidationOutcome::Liquidated { .. } => Ok(Bytes::new()),
        LiquidationOutcome::NoPosition => Err(perp_err("liquidate: no open position")),
        // An ISOLATED position above its own maintenance: nothing written (`liquidate_position`
        // validates before it writes). For a FLAGGED account the call is the episode's instead —
        // a keeper told "any `marketId`" may well pick the market of a healthy isolated position
        // (P3 review) — and the episode judges the account for itself.
        LiquidationOutcome::AboveMaintenance => {
            if storage::load_account_ref(context, args.user)?.in_liquidation() {
                return run_liquidate_episode(context, args.user, args.marketId, caller);
            }
            Err(perp_err("liquidate: position is above maintenance margin"))
        }
    }
}

/// `liquidate()`'s cross route: the account-level episode for `user` starting at `market_id`, with
/// a fresh [`cross_liquidation::MANUAL_CROSS_STEP_BUDGET`] steps and [`ADL_BUDGET_PER_UPDATE`] ADL
/// fills. A healthy, unflagged account is refused before any write.
fn run_liquidate_episode<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    liquidator: Address,
) -> Result<Bytes, PerpError> {
    let mut budget = cross_liquidation::MANUAL_CROSS_STEP_BUDGET;
    let mut adl_budget = ADL_BUDGET_PER_UPDATE;
    // No sweep owns this call: the episode writes any continuation to its market's `deferred_work`
    // directly, so this stays false.
    let mut deferred = false;
    let caller = cross_liquidation::EpisodeCaller {
        sweep_market: None,
        fresh_market: None,
        liquidator,
    };
    match cross_liquidation::liquidate_cross_account(
        context,
        user,
        market_id,
        caller,
        &mut budget,
        &mut adl_budget,
        &mut deferred,
    )? {
        cross_liquidation::CrossLiquidationOutcome::Healthy => {
            Err(perp_err(LIQUIDATE_CROSS_HEALTHY_REJECT))
        }
        cross_liquidation::CrossLiquidationOutcome::Recovered
        | cross_liquidation::CrossLiquidationOutcome::Liquidated { .. } => Ok(Bytes::new()),
    }
}

/// `liquidate()`'s refusal for a position CROSSED in force whose ACCOUNT is at or above cross
/// maintenance (`crossMarginBalance ≥ totalCrossMaintMargin`) — nothing written.
pub(crate) const LIQUIDATE_CROSS_HEALTHY_REJECT: &str =
    "liquidate: account is above cross maintenance margin";

/// Max liquidation STEPS per `updateIndexPrice` sweep: one isolated liquidation, or one step of a
/// cross episode (one market's cancel-all, or one market's close — cross P3). This bounds the
/// expensive half — each step cancels or closes through the book + writes — but NOT the
/// registry scan itself: the loop still visits every registered candidate until it
/// has spent this many, so a sweep over N mostly-healthy positions is O(N)
/// (load + funding-settle + revert per healthy candidate). That O(N) scan on the
/// (delayed-execution) newPayload critical path is a known scale limit; the real fix
/// is the tick-bucket candidate index (auto-liq Phase E), which visits only
/// liquidatable candidates. Overflow of THIS cap defers to the next update: the
/// registry is re-scanned every update and un-liquidated candidates stay underwater
/// until the mark moves, so nothing is permanently missed.
pub(super) const MAX_SWEEP_ACTIONS_PER_UPDATE: u32 = 50;

/// Total ADL fills allowed across ONE `updateIndexPrice` (shared by every liquidation
/// in the sweep). ADL closes an insolvent book-unfillable residual as forced trades
/// against opposite-side holders (scheme X: no Insurance Fund on the residual path);
/// each fill writes two positions, so this bounds the per-tx work on the delayed-
/// execution critical path. A residual not fully closed within the budget stays open
/// and is re-swept next update — deferral is safe (no realized bad debt, the opposite
/// side's offsetting gains persist).
const ADL_BUDGET_PER_UPDATE: u32 = 128;

/// Protocol-automatic liquidation sweep, run synchronously at the tail of
/// [`run_update_index_price`] after the new mark + funding are persisted (so the
/// `Liquidation` events attach to the oracle's updateIndexPrice receipt). Scans
/// the market's open-position registry and liquidates every candidate below
/// maintenance at the new mark.
///
/// Each candidate is attempted under its own journal checkpoint:
/// - an ISOLATED position ([`liquidate_position`]): `Liquidated` → commit the writes; healthy /
///   stale (`AboveMaintenance` / `NoPosition`) → revert (the sweep leaves no trace);
/// - a position CROSSED in force: judged on its ACCOUNT (`E < Σ_C MM`, the total health check
///   `margin::view::cross_health` — the account, its cross-leg blob and the legs' markets, no
///   position walk, cross P4) by the cross episode (`cross_liquidation::liquidate_cross_account`),
///   which starts at this market and spends the same step budget — `Healthy` → revert, anything
///   else → commit. The registry is a SET, so an account is processed at most once per sweep,
///   however many of its markets an episode touches.
///
/// Any other error after the liquidatability check is a commit-only invariant failure and aborts
/// the node rather than allowing a partial write set.
fn run_liquidation_sweep<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &crate::types::Market,
    mark_price: u64,
    // Shared with the trigger sweep, which runs FIRST and eats from the same pot — see
    // `run_deferred_sweeps`. Being starved here is safe (a liquidation deferred is a liquidation
    // delayed); being starved there is a user's stop-loss not firing.
    budget: &mut u32,
    // This market's mark was published by THIS transaction (an oracle tick) — exempt from the
    // cross episode's stale-mark guard. `false` on a drain.
    fresh_mark: bool,
) -> Result<bool, PerpError> {
    // Snapshot the registry (owned Vec, deterministic insertion order). Liquidations
    // mutate the live registry via save_position, but iterating the snapshot is stable;
    // a candidate already closed by an earlier cascade in this sweep resolves to
    // NoPosition (skipped), and cascade-created candidates are picked up next update.
    let candidates = storage::load_position_registry(context, market_id)?;
    // One ADL-fill budget shared across every liquidation in this sweep (bounds per-tx
    // ADL work regardless of how many positions liquidate).
    let mut adl_budget = ADL_BUDGET_PER_UPDATE;
    // Set by any liquidation that stopped on a PER-CALL budget rather than on the market's state.
    // Today that is the distinct-maker cap inside the book leg; the shared ADL budget is read
    // directly below, since it lives here.
    let mut deferred = false;
    for user in candidates {
        if *budget == 0 {
            // Stopped on the cap with at least one candidate unexamined. Conservative: that
            // candidate may well be healthy, so this can report deferral where none exists — the
            // drain then finds nothing and clears the bit. Reporting a FALSE NEGATIVE here would be
            // the harmful direction, and the `break` cannot produce one.
            return Ok(true);
        }
        // ── A position CROSSED in force: the ACCOUNT is the candidate ──
        let crossed = {
            let pos = storage::load_position_ref(context, user, market_id)?;
            pos.amount != 0
                && crate::margin::margin_type::effective_margin_type(&pos)
                    == crate::types::MarginType::Crossed
        };
        // commit-only (#23): healthy candidates are write-free (funding is computed in memory
        // and only applied when liquidatable), so the checkpoint only balances EVM-side state.
        // A liquidation that FAILS mid-apply would leave partial perp writes with no undo — a
        // corruption-anyway condition: halt loudly rather than skip silently.
        let cp = context.checkpoint();
        let wrote = if crossed {
            let caller = cross_liquidation::EpisodeCaller {
                sweep_market: Some(market_id),
                fresh_market: fresh_mark.then_some(market_id),
                liquidator: Address::ZERO,
            };
            match cross_liquidation::liquidate_cross_account(
                context,
                user,
                market_id,
                caller,
                budget,
                &mut adl_budget,
                &mut deferred,
            ) {
                // Healthy: zero perp writes were made.
                Ok(cross_liquidation::CrossLiquidationOutcome::Healthy) => false,
                // The episode spent its own steps out of `budget`.
                Ok(_) => true,
                Err(e) => {
                    panic!(
                        "liquidation sweep: the cross liquidation episode failed mid-apply \
                         (commit-only invariant): {e:?}"
                    );
                }
            }
        } else {
            match liquidate_position(
                context,
                user,
                market_id,
                market,
                mark_price,
                Address::ZERO,
                &mut adl_budget,
                &mut deferred,
            ) {
                Ok(LiquidationOutcome::Liquidated { .. }) => {
                    *budget -= 1;
                    true
                }
                // AboveMaintenance / NoPosition: zero perp writes were made.
                Ok(_) => false,
                Err(e) => {
                    panic!(
                        "liquidation sweep: liquidate_position failed mid-apply (commit-only invariant): {e:?}"
                    );
                }
            }
        };
        if wrote {
            context.checkpoint_commit();
        } else {
            context.checkpoint_revert(cp);
        }
    }
    // Walked every candidate without hitting the cap.
    //
    // ⚠️ That is NOT "there is nothing liquidatable". This function reports CAP PRESSURE — work a
    // LATER ATTEMPT IN THE SAME BLOCK could finish because a budget, not the market, stopped it:
    //
    //   * the shared sweep budget            -> the early `return Ok(true)` above
    //   * the shared ADL fill budget         -> `adl_budget == 0` here
    //   * the per-match distinct-maker cap   -> `deferred`, set by the book leg
    //
    // All three reset on the next call, so `drainDeferredWork` genuinely makes progress on each.
    // What this does NOT report, deliberately, is a residual with no eligible counterparty: that
    // outcome is identical until the mark, the book or the position set changes, so flagging it
    // would spin a keeper's `while (drain())` forever for nothing.
    //
    // `adl_budget == 0` can be a false POSITIVE (the last fill may have been the last one needed);
    // the drain then finds nothing and clears the bit. A false NEGATIVE would be the harmful
    // direction and neither term can produce one.
    Ok(adl_budget == 0 || deferred)
}

/// The two capped sweeps, in order, plus the `Market::deferred_work` bookkeeping they share.
///
/// Both the oracle path and [`run_drain_deferred_work`] go through here, so the ORDER and the bit's
/// meaning are defined once. Returns whether work was left behind.
///
/// # The order is not arbitrary
///
/// The band GC runs FIRST. Every level it removes was already unmatchable to a liquidation close
/// banded on the same mark, so it cannot reduce what the close absorbs or flip a book-absorbed
/// close into an insurance-fund or ADL one. What it does change is the BBO — and running first is
/// what makes that safe: nothing has touched the book yet in this call, so the `best_bid`/`best_ask`
/// cache is provably live, which is the documented precondition of the cheap
/// [`crate::trading::remove_from_book_after_cancel`] variant. Running after the sweep would hand it
/// a cache rewritten by an arbitrary number of matches and mid-match auto-cancels.
///
/// That holds identically on the drain path, which is why the drain can reuse this wholesale: a
/// drain transaction has touched nothing either.
///
/// # The bit is written ONLY on transition
///
/// A no-op drain (bit already `0`, both sweeps clean) must cost one `Market` read and NOTHING else;
/// writing `0` over `0` would turn the cheap keeper poll into a per-call storage write. On the
/// oracle path the saving is nominal — `save_mark_price` re-emits the `Market` blob every update
/// anyway — but the rule belongs here rather than at one call site.
pub(super) fn run_deferred_sweeps<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &Market,
    // `IndexPriceState.timestamp` of `mark_price` — the trailing structure's tick sequence.
    seq: u64,
    mark_price: u64,
    // `mark_price` was published by THIS transaction (the oracle path), not re-read (the drain).
    fresh_mark: bool,
) -> Result<bool, PerpError> {
    // ── Trailing stops, step 1: fold the mark into the peaks — FIRST and NOT budgeted ─────────
    //
    // A peak the budget skipped would make a stop fire later than it should, and a skipped tick
    // can never be replayed, so this is the one leg that always runs in full. It is idempotent at
    // a fixed `(seq, mark)`, which is what lets the drain call this whole function again.
    crate::trading::algo::trailing::on_mark(context, market_id, seq, mark_price)?;

    let gc_deferred = run_out_of_band_expiry_sweep(context, market_id, market)?;

    // ── ONE budget, consumed in order: triggers, then liquidations ───────────────────────────
    //
    // Triggers go first, and not merely out of politeness to the user. A stop-loss that executes
    // REMOVES the position the liquidation sweep would otherwise have had to handle, so the two
    // are not competing for the same work — spending here reduces what is left to spend below.
    // The reverse order is strictly worse: a user whose stop did not fire BECAUSE we spent the
    // budget liquidating them is the one outcome nobody can defend.
    //
    // Sharing one counter rather than giving triggers their own cap keeps the per-tick gas bound
    // a single number, so the argument for it does not have to be re-derived.
    let mut budget = MAX_SWEEP_ACTIONS_PER_UPDATE;
    // ── A DEACTIVATED market converts no conditional order ───────────────────────────────────
    //
    // `updateMarket(active = false)` stops every new order, and a fired conditional IS a new order:
    // `place_order_core` would refuse it write-clean ("market not active"), and `fire` treats a
    // refusal as an OUTCOME — the order would be SPENT (`TriggerRejected`) although nothing traded,
    // so a pause would quietly destroy every stop-loss the next ticks crossed. So the three legs
    // that turn a conditional into something else — fixed fires, trailing fires, trailing
    // activations — wait for reactivation. The peaks (`on_mark` above) keep following the
    // published mark, so an activated trailing stop trails through the pause.
    //
    // A skipped leg reports NO deferral: its work is not budget-bound, a drain could do nothing
    // with it, and a set bit would spin a keeper's `while (drain())` for the whole pause. Nothing
    // is lost by that either — none of these legs is latched, so the first tick after
    // reactivation re-derives everything from its own mark (and an activation then starts its
    // peak at that mark), exactly as for work a budget left undrained.
    let (trigger_deferred, trailing_deferred, activation_deferred) = if market.active {
        let trigger_deferred =
            crate::trading::algo::run_trigger_sweep(
                context,
                market_id,
                market,
                mark_price,
                &mut budget,
            )?;
        // Trailing fires share the pot, after the fixed triggers (no price-priority merge between
        // the two: it would only matter on overflow, which the drain finishes at the same mark). A
        // SEPARATE call, because the fixed sweep returns early when the budget runs out.
        let trailing_deferred =
            crate::trading::algo::trailing::run_fire_sweep(
                context,
                market_id,
                market,
                mark_price,
                &mut budget,
            )?;
        // Activations on their OWN cap: a crowd of crossing activation prices must not spend the
        // budget stop-losses and liquidations need. AFTER the fire walk, and the order is
        // load-bearing: a fire in this tick can END a position — as the taker, or as the maker
        // another owner's fired order hits — that a pending trailing stop is bound to, and only
        // fires-then-activations sees that at activation and spends the stop as `PositionClosed`
        // instead of activating it on a position that no longer exists. (L4 — a new activation
        // cannot fire on its own tick — covers only the other direction: nothing activated here
        // could have fired in the walk above.) Pinned by `kill_t19_*` in the trailing spec suite.
        let mut activation_cap =
            crate::trading::algo::trailing::MAX_TRAILING_ACTIVATIONS_PER_UPDATE;
        let activation_deferred = crate::trading::algo::trailing::run_activation_sweep(
            context,
            market_id,
            market,
            seq,
            mark_price,
            &mut activation_cap,
        )?;
        (trigger_deferred, trailing_deferred, activation_deferred)
    } else {
        (false, false, false)
    };
    let sweep_deferred =
        run_liquidation_sweep(context, market_id, market, mark_price, &mut budget, fresh_mark)?;
    let deferred = gc_deferred
        || trigger_deferred
        || trailing_deferred
        || activation_deferred
        || sweep_deferred;

    if (market.deferred_work != 0) != deferred {
        storage::save_deferred_work(context, market_id, deferred)?;
    }
    Ok(deferred)
}
