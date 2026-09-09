use alloy_primitives::IntoLogData;
use crate::host::PerpHost;
use primitives::{Address, Log};

use super::execute_order_cancellation;
use crate::{
        errors::{perp_err, perp_invariant_err},
    interface::IPerpDex,
    math::{
        calc_maker_fee_for_order_qty_with_bps, calc_trading_fee,
        calc_value, calc_value_i64, checked_u64_to_i64, is_above_maintenance_margin,
        max_leverage_for_notional,
    },
    storage,
    types::{AccountUpdateReason, OrderStatus, Side},
    PERP_DEX_ADDRESS,
    PerpError,
};

// ── Position settlement ───────────────────────────────────────────────────────

/// Accumulates fill data for a taker across multiple partial fills, then
/// commits the net effect to storage in a single [`finalize`] call.
///
/// A single taker order may be matched against several maker levels; each
/// level produces one `record_fill` call. Batching lets us charge one fee,
/// perform one margin check, and emit one `PositionChanged` log for the
/// entire order, regardless of how many makers filled it.
///
/// Lifecycle: [`load`] → one or more [`record_fill`] → [`finalize`].
pub(super) struct TakerSettlement {
    user: Address,
    market_id: u64,
    fills: Vec<RecordedFill>,
    taker_fee_bps: u64,
}

struct RecordedFill {
    price: u64,
    quantity: u64,
    taker_side: Side,
    /// OUTPUT, not input: this fill's share of the taker's close PnL, written by
    /// [`finalize_core`]'s apportionment and read by
    /// [`MatchRegistry::backfill_taker_realized_pnl`] to fill in `Trade.takerRealizedPnl`.
    ///
    /// It lives on the recorded fill rather than in a returned `Vec<i64>` so the apportionment
    /// costs ZERO extra allocation — this vector already exists, one slot per fill.
    ///
    /// `0` until `finalize_core` runs, and legitimately `0` afterwards for any fill whose leg was
    /// purely opening (see the apportionment note).
    realized_pnl: i64,
}

impl TakerSettlement {
    /// Snapshots the taker's pre-trade position and account into the
    /// accumulator.  `remaining_closing_qty` is initialised from the current
    /// position size so that the first `record_fill` sees the full closing
    /// capacity.
    pub(super) fn load<H: PerpHost>(
        context: &mut H,
        user: Address,
        market_id: u64,
        waive_taker_fee: bool,
    ) -> Result<Self, PerpError> {
        let rates = storage::load_user_fee_rates(context, user)?;
        Ok(Self {
            user,
            market_id,
            fills: Vec::new(),
            // A forced liquidation close pays no taker trading fee — the liquidated
            // user is already charged the liquidation clearance fee (to the IF). This
            // also keeps the close from reverting on `ensure_taker_wallet_can_cover_margin`
            // when the underwater user has no free wallet to cover a taker fee.
            taker_fee_bps: if waive_taker_fee {
                0
            } else {
                rates.taker_fee_bps
            },
        })
    }

    pub(super) fn taker_fee_bps(&self) -> u64 {
        self.taker_fee_bps
    }

    /// Accumulates one partial fill into the running closing/opening totals.
    ///
    /// The matcher may call this multiple times for a single taker order (one
    /// call per maker level that gets consumed). State is only committed to
    /// storage once [`finalize`] is called, so `self.pos` is intentionally
    /// **not** mutated here — that is also why closing capacity is tracked via
    /// `remaining_closing_qty` rather than re-reading `pos.amount` on each
    /// call (which would still reflect the pre-trade position for every fill).
    ///
    /// # Closing vs opening split
    ///
    /// A fill reduces an existing opposite-side position first (closing), then
    /// any remainder opens a new position (opening). The two legs have
    /// different PnL treatment in `finalize`:
    /// - closing leg → realises PnL against entry price (`v_quote_balance`)
    /// - opening leg → locks fresh margin at the current leverage
    ///
    /// Notional values (not just quantities) are accumulated here because fee
    /// calculation uses the total traded notional across all partial fills.
    pub(super) fn record_fill(
        &mut self,
        fill_price: u64,
        fill_qty: u64,
        taker_side: Side,
        _market: &crate::types::Market,
    ) -> Result<(), PerpError> {
        self.fills.push(RecordedFill {
            price: fill_price,
            quantity: fill_qty,
            taker_side,
            // Apportioned later, in `finalize_core` — nothing here knows the entry basis yet.
            realized_pnl: 0,
        });
        Ok(())
    }

    /// Commits all accumulated fills to storage in a single atomic sequence.
    ///
    /// # Settlement order
    ///
    /// 1. **Apply position fill** — mutates `pos` and credits realised PnL from
    ///    the closing leg to the wallet.
    /// 2. **Compute fee** — based solely on traded notional, charged out of the margin the fill
    ///    funds (`fee_from_margin = min(fee, opening_margin)`), remainder from the wallet.
    /// 3. **Save** — persists all position and PnL changes in one write so that
    ///    the next step reads the correct wallet balance from storage.
    /// 4. **Ensure the AVAILABLE balance covers opening margin + fee** — if short, same-side open
    ///    orders are auto-cancelled (LIFO). Cancelling frees no cash (nothing is escrowed); it
    ///    lowers `Bid`/`Ask` and so lowers `Σ ooIM`, which raises the available.
    /// 5. **Deduct opening margin and fee from wallet** — both deducted cleanly
    ///    from the wallet; position margin is never touched for fee payment.
    /// 6. **Emit log** — single `PositionChanged` event for the full order.
    ///
    /// The old step 2, "recompute `margin_reserved`", is gone with the escrow: the fill's effect
    /// on the taker's remaining open-order requirement is picked up on the next `ooIM` evaluation
    /// from the new `pos.amount`, with no stored field to reconcile.
    ///
    /// # Why the taker leg carries NO `Last × 1.0015` markup
    ///
    /// Binance gates a MARKET order at `Last Price × 1.0015` on both sides, but escrows nothing
    /// against it: the measured debit is `Ne / L` at the FILL price, digit for digit, so the markup
    /// 「过闸即消失」 and its only function is a SLIPPAGE ALLOWANCE
    /// (`misc/binance-flip-and-admission.md` §3.8). It exists because their admission runs before
    /// the match, against an unknown fill price.
    ///
    /// Ours does not. Every taker path — market, IOC, FOK, and a GTC's crossing portion — matches
    /// FIRST and gates SECOND, on `core.total_required`, which is the realised draw at the realised
    /// fill prices, inside the same call. There is no window between the gate and the fill for
    /// slippage to open, so a 15 bps allowance on top of a number we already know exactly would only
    /// refuse orders we can see are affordable. It is deliberately NOT implemented; the markup lives
    /// solely on the RESTING side, where the fill price genuinely is unknown at admission
    /// (`margin_view::assuming_price_floor`). Market/IOC/FOK remainders never rest
    /// (`rest_remainder = false`), so they contribute nothing to `Bid`/`Ask` either.
    ///
    /// **Re-verified path by path (2026-08-18), because "matches first" is the whole argument and a
    /// single pre-match affordability decision would break it.** The walk itself performs ZERO
    /// storage writes (`trading::mod.rs`, the "commit-only #23 L2b" note after the match loop), so
    /// gating after it is free of the leak that would otherwise force an earlier gate:
    ///
    /// ```text
    /// path                pre-match gates                             taker affordability gate
    /// Market              validate (qty/tick) + market-index count cap  finalize_compute, realised
    /// IOC                 same                                          finalize_compute, realised
    /// FOK                 same + check_fok_feasibility                   finalize_compute, realised
    /// GTC (crossing)      same                                          finalize_compute, realised
    /// GTC (remainder)     —                                             rest_delta, LIMIT price
    /// PostOnly            cross test against the BBO                    no taker portion exists
    /// ```
    ///
    /// `check_fok_feasibility` is the one that looks like a counterexample and is not: it walks the
    /// book before matching, but its accumulator is a `u64` of fillable BASE QUANTITY and it reads no
    /// account, no position and no wallet — the limit price appears only as a bound on which levels
    /// count. It answers "can this be filled in full", never "can this be afforded". So no path
    /// decides affordability before the fill prices are known, and there is nothing here for a
    /// `Last × 1.0015` slippage allowance to protect. **Do not add one**: it would refuse orders
    /// whose exact cost is already in hand, and on Binance the uplift is not even escrowed on this
    /// path (§3.8 「过闸即消失」).
    pub(super) fn finalize_compute<H: PerpHost>(
        mut self,
        context: &mut H,
        reg: &mut MatchRegistry,
        taker_side: Side,
        market: &crate::types::Market,
    ) -> Result<Option<TakerPlan>, PerpError> {
        // ── An EMPTY fill set has nothing to gate, and no longer pre-validates the REST ─────────
        //
        // It used to. `match_order` flushed immediately after this call and the walk records writes
        // even when nothing filled (a `SaveLevel` for every level it entered, plus the
        // `DeleteOrder`/`OrderCancelled`/`RemovePrice`/`SaveBest` of a maker the K9 guard rejected),
        // so a rest that `rest_in_book` would later refuse HAD to be refused here — with
        // `rest_is_affordable`, which was written to be `rest_in_book`'s own formula on the same
        // state, precisely so it could not diverge.
        //
        // It diverged anyway (val0 block 1,098,719), and the reason is instructive: the two were
        // algebraically identical and differed in ONE INPUT, because `save_last_traded_price` ran
        // between them and moved the Assuming-Price floor `T`. **An exact second evaluation is the
        // bug, not the fix** — it can only ever be as correct as the state it reads, and there is no
        // way to keep two read points in step by inspection.
        //
        // So it is gone, and with it `RestReq`, `with_rest_entry` and `rest_is_affordable`. The
        // caller no longer flushes after this call: it hands the unwritten `MatchOutcome` to
        // `rest_in_book`, which decides ONCE, above the barrier. There is nothing here for a second
        // opinion to be right or wrong about.
        if self.fills.is_empty() {
            return Ok(None);
        }

        // The taker joins the registry: funding computed on the copy (event pushed after all
        // maker events — the same stream position finalize applied it at), and on a self-match
        // the fills' maker-side effects are already in these copies.
        let i = reg.get_or_load(context, self.user, self.market_id, market)?;
        let mark = market.mark_price; // field of the threaded Market — no storage read
        let w = &mut reg.users[i].1;

        let core = finalize_core(
            &mut w.pos,
            &mut w.account,
            &mut self.fills,
            taker_side,
            mark,
            self.taker_fee_bps,
            market,
        )?;

        // `finalize_core` has apportioned each fill's share of the taker's close PnL; stamp it onto
        // the `Trade` rows the walk queued with a `0` placeholder. Done HERE, at the first point
        // where both the apportionment and the event queue are in hand, and still comfortably
        // before `apply` replays the queue. `w` is re-borrowed below because this needs `reg`
        // whole.
        reg.backfill_taker_realized_pnl(&self.fills)?;
        let w = &mut reg.users[i].1;

        // ── Derived-ooIM gate for the FILLS, and the basis the REST will be decided against ──
        // `core.total_required` (real cash — the opening margin plus the part of the fee margin
        // could not absorb) must come out of the available balance. Nothing is escrowed.
        //
        // Everything is measured on the POST-FILL state the flush is about to write:
        // `after_fills` is `w.pos` with `Bid`/`Ask` taken from the working lists the walk has
        // already consumed entries from, and the wallet is `w.account`'s, which already carries
        // this fill's close proceeds. That is exactly what `finalize_apply` will re-derive from
        // storage after the flush, so the pre-flush decision and the post-flush one agree.
        //
        // ── The REST is NOT gated here any more (atomic match+rest) ───────────────────────────
        // This function used to take a `RestReq` and add the remainder's marginal `ooIM` to `need`,
        // because `rest_in_book` ran AFTER the flush and its own refusal would therefore have
        // leaked the fills (val0 block 1,098,719). It no longer does: `match_order` returns the
        // UNFLUSHED registry and this plan, and the caller lets `rest_in_book` DECIDE before the
        // write barrier. So there is exactly ONE evaluation of the rest's affordability, which is
        // the property the val0 leak was the absence of — two algebraically identical checks
        // diverged because `save_last_traded_price` ran between them and moved the Assuming-Price
        // floor `T`. What this function owes the rest decision is not a second opinion but the
        // STATE to decide on: [`RestBasis`], the taker's position + wallet exactly as the flush and
        // `finalize_apply` will leave them.
        let (bd, pd) = (market.base_decimals, market.price_decimals);
        // `Bid`/`Ask` come off the WORKING order lists, which are the authoritative record of what
        // the walk has consumed — each surviving entry still at its own frozen assuming price.
        let after_fills = work_position_snapshot(w, bd, pd)?;
        let wallet = w.account.perp_wallet_balance;
        let available = crate::margin_view::derived_available_balance_with(
            context,
            self.user,
            Some(wallet),
            Some((self.market_id, &after_fills)),
        )?;

        // LEVEL 1 fast path (the common case): the available already covers the fills with NO
        // cover cancels → produce the plan with ZERO order-list clones. Only a genuinely tight
        // taker falls through.
        let (rest_pos, cover_cancelled) =
            if crate::margin_view::derived_can_afford(available, core.total_required as i128) {
                (after_fills, false)
            } else {
                // Cover needed (rare): simulate the LIFO same-side cancels on clones, reusing
                // `release_open_order_margin_core` so the sim cannot diverge from `finalize_apply`'s
                // real loop. A cancel frees no cash now — it lowers `Bid`/`Ask` and therefore
                // `Σ ooIM`, which is what raises the available. The loop's own test is
                // `available >= total_required`, a straight money-out amount and NOT a delta, so
                // `rest_in_book`'s post-state restatement does not apply to it — `available >=
                // amount` is already the right form (the same class as `transferFromPerp` /
                // `addPositionMargin`).
                //
                // ⚠️ The loop's break condition covers `total_required` ONLY, and always did: it
                // has never cancelled orders to make room for a REST (whenever the fills alone were
                // affordable it broke on the first iteration with zero cancels). So dropping the
                // rest term from the fast-path test above changes WHICH branch a fills-affordable /
                // rest-unaffordable taker takes, and nothing about how many of its orders are
                // cancelled — that taker now reaches `rest_in_book`, which refuses, and the whole
                // order rejects cleanly pre-flush instead of rejecting from in here.
                let mut sim_pos = after_fills.clone();
                let mut sim_buy = w.buy_entries.clone();
                let mut sim_sell = w.sell_entries.clone();
                let mut cancelled_any = false;
                loop {
                    // No re-fold: `release_open_order_margin_core` below subtracts each cancelled
                    // entry's frozen term from `sim_pos`, so its aggregates ARE `Bid`/`Ask` for the
                    // simulated book.
                    let avail = crate::margin_view::derived_available_balance_with(
                        context,
                        self.user,
                        Some(wallet),
                        Some((self.market_id, &sim_pos)),
                    )?;
                    if crate::margin_view::derived_can_afford(avail, core.total_required as i128) {
                        break;
                    }
                    let next = match taker_side {
                        Side::Buy => sim_buy.back().map(|e| e.order_id),
                        Side::Sell => sim_sell.back().map(|e| e.order_id),
                    };
                    let Some(oid) = next else {
                        return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
                    };
                    super::release_open_order_margin_core(
                        &mut sim_pos,
                        &mut sim_buy,
                        &mut sim_sell,
                        taker_side,
                        &oid,
                        market,
                    )?;
                    cancelled_any = true;
                }
                (sim_pos, cancelled_any)
            };

        // The wallet `finalize_apply`'s debit will leave. Cannot underflow: both branches above
        // exit with `derived_can_afford(available, total_required)` true, and
        // `available = wallet − Σ ooIM ≤ wallet` because `Σ ooIM ≥ 0`.
        let rest_wallet = wallet
            .checked_sub(checked_u64_to_i64(
                core.total_required,
                "settlement: taker total required",
            )?)
            .ok_or_else(|| perp_err("perp wallet: balance underflow"))?;

        let pos_log = w.pos.clone();
        reg.push_event(MatchEvent::AbsorbBadDebt {
            market_id: self.market_id,
            amount: core.bad_debt,
        });

        Ok(Some(TakerPlan {
            user: self.user,
            market_id: self.market_id,
            fee: core.fee,
            total_required: core.total_required,
            pos_log,
            realized_pnl: core.realized_pnl,
            closed_quantity: core.closed_quantity,
            rest_basis: RestBasis {
                pos: rest_pos,
                wallet: rest_wallet,
                cover_cancelled,
            },
        }))
    }
}

/// The taker's state **as the registry flush and [`finalize_apply`] will leave it** — the basis the
/// GTC remainder's rest decision is taken against, handed forward instead of re-derived.
///
/// # Why this exists
///
/// `rest_in_book` used to run after the flush and read the settled store. That is what made the
/// val0 leak at block 1,098,719 possible: its refusal fired with the fills already committed and no
/// perp undo to roll them back. Now the flush is deferred until after the rest decision, so
/// `rest_in_book` runs BEFORE the store holds any of this — and it must still measure the same
/// numbers. They live here.
///
/// Every field is the *post* value, not a delta:
/// * `pos` — `w.pos` with `Bid`/`Ask` resynced from the working order lists
///   ([`work_position_snapshot`]), then with the cover loop's simulated cancels applied. The flush
///   writes those same working copies and `finalize_apply` performs those same cancels, so this IS
///   the position the store will hold.
/// * `wallet` — post-fill (close proceeds already credited by `finalize_core`) and post-debit
///   (`− total_required`, which `finalize_apply` takes).
/// * `cover_cancelled` — whether the cover loop will really cancel at least one of the taker's own
///   SAME-SIDE resting orders. The rest decision needs it because such a cancel can lower the
///   taker's own side of the BBO, and `rest_in_book`'s new-best band gate reads the BBO pre-flush.
///   See the `may_become_best` note there.
#[derive(Clone)]
pub(super) struct RestBasis {
    pub(super) pos: crate::types::PerpPosition,
    pub(super) wallet: i64,
    pub(super) cover_cancelled: bool,
}

/// The registry working copy's position AS THE FLUSH WILL WRITE IT: `w.pos` with the per-side
/// aggregates resynced from the working order lists, which are the authoritative record of what
/// the match walk has consumed so far.
///
/// The aggregates ARE `Bid`/`Ask`, and `Bid`/`Ask` are inputs to `ooIM`, so any gate evaluated
/// mid-match has to see the walk's effect on them. `settle_maker_fill_core` and
/// `release_open_order_margin_core` maintain them incrementally as they mutate the lists; this recomputes
/// from the lists so a gate can never be decided on a stale aggregate, and the flush asserts the
/// two agree. The recompute values each surviving entry at its own FROZEN `assuming_price`
/// (`math::sum_side_totals`), so it reproduces the incremental result exactly — a partially filled
/// sell keeps the basis it was placed at.
fn work_position_snapshot(
    w: &UserWork,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<crate::types::PerpPosition, PerpError> {
    let mut pos = w.pos.clone();
    let (tbq, tbn) =
        crate::math::sum_side_totals(w.buy_entries.iter().copied(), base_decimals, price_decimals)?;
    let (tsq, tsn) = crate::math::sum_side_totals(
        w.sell_entries.iter().copied(),
        base_decimals,
        price_decimals,
    )?;
    pos.total_buy_qty = tbq;
    pos.total_buy_notional = tbn;
    pos.total_sell_qty = tsq;
    pos.total_sell_notional = tsn;
    Ok(pos)
}

/// The taker settlement's APPLY half (commit-only #23 L2b): everything after the decision —
/// the wallet-cover cancel loop (guaranteed to suffice by the compute simulation; its final
/// check is now an unreachable invariant), the margin+fee debit, the fee credit, and the
/// taker PositionChanged log. The taker's pos/account fill effects are written by the registry
/// flush; the cancels/debit below re-load and update storage exactly as before.
pub(super) struct TakerPlan {
    user: Address,
    market_id: u64,
    fee: u64,
    total_required: u64,
    pos_log: crate::types::PerpPosition,
    realized_pnl: i64,
    closed_quantity: u64,
    /// The state this plan's APPLY will leave behind, for the rest decision that runs BEFORE it.
    /// See [`RestBasis`].
    rest_basis: RestBasis,
}

impl TakerPlan {
    /// The post-fill / post-cover / post-debit basis for a GTC remainder's rest decision.
    pub(super) fn rest_basis(&self) -> RestBasis {
        self.rest_basis.clone()
    }
}

pub(super) fn finalize_apply<H: PerpHost>(
    context: &mut H,
    plan: TakerPlan,
    taker_side: Side,
    market: &crate::types::Market,
) -> Result<(), PerpError> {
    ensure_taker_wallet_can_cover_margin(
        context,
        plan.user,
        plan.market_id,
        taker_side,
        plan.total_required,
        market,
    )?;

    // Debit the taker wallet in place (no UserAccount/String load+save clone pair). pos is already
    // correct in storage (registry flush); cancels saved their own pos updates and the log fields
    // are untouched by cancellations.
    // `Order`: the taker's margin + fee debit for the order that just matched.
    storage::mutate_account_balance(context, plan.user, AccountUpdateReason::Order, |a| {
        a.debit_perp(plan.total_required)
    })??;
    credit_fee_recipient(context, plan.market_id, plan.fee)?;

    // ── ONE account snapshot for the TAKER, for this whole ORDER — the group HEADER ─────────────
    //
    // Off the SETTLED store, not off a working copy: the two lines above are the last of the
    // taker's state (the cover cancels, the margin+fee debit), so unlike a maker mid-sweep the
    // taker's account really is final here. It is emitted immediately BEFORE the taker's own
    // `PositionChanged`, which is what makes the two an `ACCOUNT_UPDATE` group (`crate::events`);
    // it used to be published by `match_order` AFTER this log, which orphaned that row.
    //
    // A LIQUIDATION CLOSE is NOT excluded. It used to be — `match_order` skipped the emit for a
    // liquidated user on the grounds that the close is one leg of a liquidation that goes on to
    // charge a clearance fee and possibly settle a residual or run ADL. Under the adjacency rule
    // that skip ORPHANS the liquidated user's `PositionChanged`, which is strictly worse than
    // publishing an intermediate: an indexer would attach that position to whatever group preceded
    // it. A liquidation is legitimately SEVERAL `ACCOUNT_UPDATE` pushes (close, clearance fee, ADL),
    // not one settled end-state, and Binance behaves the same way. The later legs write the account
    // again, re-mark the user, and the end-of-call drain publishes the settled end state as a
    // 0-position group.
    //
    // Clearing the mark unconditionally is sound for the same reason as before: nothing further in
    // the call writes this user's account *for this order* (the placement path's remaining writes
    // are `rest_in_book`'s `save_position_reservation_only` and the nonce's `mutate_account`,
    // neither of which marks). A later batch item, or a later liquidation leg, DOES write, re-marks,
    // and is published again.
    storage::publish_account_snapshot_now(context, plan.user, AccountUpdateReason::Order)?;

    // `market` is the same threaded `Market` whose `mark_price` `finalize_compute` fed to
    // `finalize_core` (`let mark = market.mark_price`), so the log values the taker's post-fill
    // position at exactly the mark this settlement was decided against.
    crate::events::emit_position_changed(
        context,
        plan.user,
        market,
        &plan.pos_log,
        plan.realized_pnl,
        plan.closed_quantity,
    )?;

    Ok(())
}

/// Outcome of attempting to settle one maker fill. A maker's trading fee is charged from the
/// margin the fill funds (`min(fee, opening_margin)`), the remainder from the wallet — the same
/// rule the taker path uses. There is no fee escrow.
pub(super) enum MakerFillOutcome {
    /// The fill was applied; carries the maker's trading fee plus the `PositionChanged` payload for
    /// this fill.
    ///
    /// ⚠️ The log payload travels back out to the WALK rather than being pushed from inside
    /// [`settle_maker_fill_registry`], and that is the whole point of it being here. The maker's
    /// account header has to sit IMMEDIATELY before its position row (`crate::events`), and the
    /// header is pushed by the walk (`MatchRegistry::push_maker_snapshot`) after the fill's `Trade`
    /// row — so the position row has to be pushed after that, by the same caller. Pushed from in
    /// here it would land BEFORE the `Trade`, one event too early, and orphan itself.
    Filled {
        maker_fee: u64,
        /// The maker's position AFTER this fill (all-scalar clone).
        pos_snapshot: crate::types::PerpPosition,
        realized_pnl: i64,
        closed_quantity: u64,
    },
    /// The fill was NOT applied and the caller must cancel the maker order. TWO triggers, both
    /// meaning "this maker cannot take this fill":
    ///
    /// 1. it would open/increase the position below the maintenance-margin threshold at the
    ///    current mark (K9);
    /// 2. it would open at a leverage the resulting size's margin tier no longer permits.
    ///
    /// A wallet SHORTFALL is deliberately NOT in this list: the fill happens and the wallet goes
    /// negative (Binance never sweeps an under-covered lien — see the long note in
    /// [`settle_maker_fill_core`]). This channel is for fills that would leave an INSOLVENT
    /// POSITION, which is a different question from an under-funded wallet.
    ///
    /// Funding accrued on the maker's position IS settled and persisted regardless (it is owed
    /// whether or not the fill happens, and may already have touched the Insurance Fund inline).
    RejectedInsolvent,
}

// ── Match working-copy registry (commit-only #23, tranche-4 L1) ─────────────────
// Per-user working copies for one match: each touched user (maker, and taker on self-match) is
// loaded ONCE (with funding computed+applied at first touch, exactly where today's first
// settle_maker_fill did it) and saved ONCE at flush — same write-key set and net values as
// today's per-fill saves, with per-list dirty flags so an untouched list never enters the delta.
// Vec-backed (few users per match) for deterministic flush order.

pub(super) struct UserWork {
    pos: crate::types::PerpPosition,
    account: crate::types::UserAccount,
    buy_entries: std::collections::VecDeque<crate::types::OrderEntry>,
    sell_entries: std::collections::VecDeque<crate::types::OrderEntry>,
    dirty_buy: bool,
    dirty_sell: bool,
    /// `Σ pos.margin` over this user's OTHER markets — the part of the stored
    /// [`crate::types::UserAccount::total_position_margin`] this match provably cannot move (one
    /// match settles exactly one market).
    ///
    /// # Why it is here, and why it is a DIFFERENCE rather than the two terms
    ///
    /// The maker's authoritative state during a sweep is this working copy, not storage — the walk
    /// performs zero storage writes and the flush writes these copies. So a per-fill account
    /// snapshot has to be derived from the copy, and `totalWalletBalance = cw + Σ_all_markets
    /// pos.margin` needs a Σ the copy does not carry: `account.total_position_margin` is a
    /// whole-account aggregate maintained exclusively by `storage::save_position`, and
    /// `storage::save_account` deliberately CLOBBERS it from the store, so the field on an owned
    /// copy is not a value anything may read.
    ///
    /// [`MatchRegistry::get_or_load`] holds both terms it needs in the same breath — the stored
    /// aggregate on the freshly loaded account, and this market's stored `pos.margin` on the
    /// freshly loaded position, BEFORE funding is folded into the latter. Their difference is
    /// invariant for the whole match, so capturing it once turns every later snapshot into
    /// `base + w.pos.margin` with no load and, crucially, with no dependence on *when* storage is
    /// read (the naive form, "read the stored aggregate at fill time", is only correct while the
    /// store is still pre-match — a property of the current walk rather than of the formula).
    ///
    /// Pre-funding is the right anchor precisely because the stored aggregate is pre-funding too:
    /// `funding::compute_funding_settlement` moves `pos.margin` in MEMORY and
    /// `apply_funding_settlement` writes only the insurance fund and its logs, so the stored
    /// position — and therefore the stored Σ — still holds the pre-funding margin right up to the
    /// flush's `save_position`.
    ///
    /// Verified rather than argued: [`MatchRegistry::flush`] cross-checks `base + w.pos.margin`
    /// against the settled store on every flushed user in `debug_assertions` builds.
    other_market_position_margin: i64,
    /// The payload of the last account snapshot published for this user during the match, if any.
    /// Kept so the flush can decide whether the end-of-call drain still owes this user an event —
    /// see the "duplicate mark" note in [`MatchRegistry::flush`].
    last_published: Option<crate::margin_view::AccountWalletBalances>,
}

impl UserWork {
    /// The three balances `AccountBalanceChanged` publishes, **as the flush will write them** —
    /// the working-copy analogue of [`crate::margin_view::index_account_wallet_balances`].
    ///
    /// `cw` and `usdcBalance` are read straight off the working account (the flush's `save_account`
    /// writes exactly this struct). `wb = cw + Σ_all_markets pos.margin` reconstructs the Σ from
    /// [`Self::other_market_position_margin`] plus this market's live working `pos.margin`, which is
    /// the value the flush's `save_position` is going to store. The narrowing guard is the same one
    /// (and the only one) the settled producer has.
    fn wallet_balances(&self) -> Result<crate::margin_view::AccountWalletBalances, PerpError> {
        crate::margin_view::wallet_balances_from_parts(
            &self.account,
            self.other_market_position_margin,
            self.pos.margin,
        )
    }
}

/// One deferred side effect of the match walk (commit-only #23, L2a). The walk pushes these in
/// the EXACT sequence the old code performed them; [`MatchRegistry::flush`] replays them in order,
/// so the log stream and the insurance-fund/trade-counter evolutions are byte-identical.
pub(super) enum MatchEvent {
    ApplyFunding(crate::funding::PendingFunding),
    AbsorbBadDebt {
        market_id: u64,
        amount: u64,
    },
    FeeCredit {
        market_id: u64,
        amount: u64,
    },
    PositionChanged {
        user: Address,
        pos: crate::types::PerpPosition,
        realized_pnl: i64,
        closed_quantity: u64,
    },
    SaveOrder {
        order_id: [u8; 32],
        order: crate::types::Order,
    },
    /// delete-on-terminal: drop a maker order that reached a terminal status (fully filled during
    /// the walk, or cancelled by the K9 insolvency reject) from the order map.
    DeleteOrder {
        order_id: [u8; 32],
    },
    OrderCancelled {
        user: Address,
        order_id: [u8; 32],
        /// Attribution stamped on the log — see `crate::types::CancelReason`.
        reason: crate::types::CancelReason,
    },
    Trade {
        market_id: u64,
        taker_order_id: [u8; 32],
        maker_order_id: [u8; 32],
        taker: Address,
        maker: Address,
        price: u64,
        quantity: u64,
        taker_side: Side,
        taker_fee: u64,
        maker_fee: u64,
        /// This fill's share of the TAKER's close PnL (`Trade.takerRealizedPnl`).
        ///
        /// ⚠️ Pushed as a PLACEHOLDER `0` by the walk and backfilled later by
        /// [`MatchRegistry::backfill_taker_realized_pnl`]. It cannot be known at push time: the
        /// taker settles ONCE for the whole order, in `finalize_core`, after the walk has ended.
        /// The maker's is known immediately (it settles per fill) and is filled in at push time.
        taker_realized_pnl: i64,
        /// This fill's MAKER close PnL (`Trade.makerRealizedPnl`) — verbatim from this fill's own
        /// `settle_maker_fill_registry`, not apportioned.
        maker_realized_pnl: i64,
    },
    /// Writes a level blob: its FIFO `queue` (ids) AND the post-walk LIVE `count` (Obs-1 merge —
    /// count lives in the level blob, so one event, not a separate SaveCount). `count == 0` deletes
    /// the level.
    SaveLevel {
        is_bid: bool,
        price: u64,
        queue: Vec<[u8; 32]>,
        count: u64,
    },
    RemovePrice {
        is_bid: bool,
        price: u64,
    },
    SaveBest {
        is_bid: bool,
        price: u64,
    },
    MidPriceSample {
        best_bid: u64,
        best_ask: u64,
    },
    /// One `AccountBalanceChanged` for a MAKER, at its own fill.
    ///
    /// The payload is carried rather than looked up: the maker's state at fill time exists only in
    /// the `UserWork` copy (see [`UserWork::wallet_balances`]), and by the time this replays the
    /// store has still not been written. The event exists purely to put the log at the right
    /// STREAM POSITION — immediately after the `Trade` row for that fill, so the snapshot closes
    /// the maker's rows for that fill instead of floating to the end of the call.
    AccountSnapshot {
        user: Address,
        balances: crate::margin_view::AccountWalletBalances,
        /// Carried alongside the payload for the same reason: the reason belongs to the PUBLISH
        /// SITE, and the publish site is the walk (`push_maker_snapshot`), not this replay.
        reason: AccountUpdateReason,
    },
}

pub(super) struct MatchRegistry {
    users: Vec<(Address, UserWork)>,
    events: Vec<MatchEvent>,
    /// Fee recipient seen during the walk + credits not yet materialised into a working copy
    /// (see [`Self::credit_admin`]).
    fee_admin: Option<Address>,
    admin_credit_pending: u64,
}

impl MatchRegistry {
    pub(super) fn new() -> Self {
        Self {
            users: Vec::new(),
            events: Vec::new(),
            fee_admin: None,
            admin_credit_pending: 0,
        }
    }

    pub(super) fn push_event(&mut self, e: MatchEvent) {
        self.events.push(e);
    }

    /// Stamps the apportioned `Trade.takerRealizedPnl` onto the `Trade` events the walk already
    /// queued with a `0` placeholder.
    ///
    /// The walk cannot fill it in: the taker settles once, in [`finalize_core`], after the last
    /// fill. So the events are pushed incomplete and completed here, which is safe because NOTHING
    /// has been emitted yet — the queue is only replayed by [`Self::apply`], strictly after
    /// `finalize_compute` (and if the taker is refused, the queue is dropped unemitted).
    ///
    /// PAIRING. The k-th queued `Trade` is the k-th recorded fill, by two facts that must hold
    /// together:
    ///
    /// 1. The registry is created FRESH per `match_order` call (`trading::mod.rs`), so every
    ///    `Trade` in this queue belongs to this one taker order. There is no need — and no way —
    ///    to filter by `taker_order_id`.
    /// 2. Both walks call `taker_settlement.record_fill` IMMEDIATELY before pushing the fill's
    ///    `Trade`, with no early exit between them, so the two sequences advance in lockstep. A
    ///    K9-rejected maker `continue`s BEFORE both, so it perturbs neither.
    ///
    /// Both are load-bearing, so the count mismatch is a hard invariant error rather than a
    /// silent truncation: if a future walk ever pushes a `Trade` without recording a fill (or the
    /// registry is hoisted to span several orders), this fails loudly instead of shifting every
    /// subsequent fill's PnL onto the wrong trade.
    fn backfill_taker_realized_pnl(&mut self, fills: &[RecordedFill]) -> Result<(), PerpError> {
        let mut fills = fills.iter();
        for e in &mut self.events {
            if let MatchEvent::Trade {
                taker_realized_pnl, ..
            } = e
            {
                let fill = fills.next().ok_or_else(|| {
                    perp_invariant_err("more queued Trade events than recorded taker fills")
                })?;
                *taker_realized_pnl = fill.realized_pnl;
            }
        }
        if fills.next().is_some() {
            return Err(perp_invariant_err(
                "fewer queued Trade events than recorded taker fills",
            ));
        }
        Ok(())
    }

    /// Records ONE `AccountBalanceChanged` for a maker that has just been filled, derived from that
    /// maker's working copy.
    ///
    /// Called from the match walk immediately after the fill's `Trade` event is pushed, which is
    /// what makes the published stream read *"here is what happened to this maker, here is what the
    /// maker's account looks like as a result"*. Pushing it from inside
    /// [`settle_maker_fill_registry`] instead would put it BEFORE the `Trade` row it is meant to
    /// close.
    ///
    /// One event per fill. A maker order is consumed at most once per taker sweep, so that is one
    /// event per maker ORDER; a user resting two orders that both get filled by the same sweep gets
    /// two, which is the point — those are two economic events, and coalescing them to the taker's
    /// transaction boundary made a maker's notification cadence a function of an unrelated party's
    /// batching.
    pub(super) fn push_maker_snapshot(&mut self, user: Address) -> Result<(), PerpError> {
        let i = self
            .users
            .iter()
            .position(|(a, _)| *a == user)
            .ok_or_else(|| {
                perp_invariant_err("account snapshot for a maker not in the registry")
            })?;
        let balances = self.users[i].1.wallet_balances()?;
        self.users[i].1.last_published = Some(balances.clone());
        // `Order`: a maker fill is a matched order moving money, exactly Binance's `ORDER`.
        self.events.push(MatchEvent::AccountSnapshot {
            user,
            balances,
            reason: AccountUpdateReason::Order,
        });
        Ok(())
    }

    /// The user's working copy **if they already joined this match** — never loads, never inserts.
    /// The flush writes exactly these copies, so for a user in the registry this IS the post-flush
    /// state (funding already folded in by [`Self::get_or_load`]); that is what the zero-fill rest
    /// pre-check must measure against on a self-match.
    fn user_work(&self, user: Address) -> Option<&UserWork> {
        self.users.iter().find(|(a, _)| *a == user).map(|(_, w)| w)
    }

    /// The taker's [`RestBasis`] when the walk touched it but produced **no fill** — a self-match
    /// maker the K9 guard cancelled, which evolves the taker's own working copy (and its order
    /// lists) without ever reaching `finalize_compute`'s fill arm.
    ///
    /// `None` = the taker never joined this match, so the store is already the post-flush state for
    /// it and `rest_in_book` reads storage.
    ///
    /// No debit and no cover: with zero fills there is no `TakerPlan`, so `finalize_apply` does not
    /// run at all.
    pub(super) fn taker_rest_basis(
        &self,
        user: Address,
        base_decimals: u32,
        price_decimals: u32,
    ) -> Result<Option<RestBasis>, PerpError> {
        match self.user_work(user) {
            Some(w) => Ok(Some(RestBasis {
                pos: work_position_snapshot(w, base_decimals, price_decimals)?,
                wallet: w.account.perp_wallet_balance,
                cover_cancelled: false,
            })),
            None => Ok(None),
        }
    }

    pub(super) fn can_touch_user(&self, user: Address, limit: usize) -> bool {
        self.users.len() < limit || self.users.iter().any(|(address, _)| *address == user)
    }

    pub(super) fn defer_maker_level(
        &mut self,
        is_bid: bool,
        price: u64,
        queue: Vec<[u8; 32]>,
        count: u64,
    ) -> Result<(), PerpError> {
        if count == 0 {
            return Err(perp_invariant_err(
                "liquidation maker cap reached with zero live level count",
            ));
        }
        self.push_event(MatchEvent::SaveLevel {
            is_bid,
            price,
            queue,
            count,
        });
        Ok(())
    }

    /// First touch loads pos/account/both lists and settles funding: computed in memory NOW (the
    /// walk's working copies must carry the post-funding state) with the IF write + logs deferred
    /// as an event at this exact stream position.
    fn get_or_load<H: PerpHost>(
        &mut self,
        context: &mut H,
        user: Address,
        market_id: u64,
        market: &crate::types::Market,
    ) -> Result<usize, PerpError> {
        if let Some(i) = self.users.iter().position(|(a, _)| *a == user) {
            return Ok(i);
        }
        let mut pos = storage::load_position(context, user, market_id)?;
        let mut account = storage::load_account(context, user)?;
        // `Σ pos.margin` over the user's OTHER markets. Captured HERE, from the two pre-match reads
        // above and before funding touches `pos.margin`, because that is the only instant both terms
        // are in hand and both are pre-match. See `UserWork::other_market_position_margin`.
        let other_market_position_margin = account
            .total_position_margin
            .checked_sub(pos.margin)
            .ok_or_else(|| perp_err("match: Σ position margin underflow"))?;
        // Read-through: fees credited to the admin before they joined the registry are pending;
        // fold them into the copy so this user's wallet matches the old per-fill storage writes.
        //
        // ⚠️ BEFORE the funding compute below, not after. Funding never reads the account (it
        // settles against `pos.margin` only), so the two are order-independent as far as the
        // ARITHMETIC goes — but the funding compute now derives the `AccountBalanceChanged` header
        // for its own group from this copy, and a header taken before the fold would publish a
        // wallet the flush is about to overwrite.
        if self.fee_admin == Some(user) && self.admin_credit_pending > 0 {
            account.credit_perp(self.admin_credit_pending)?;
            self.admin_credit_pending = 0;
        }
        let pending = crate::funding::compute_funding_settlement(
            context,
            user,
            market,
            &mut pos,
            Some(&account),
        )?;
        // A funding settle publishes its OWN account header (`apply_funding_settlement`), so it
        // counts as a published payload for this user: recording it lets the flush's mark-clear gate
        // suppress a duplicate drain row for a user whose only event in this match was the funding
        // group — an insolvency-rejected maker, whose fill never lands.
        let mut last_published = None;
        if let Some(p) = pending {
            last_published = Some(p.snapshot().clone());
            self.events.push(MatchEvent::ApplyFunding(p));
        }
        let buy_entries = storage::load_buy_orders(context, user, market_id)?;
        let sell_entries = storage::load_sell_orders(context, user, market_id)?;
        self.users.push((
            user,
            UserWork {
                pos,
                account,
                buy_entries,
                sell_entries,
                dirty_buy: false,
                dirty_sell: false,
                other_market_position_margin,
                last_published,
            },
        ));
        Ok(self.users.len() - 1)
    }

    /// Fee-recipient wallet credit with registry read-through (zero storage writes during the
    /// walk): if the admin is a registry user the credit lands on their working copy NOW (so an
    /// admin who is also a maker sees earlier fees mid-walk, as the old per-fill storage writes
    /// provided); otherwise it accumulates and [`Self::flush`] materialises the total once (same
    /// key, same net value as the old per-fill credits). `get_or_load` folds the pending amount in
    /// if the admin joins the registry later.
    fn credit_admin(&mut self, admin: Address, amount: u64) -> Result<(), PerpError> {
        self.fee_admin = Some(admin);
        if let Some((_, w)) = self.users.iter_mut().find(|(a, _)| *a == admin) {
            return w.account.credit_perp(amount);
        }
        self.admin_credit_pending = self
            .admin_credit_pending
            .checked_add(amount)
            .ok_or_else(|| perp_err("placeOrder: admin fee credit overflow"))?;
        Ok(())
    }

    /// Applies the match: replays the deferred events in the EXACT order the walk recorded them
    /// (byte-identical log stream + IF/trade-counter evolution), then writes every touched user's
    /// final state (dirty lists, position + account — same key set as the old per-fill saves).
    ///
    /// Takes the threaded `Market` rather than a bare `market_id`: the deferred
    /// `PositionChanged` replay needs the mark + decimals to value `unrealizedProfit`, and this is
    /// the SAME `Market` whose `mark_price` every deferred fill was settled against
    /// (`settle_maker_fill_registry` / `finalize_compute` both read `market.mark_price`), so no
    /// site can drift onto a different mark. It also saves the `load_market_ref` this function
    /// used to do for the aggregate recompute's decimals.
    pub(super) fn flush<H: PerpHost>(
        mut self,
        context: &mut H,
        market: &crate::types::Market,
    ) -> Result<(), PerpError> {
        let market_id = market.market_id;
        let events = core::mem::take(&mut self.events);
        for e in events {
            match e {
                MatchEvent::ApplyFunding(p) => {
                    crate::funding::apply_funding_settlement(context, p)?;
                }
                MatchEvent::AbsorbBadDebt { market_id, amount } => {
                    absorb_bad_debt_into_insurance_fund(context, market_id, amount)?;
                }
                MatchEvent::FeeCredit { market_id, amount } => {
                    // Wallet credit already handled via the registry read-through (credit_admin);
                    // only the fee-total write replays here.
                    storage::add_market_fee_total(context, market_id, amount)?;
                }
                MatchEvent::PositionChanged {
                    user,
                    pos,
                    realized_pnl,
                    closed_quantity,
                } => {
                    crate::events::emit_position_changed(
                        context,
                        user,
                        market,
                        &pos,
                        realized_pnl,
                        closed_quantity,
                    )?;
                }
                MatchEvent::SaveOrder { order_id, order } => {
                    storage::save_order(context, &order_id, &order)?;
                }
                MatchEvent::DeleteOrder { order_id } => {
                    storage::delete_order(context, &order_id)?;
                }
                MatchEvent::OrderCancelled {
                    user,
                    order_id,
                    reason,
                } => {
                    context.log(Log {
                        address: PERP_DEX_ADDRESS,
                        data: IPerpDex::OrderCancelled {
                            user,
                            orderId: primitives::FixedBytes(order_id),
                            marketId: market_id,
                            reason: reason as u8,
                        }
                        .to_log_data(),
                    });
                }
                MatchEvent::SaveLevel {
                    is_bid,
                    price,
                    queue,
                    count,
                } => {
                    if is_bid {
                        storage::save_bid_level(context, market_id, price, count, &queue)?;
                    } else {
                        storage::save_ask_level(context, market_id, price, count, &queue)?;
                    }
                }
                MatchEvent::RemovePrice { is_bid, price } => {
                    if is_bid {
                        storage::remove_bid_price(context, market_id, price)?;
                    } else {
                        storage::remove_ask_price(context, market_id, price)?;
                    }
                }
                MatchEvent::SaveBest { is_bid, price } => {
                    if is_bid {
                        storage::save_best_bid(context, market_id, price)?;
                    } else {
                        storage::save_best_ask(context, market_id, price)?;
                    }
                }
                MatchEvent::MidPriceSample { best_bid, best_ask } => {
                    crate::risk::record_mid_price_sample_for_best_quote_change(
                        context, market_id, best_bid, best_ask,
                    )?;
                }
                MatchEvent::AccountSnapshot {
                    user,
                    balances,
                    reason,
                } => {
                    // Log only — the mark is cleared at the end of the per-user save loop below,
                    // because it is those saves that do the marking.
                    storage::log_account_snapshot(context, user, &balances, reason);
                }
                MatchEvent::Trade {
                    market_id,
                    taker_order_id,
                    maker_order_id,
                    taker,
                    maker,
                    price,
                    quantity,
                    taker_side,
                    taker_fee,
                    maker_fee,
                    taker_realized_pnl,
                    maker_realized_pnl,
                } => {
                    let trade_id = storage::next_trade_id(context, market_id)?;
                    context.log(Log {
                        address: PERP_DEX_ADDRESS,
                        data: IPerpDex::Trade {
                            marketId: market_id,
                            tradeId: trade_id,
                            takerOrderId: primitives::FixedBytes(taker_order_id),
                            makerOrderId: primitives::FixedBytes(maker_order_id),
                            taker,
                            maker,
                            price,
                            quantity,
                            takerSide: taker_side as u8,
                            takerFee: taker_fee,
                            makerFee: maker_fee,
                            takerRealizedPnl: taker_realized_pnl,
                            makerRealizedPnl: maker_realized_pnl,
                        }
                        .to_log_data(),
                    });
                }
            }
        }
        // Admin fee credits never materialised into a working copy (admin was not a trading
        // party): one storage credit of the accumulated total — same key + net value as the old
        // per-fill credits.
        if self.admin_credit_pending > 0 {
            let admin = self
                .fee_admin
                .ok_or_else(|| perp_invariant_err("pending admin fee credit without an admin"))?;
            // `Order`: a trading-fee credit is money moved by a matched order, and it is the ONLY
            // thing that moved this account. (This is the mark that survives to the drain for a fee
            // recipient who is also a maker — both marks are `Order`, so the merge is a no-op and
            // that row keeps a truthful label.)
            storage::mutate_account_balance(context, admin, AccountUpdateReason::Order, |a| {
                a.credit_perp(self.admin_credit_pending)
            })??;
        }
        // #A: base/price decimals for the reservation-aggregate recompute below — off the threaded
        // `Market` (this used to re-`load_market_ref` for them).
        let (bd, pd) = (market.base_decimals, market.price_decimals);
        for (user, mut w) in self.users {
            if w.dirty_buy {
                storage::save_buy_orders(context, user, market_id, &w.buy_entries)?;
            }
            if w.dirty_sell {
                storage::save_sell_orders(context, user, market_id, &w.sell_entries)?;
            }
            // The match may have filled/cancelled maker & taker orders — resync the per-side
            // aggregates from the authoritative working-copy lists (recompute, not incremental:
            // the match path is rare and already re-serialises the whole list here).
            let (tbq, tbn) = crate::math::sum_side_totals(w.buy_entries.iter().copied(), bd, pd)?;
            let (tsq, tsn) = crate::math::sum_side_totals(w.sell_entries.iter().copied(), bd, pd)?;
            // ...and the walk maintains them INCREMENTALLY as it goes, because the gates it
            // evaluates mid-match (and `release_open_order_margin_core`, which subtracts from them) read
            // them. Belt-and-braces: the two must agree, or a gate was decided on a stale `Bid`.
            debug_assert_eq!(
                (
                    w.pos.total_buy_qty,
                    w.pos.total_buy_notional,
                    w.pos.total_sell_qty,
                    w.pos.total_sell_notional
                ),
                (tbq, tbn, tsq, tsn),
                "match flush: incrementally-maintained (Bid, Ask) for {user} diverged from the \
                 working-copy order lists"
            );
            w.pos.total_buy_qty = tbq;
            w.pos.total_buy_notional = tbn;
            w.pos.total_sell_qty = tsq;
            w.pos.total_sell_notional = tsn;
            // ── The working-copy snapshot formula CONVERGES on the settled store ──────────────
            //
            // Every per-fill account snapshot this match published was derived from the working copy
            // (`UserWork::wallet_balances`), because the copy — not storage — is the authoritative
            // record during the walk, and writing maker state through per fill would destroy that
            // (`MatchRegistry::user_work`'s "this IS the post-flush state" contract, and with it the
            // K9 measurement and the zero-fill rest pre-check). Derived state is only as good as its
            // convergence, so the derivation is checked against the thing it stands in for: the FINAL
            // working copy, put through the same formula, must equal what
            // `margin_view::index_account_wallet_balances` reads back out of the settled store one
            // line later. Captured before the writes because `save_account` consumes `w.account`.
            let published = w.last_published.take();
            // Release builds only fold it when there is a published payload to compare against —
            // `wallet_balances` parses the decimal `usdc_balance`, and a flushed user who published
            // nothing (an untouched maker, a K9 reject) must not pay for that. Debug builds always
            // fold it, because the convergence guard below is the reason the derivation is trusted at
            // all and it has to run on every flushed user, not only the ones that emitted.
            let derived = if published.is_some() || cfg!(debug_assertions) {
                Some(w.wallet_balances()?)
            } else {
                None
            };
            // `Order` for every flushed user: each is in this registry because a matched order
            // touched them (a maker fill, the taker leg of a self-match, or a fill rejected as
            // opening-into-insolvency whose order was cancelled).
            storage::save_position(context, user, market_id, &w.pos, AccountUpdateReason::Order)?;
            storage::save_account(context, user, w.account, AccountUpdateReason::Order)?;
            #[cfg(debug_assertions)]
            {
                let settled = crate::margin_view::index_account_wallet_balances(
                    context,
                    user,
                    "match flush convergence",
                )?;
                debug_assert_eq!(
                    derived.as_ref(),
                    Some(&settled),
                    "match flush: the working-copy account-snapshot formula for {user} \
                     (Σ over other markets + working pos.margin, on the working wallet) diverged \
                     from the settled store — every per-fill snapshot published during this match \
                     is derived from that formula"
                );
            }
            // ── The duplicate mark ───────────────────────────────────────────────────────────
            //
            // The two writes above MARK this user in the call-scoped touched set, so the end-of-call
            // drain would publish a second event for a maker who already got one per fill. Clear the
            // mark — but only when the drain would have nothing to add, i.e. when the settled state
            // is byte-identical to the last payload published for this user. That is not the common
            // case dressed up as a rule: a maker who is ALSO the fee recipient keeps receiving
            // `credit_admin` read-through credits from LATER makers' fills, after their own last
            // snapshot, and unmarking unconditionally would drop that update on the floor. Comparing
            // payloads is exact here precisely because of the convergence checked just above.
            //
            // ⚠️ That branch is LIVE and its only coverage is
            // `trading::tests::account_snapshot_events::a_fee_recipient_who_is_also_a_maker_gets_a_drain_row_for_the_later_fee`.
            // Replacing this condition with a bare `published.is_some()` passes every other test in
            // the suite — including `assert_account_update_groups`, which is structurally
            // BLIND to it: the dropped row carries a fee credit, and a fee credit to the admin is
            // named by no `Trade` / `PositionChanged` / `FundingSettled`, so that user is never left
            // "pending". Do not delete that test believing the generic invariant subsumes it.
            //
            // The equality also does NOT hold for a self-matching taker — the taker leg evolves this
            // same working copy past the maker-leg payload — so a self-matcher stays marked here on
            // purpose, and `finalize_apply`'s own emit is what closes them out. See
            // `storage::publish_account_snapshot_now`.
            if published.is_some() && published == derived {
                storage::clear_account_snapshot_mark(context, user);
            }
        }
        Ok(())
    }
}

/// Registry-backed maker fill (commit-only #23 L1): identical decision + effects to
/// [`settle_maker_fill`] but pos/account/lists evolve in the registry (saved once at flush).
/// IF writes (bad debt), fee credit, and the PositionChanged log stay immediate — the same
/// stream positions as today.
#[allow(clippy::too_many_arguments)]
pub(super) fn settle_maker_fill_registry<H: PerpHost>(
    context: &mut H,
    reg: &mut MatchRegistry,
    maker: Address,
    maker_order_id: &[u8; 32],
    market_id: u64,
    fill_price: u64,
    fill_qty: u64,
    taker_side: Side,
    market: &crate::types::Market,
) -> Result<MakerFillOutcome, PerpError> {
    let maker_side = taker_side.opposite();
    let i = reg.get_or_load(context, maker, market_id, market)?;
    // mark_price is a field of the threaded Market — no per-maker storage read.
    let mark = market.mark_price;

    let w = &mut reg.users[i].1;
    let core = settle_maker_fill_core(
        &mut w.pos,
        &mut w.account,
        &mut w.buy_entries,
        &mut w.sell_entries,
        mark,
        maker_side,
        maker_order_id,
        fill_price,
        fill_qty,
        market,
    )?;

    let (maker_fee, bad_debt, realized_pnl, closed_quantity) = match core {
        MakerFillCore::RejectedInsolvent => return Ok(MakerFillOutcome::RejectedInsolvent),
        MakerFillCore::Filled {
            maker_fee,
            bad_debt,
            realized_pnl,
            closed_quantity,
        } => (maker_fee, bad_debt, realized_pnl, closed_quantity),
    };
    match maker_side {
        Side::Buy => w.dirty_buy = true,
        Side::Sell => w.dirty_sell = true,
    }
    let pos_snapshot = w.pos.clone();

    reg.push_event(MatchEvent::AbsorbBadDebt {
        market_id,
        amount: bad_debt,
    });
    // Fee credit: the admin==ZERO reject is checked NOW (read-only, pre-write); the fee-total
    // write defers as an event, and the admin wallet credit routes through the registry
    // read-through so an admin who is also a trading party sees earlier fees mid-walk.
    if maker_fee > 0 {
        let admin = storage::load_admin(context)?;
        if admin == Address::ZERO {
            return Err(perp_err("placeOrder: fee recipient not initialised"));
        }
        reg.push_event(MatchEvent::FeeCredit {
            market_id,
            amount: maker_fee,
        });
        reg.credit_admin(admin, maker_fee)?;
    }

    // NOTE: the `PositionChanged` for this fill is NOT pushed here — it is handed back to the walk
    // and pushed after the fill's `Trade` + the maker's `AccountSnapshot`, so header and row are
    // adjacent. See `MakerFillOutcome::Filled`.
    Ok(MakerFillOutcome::Filled {
        maker_fee,
        pos_snapshot,
        realized_pnl,
        closed_quantity,
    })
}

/// Registry-backed maker cancel for a `RejectedInsolvent` fill: drops the rejected order from the
/// registry copies (flushed later) and writes the order status + OrderCancelled log immediately
/// (same stream positions as before). No money moves — the order held no escrow; removing it from
/// `Bid`/`Ask` is the whole effect.
pub(super) fn cancel_rejected_maker_registry<H: PerpHost>(
    context: &mut H,
    reg: &mut MatchRegistry,
    maker: Address,
    market_id: u64,
    maker_side: Side,
    order_id: &[u8; 32],
    maker_order: &mut crate::types::Order,
    market: &crate::types::Market,
) -> Result<(), PerpError> {
    let i = reg.get_or_load(context, maker, market_id, market)?;
    let w = &mut reg.users[i].1;
    super::release_open_order_margin_core(
        &mut w.pos,
        &mut w.buy_entries,
        &mut w.sell_entries,
        maker_side,
        order_id,
        market,
    )?;
    match maker_side {
        Side::Buy => w.dirty_buy = true,
        Side::Sell => w.dirty_sell = true,
    }
    // delete-on-terminal: the rejected maker is Cancelled → removed from the map (its id stays in
    // the level queue and is swept as a stale entry; the match walk decrements the live count for
    // this reject, so the level's count stays accurate).
    maker_order.status = OrderStatus::Cancelled;
    reg.push_event(MatchEvent::DeleteOrder {
        order_id: *order_id,
    });
    reg.push_event(MatchEvent::OrderCancelled {
        user: maker,
        order_id: *order_id,
        // K9 reject: the protocol dropped this maker, its owner did not.
        reason: crate::types::CancelReason::MakerInsolvent,
    });
    Ok(())
}

/// Effect summary of [`finalize_core`] (the pure taker-settlement decision).
pub(super) struct TakerFillCore {
    pub(super) bad_debt: u64,
    pub(super) fee: u64,
    pub(super) total_required: u64,
    pub(super) realized_pnl: i64,
    pub(super) closed_quantity: u64,
}

/// PURE core of [`TakerSettlement::finalize`] (commit-only #23, tranche-4): fill aggregation
/// (conservation-exact close/open split), position fill, K9 open-into-insolvency guard, fee +
/// total-required — over in-memory copies only, NO storage access. Any `Err` (K9 reject, checked
/// arithmetic) fires before the caller has written anything.
///
/// It no longer needs the taker's order lists: they were inputs to the flip-aware reservation
/// recompute, which went with the escrow. The fills do not touch the taker's own resting orders
/// (except on a self-match, which the registry handles on the maker side).
#[allow(clippy::too_many_arguments)]
fn finalize_core(
    pos: &mut crate::types::PerpPosition,
    account: &mut crate::types::UserAccount,
    fills: &mut [RecordedFill],
    taker_side: Side,
    mark_price: u64,
    taker_fee_bps: u64,
    market: &crate::types::Market,
) -> Result<TakerFillCore, PerpError> {
    let mut remaining_closing_qty = pos.amount.unsigned_abs();
    let mut closing_qty = 0u64;
    let mut closing_value = 0u64;
    let mut opening_qty = 0u64;
    let mut opening_value = 0u64;
    let is_buy = taker_side == Side::Buy;

    // ── Per-fill realized-PnL apportionment, for `Trade.takerRealizedPnl` (LOGGING ONLY) ────────
    //
    // The taker settles ONCE, below, on the summed legs: `apply_position_fill` is the only thing
    // that mutates state and it sees only the totals. But the `@order` stream's `o.rp` is a
    // PER-FILL figure (measured Binance, internal R15: apportioned per fill, never accumulated
    // onto the last one), so the aggregate has to be split N ways. These two lines are the entry
    // basis that split is taken against.
    //
    // ⚠️⚠️ THE SPLIT IS BY TELESCOPING DIFFERENCES OF THE AGGREGATE FORMULA. DO NOT "SIMPLIFY" IT
    // INTO AN INDEPENDENT PER-FILL COMPUTATION. ⚠️⚠️
    //
    // `apply_position_fill`'s closing leg is, for a position of `pos_abs` at basis `vq`:
    //
    //     remaining_qty = pos_abs − closing_qty
    //     remaining_vq  = vq × remaining_qty / pos_abs        ← ONE truncating division
    //     vq_fraction   = vq − remaining_vq
    //     realized_pnl  = vq_fraction + (is_buy ? −closing_value : +closing_value)
    //
    // We walk `remaining_closing_qty` down fill by fill and evaluate `remaining_vq` at each step
    // AGAINST THE ORIGINAL `pos_abs`, then take successive differences:
    //
    //     fill 1: rem_q₁ = pos_abs − q₁   rem_vq₁ = vq × rem_q₁ / pos_abs   vqf₁ = vq      − rem_vq₁
    //     fill 2: rem_q₂ = rem_q₁  − q₂   rem_vq₂ = vq × rem_q₂ / pos_abs   vqf₂ = rem_vq₁ − rem_vq₂
    //     …
    //     Σ vqf = vq − rem_vqₙ  ≡  the aggregate `vq_fraction`
    //
    // Every intermediate `rem_vq` term is shared VERBATIM by two adjacent steps, so the sum
    // telescopes to the aggregate with ZERO RESIDUAL no matter how any individual division
    // truncates — the cancellation is exact because it is literally the same value subtracted and
    // added back, not two roundings that happen to agree. Computing each fill's PnL independently
    // (its own `vq × q_k / pos_abs`) would re-floor N times, and the N floors would NOT sum to the
    // aggregate's single floor: that is precisely the ±1 phantom mint/burn class this repo has
    // already been bitten by, and exactly what the "floor the WHOLE matched quantity ONCE" comment
    // in the loop below is warning about for the closing/opening split. Same technique, same
    // reason, as the slice form in `math::maintenance_margin`.
    //
    // The cash leg needs nothing: the loop already accumulates `closing_value` from the same
    // `fill_closing_value` terms, so `Σ fill_closing_value == closing_value` by construction.
    //
    // `pos` is NOT mutated by this loop (only read, for `pos.amount`'s sign and size) — the sole
    // mutation is `apply_position_fill` after it. So every closing leg is priced off the ONE
    // pre-order basis they all genuinely share, which is what makes the telescoping legitimate.
    let pos_abs = pos.amount.unsigned_abs();
    let entry_vq = pos.v_quote_balance as i128;
    let mut prev_remaining_vq = entry_vq;

    for fill in fills.iter_mut() {
        if fill.taker_side != taker_side {
            return Err(perp_invariant_err("taker settlement mixed fill sides"));
        }
        let fill_closing_qty = if (is_buy && pos.amount < 0) || (!is_buy && pos.amount > 0) {
            fill.quantity.min(remaining_closing_qty)
        } else {
            0
        };
        remaining_closing_qty = remaining_closing_qty.saturating_sub(fill_closing_qty);
        let fill_opening_qty = fill.quantity - fill_closing_qty;
        // Conservation: floor the WHOLE matched quantity ONCE and derive the opening leg by
        // subtraction, so the taker and maker attribute the SAME total quote to this fill
        // (closing + opening == calc_value(price, fill.quantity)); independent flooring lets the
        // two parties' split boundaries floor to a different sum → ±1 phantom mint/burn.
        let fill_value = calc_value(
            fill.price,
            fill.quantity,
            market.base_decimals,
            market.price_decimals,
        )?;
        let fill_closing_value = calc_value(
            fill.price,
            fill_closing_qty,
            market.base_decimals,
            market.price_decimals,
        )?;
        let fill_opening_value = fill_value
            .checked_sub(fill_closing_value)
            .ok_or_else(|| perp_err("settlement: fill opening value underflow"))?;
        closing_qty = closing_qty
            .checked_add(fill_closing_qty)
            .ok_or_else(|| perp_err("placeOrder: closing quantity overflow"))?;
        closing_value = closing_value
            .checked_add(fill_closing_value)
            .ok_or_else(|| perp_err("placeOrder: closing value overflow"))?;
        opening_qty = opening_qty
            .checked_add(fill_opening_qty)
            .ok_or_else(|| perp_err("placeOrder: opening quantity overflow"))?;
        opening_value = opening_value
            .checked_add(fill_opening_value)
            .ok_or_else(|| perp_err("placeOrder: opening value overflow"))?;

        // ── This fill's step of the telescoping split (see the note above the loop) ─────────────
        //
        // `remaining_closing_qty` has ALREADY been decremented by this fill's closing leg, so it
        // is `rem_qₖ`. Evaluate the aggregate's `remaining_vq` at it — against the ORIGINAL
        // `pos_abs`, never against a running position — and difference against the previous step.
        //
        // `pos_abs == 0` is a flat position: no fill can close (the `fill_closing_qty` test above
        // is false for `pos.amount == 0`), the aggregate skips its closing leg entirely, and the
        // division would be by zero. So every fill's share is 0, which is also what the formula
        // would say.
        //
        // A purely OPENING leg falls out as 0 for free and needs no special case:
        // `fill_closing_qty == 0` leaves `remaining_closing_qty` untouched, so `remaining_vq` is
        // bit-identical to `prev_remaining_vq`, `vq_fraction` is 0, and `fill_closing_value` is 0.
        // That is the flip behaviour R15 measured — `rp` covers only the closing leg.
        fill.realized_pnl = if pos_abs == 0 {
            0
        } else {
            let remaining_vq = entry_vq * remaining_closing_qty as i128 / pos_abs as i128;
            let vq_fraction = prev_remaining_vq - remaining_vq;
            prev_remaining_vq = remaining_vq;
            let fill_close_quote_delta = if is_buy {
                -(fill_closing_value as i128)
            } else {
                fill_closing_value as i128
            };
            i64::try_from(vq_fraction + fill_close_quote_delta)
                .map_err(|_| perp_err("settlement: per-fill realized PnL exceeds i64 range"))?
        };
    }

    let fill_outcome = apply_position_fill(
        pos,
        &mut account.perp_wallet_balance,
        closing_qty,
        closing_value,
        opening_qty,
        opening_value,
        is_buy,
        // NOT capped, unlike the maker path: the taker's draw is gated at fill time — the caller
        // (`finalize_compute`) refuses, or covers-then-refuses, unless `available` covers
        // `total_required`, and `available = wallet − Σ ooIM ≤ wallet`, so the cash IS there. A cap
        // here would silently short-fund a taker the gate already vouched for.
        OpeningMarginFunding::Requirement,
    )?;

    // ── THE PROOF THAT THE APPORTIONMENT IS EXACT ───────────────────────────────────────────────
    //
    // The per-fill values are DERIVED FOR LOGGING ONLY; the call above is the sole mutation of
    // state and it saw only the totals. This assertion is what pins the two together: the split
    // must sum to the very number that moved the money, with no residual. It is the runtime
    // statement of the telescoping argument above — if someone rewrites the split as an
    // independent per-fill computation, the re-flooring shows up here rather than as a ±1 drift in
    // a consumer's `cr` weeks later.
    //
    // Deliberately on the PRODUCTION taker path, not in a test: it then runs under every debug
    // build — the whole test suite, and every dev/devnet node — against real traffic, instead of
    // only the fill shapes a test author thought of. `debug_assert_eq!` puts both the comparison
    // and the fold behind `cfg(debug_assertions)`, so a release node pays nothing for it.
    debug_assert_eq!(
        fills.iter().map(|f| f.realized_pnl as i128).sum::<i128>(),
        fill_outcome.realized_pnl as i128,
        "per-fill realized PnL must telescope EXACTLY to the aggregate that mutated state"
    );

    // ── Trading fee: charged from the margin this fill just funded (Binance parity) ──
    // Computed HERE, before K9, because the fee must have LEFT the position margin by the time
    // maintenance is checked — otherwise a fill could be admitted that is instantly liquidatable.
    // `fee_from_margin = min(fee, opening_margin)` cannot underflow: `apply_position_fill` just
    // added the whole `opening_margin` to `pos.margin` (and never leaves `pos.margin` negative),
    // and a pure close has `opening_margin == 0` → the whole fee falls to the wallet, exactly as
    // before this change. A flip splits proportionally: the fee is charged on the FULL notional
    // while only the opening leg funds margin, so a naive `margin -= fee` would over-draw.
    let fee_notional = closing_value
        .checked_add(opening_value)
        .ok_or_else(|| perp_err("placeOrder: taker fee notional overflow"))?;
    let fee = calc_trading_fee(fee_notional, taker_fee_bps)?;
    let fee_from_margin = fee.min(fill_outcome.opening_margin);
    let fee_from_wallet = fee - fee_from_margin;
    let from_margin_i64 = checked_u64_to_i64(fee_from_margin, "settlement: fee from margin")?;
    pos.margin = pos
        .margin
        .checked_sub(from_margin_i64)
        .ok_or_else(|| perp_err("settlement: margin fee underflow"))?;

    // Open-into-insolvency guard (K9): a taker may not open/increase a position that is already
    // below maintenance margin at the current mark. Skipped when mark is unset (0). Closing/
    // reducing is never gated: its realized loss beyond margin is legitimate bad debt.
    if opening_qty > 0
        && mark_price > 0
        && !is_above_maintenance_margin(
            &market.tiers,
            mark_price,
            pos.amount,
            pos.v_quote_balance,
            pos.margin,
            market.base_decimals,
            market.price_decimals,
        )?
    {
        return Err(perp_err("placeOrder: open would breach maintenance margin"));
    }

    // Per-open margin-tier guard. NOT redundant with K9 above: K9 only asks "is the
    // resulting position solvent at mark", so a user sitting at leverage 5 who grows into
    // a max-leverage-3 tier passes K9 yet must still be refused. Under today's single-tier
    // table this can never fire (the bound equals the `setLeverage` cap), which is exactly
    // the point: enabling multi-tier becomes a pure config change. Pre-write, like every
    // other genuine reject on this path (commit-only #23).
    if opening_qty > 0 {
        let abs_notional = calc_value_i64(
            mark_price,
            pos.amount,
            market.base_decimals,
            market.price_decimals,
        )?
        .checked_abs()
        .ok_or_else(|| perp_err("placeOrder: tier notional abs overflow"))?;
        let tier_cap = max_leverage_for_notional(&market.tiers, abs_notional);
        if pos.leverage.max(1) > tier_cap as u64 {
            return Err(perp_err(
                "placeOrder: leverage exceeds the margin tier for this position size",
            ));
        }
    }

    // The reservation recompute that used to sit here is GONE with the escrow. `pos.amount`
    // changing does re-price the taker's remaining resting orders — but on the DERIVED basis that
    // shows up by itself, in the next `ooIM` evaluation, with no stored field to reconcile and no
    // `mr_credit`/`mr_extra` wallet legs to keep `W + M + MR` conserved. `W + M` is conserved here
    // by construction: the wallet funds exactly what the position and the fee recipient receive.
    //
    // The wallet funds the opening margin plus only the part of the fee the opening margin could
    // not absorb. Conservation: wallet moves by −(opening_margin + fee_from_wallet), margin by
    // +(opening_margin − fee_from_margin), so the user's net change is exactly −fee — the full
    // amount `finalize_apply` hands to the fee recipient.
    let total_required = fill_outcome
        .opening_margin
        .checked_add(fee_from_wallet)
        .ok_or_else(|| perp_err("placeOrder: opening margin + fee overflow"))?;

    Ok(TakerFillCore {
        bad_debt: fill_outcome.bad_debt,
        fee,
        total_required,
        realized_pnl: fill_outcome.realized_pnl,
        closed_quantity: closing_qty,
    })
}

/// Outcome of [`settle_maker_fill_core`]: the pure maker-fill decision + its effect summary.
pub(super) enum MakerFillCore {
    Filled {
        maker_fee: u64,
        bad_debt: u64,
        realized_pnl: i64,
        closed_quantity: u64,
    },
    RejectedInsolvent,
}

/// PURE core of [`settle_maker_fill`] (commit-only #23, tranche-4): the complete maker-fill
/// decision sequence — trial fill, K9 open-into-insolvency guard, entry reduce, flip-aware
/// reserve recompute, net release — over in-memory working copies only. NO storage access, so the
/// match compute phase can run it to full fidelity before any write. The caller has already
/// computed funding on `pos`/`account` (funding persists regardless of outcome) and supplies both
/// order-entry lists (the maker side is reduced; both feed the reserve recompute).
#[allow(clippy::too_many_arguments)]
pub(super) fn settle_maker_fill_core(
    pos: &mut crate::types::PerpPosition,
    account: &mut crate::types::UserAccount,
    buy_entries: &mut std::collections::VecDeque<crate::types::OrderEntry>,
    sell_entries: &mut std::collections::VecDeque<crate::types::OrderEntry>,
    mark_price: u64,
    maker_side: Side,
    maker_order_id: &[u8; 32],
    fill_price: u64,
    fill_qty: u64,
    market: &crate::types::Market,
) -> Result<MakerFillCore, PerpError> {
    // READ-ONLY: locate the maker's book entry and price this fill's share of the order's maker
    // fee. Done up front because the fee must be charged BEFORE the K9 check below, while the
    // entry itself may only be reduced once the fill is accepted (a K9 reject leaves the entry in
    // place for `cancel_rejected_maker_registry` to release).
    // #B: fill_price == the resting maker's order price (a match executes at the maker's level), so
    // it is exactly the sort key `entry_fill_plan` binary-searches on.
    let (entry_idx, entry_new_amount, maker_fee) = entry_fill_plan(
        match maker_side {
            Side::Buy => &*buy_entries,
            Side::Sell => &*sell_entries,
        },
        maker_order_id,
        fill_price,
        matches!(maker_side, Side::Buy),
        fill_qty,
        market,
        match maker_side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        },
    )?;

    // Compute the fill on a TRIAL clone first (single computation — adopted verbatim on accept).
    let fill = split_position_fill(pos.amount, maker_side, fill_price, fill_qty, market)?;
    let mut trial_pos = pos.clone();
    let mut trial_wallet = account.perp_wallet_balance;
    let fill_outcome = apply_position_fill(
        &mut trial_pos,
        &mut trial_wallet,
        fill.closing_qty,
        fill.closing_value,
        fill.opening_qty,
        fill.opening_value,
        fill.is_buy,
        // M1 (see the block below `trial_wallet -= opening_margin`): the maker's opening leg is
        // funded with the cash actually at hand, and `trial_pos.margin` carries any shortfall.
        OpeningMarginFunding::CappedAtCashAtHand,
    )?;

    // ── Maker trading fee: charged from the margin this fill just funded (Binance parity) ──
    // This is a NEW wallet-side charge, not a deletion: the maker fee used to be pre-escrowed in
    // `pos.fee_reserved` at placement and merely released here, so nothing debited the maker's
    // wallet. Now the fee comes out of `opening_margin` first, with only the uncovered part
    // (the pure-close case, `opening_margin == 0`) falling to the wallet. Applied to the TRIAL
    // copy so it is already out of `trial_pos.margin` when K9 runs below.
    let fee_from_margin = maker_fee.min(fill_outcome.opening_margin);
    let fee_from_wallet = maker_fee - fee_from_margin;
    let from_margin_i64 = checked_u64_to_i64(fee_from_margin, "settlement: fee from margin")?;
    let from_wallet_i64 = checked_u64_to_i64(fee_from_wallet, "settlement: fee from wallet")?;
    trial_pos.margin = trial_pos
        .margin
        .checked_sub(from_margin_i64)
        .ok_or_else(|| perp_err("settlement: margin fee underflow"))?;
    trial_wallet = trial_wallet
        .checked_sub(from_wallet_i64)
        .ok_or_else(|| perp_err("perp wallet: balance underflow"))?;

    // ── Opening margin comes out of the WALLET at fill time, capped at what is there (M1) ──
    // Under the escrow this was free here: `apply_position_fill` added `opening_margin` to
    // `pos.margin` and the money came from `old_reserved`, the capital placement had already
    // withheld. Nothing is withheld any more, so the wallet has to fund it NOW — and the money may
    // not be there. `perp_wallet_balance` is only guaranteed to cover `Σ ooIM` at the moment each
    // order was ADMITTED; a later mark move, a taker fill, or a fee can leave it short, and ooIM
    // (which values the position leg at MARK and nets the close a fill performs) is not an upper
    // bound on the fill's actual draw in the first place.
    //
    // # The fill HAPPENS. Why this is not a reject
    //
    // MEASURED: Binance lets an open-order lien sit UNDER-COVERED and never sweeps it. At
    // `crossWalletBalance − totalOpenOrderInitialMargin = −0.00085981` an already-resting order
    // stayed `status = 'NEW'` for the whole observation window while, IN THE SAME INSTANT, a NEW
    // order was refused `-2019`. The doc's verdict: 「预留是一笔留置权,交易所允许它被欠覆盖,也不做
    // 任何清扫。订单只在清算时死。对我们的引擎:不需要实现挂单的中途拆除逻辑。」
    // (`misc/binance-margin-verified-model.md` §1.6, "保证金只在下单那一刻校验"). Admission is
    // checked once, at placement; after that the only thing that kills an order is LIQUIDATION.
    //
    // This code used to return `RejectedInsolvent` here, which cancelled the maker order and let
    // the taker walk on. That is model **M2 ("don't fill")** in
    // `misc/binance-flip-and-admission.md` §3.3, and it is wrong: the fill goes through.
    //
    // # Where the deficit lands — MEASURED (R11): the SILO, not the wallet
    //
    // This used to implement **M1′** (silo funded in full, wallet driven NEGATIVE by the
    // shortfall), from a conjecture §3.3 marked ~50/50 pending an experiment. R11 ran it and
    // measured **M1** (`derived-ooim-plan.md` §3a): the silo receives all the cash there is and not
    // a satoshi more — `isolatedWallet = 63.10632800 = W0 63.89332800 + realized −0.78700000`,
    // digit-for-digit, against an IM-implied `64.16451380`, i.e. deliberately `1.05818580` SHORT.
    // The wallet went negative only by the fill COMMISSION (`−0.25717240`) and the insurance fund
    // cleared that three seconds later. (`1.05818580 + 0.25717240 == 1.31535820`, the whole gap,
    // exactly bisected.)
    //
    // So the cap lives in `apply_position_fill` (`OpeningMarginFunding::CappedAtCashAtHand`) and
    // the debit below can no longer take `trial_wallet` under zero for a margin shortfall. What it
    // buys is STRUCTURAL, not just parity:
    //
    // * Under M1 the deficit IS PART OF THE POSITION, so it is resolved when the position closes or
    //   is liquidated, through the EXISTING liquidation-deficit → insurance-fund path: a thinner
    //   silo absorbs less of the close's loss, so more of it arrives as `bad_debt` and
    //   `absorb_bad_debt_into_insurance_fund` routes it. Nothing is stranded.
    //   Under M1′ the deficit was DECOUPLED from the position: liquidate the position and an
    //   isolated negative wallet is left behind with no path out of it.
    //   Pinned end-to-end by `risk::tests::usdc_custody`.
    // * The liquidation price is HONEST. `is_above_maintenance_margin` and the liquidation sweep
    //   read `pos.margin`, so a silo that really is short prices its own LP against real risk.
    //   Under M1′ the silo looked full and LP was optimistic. Pinned by
    //   `a_short_silo_lowers_the_maintenance_buffer_it_is_measured_against`.
    //
    // The debit is NOT optional, and the cap does not make it so: the wallet must lose exactly what
    // the position gains. Dropping it would fund `pos.margin` from nowhere — a mint, which the
    // conservation fuzz catches immediately.
    //
    // Applied to `trial_wallet`, i.e. AFTER this fill's own close proceeds have landed: a flip
    // legitimately funds its opening leg out of the closing leg's released margin and profit, so
    // most flips are fully funded and nothing is short at all.
    //
    // # What can still take the wallet negative here: the COMMISSION, and only it
    //
    // `fee_from_wallet` above is `maker_fee − min(maker_fee, opening_margin)`, so when the capped
    // opening margin cannot absorb the commission the wallet pays the rest and may go under — by at
    // most the maker fee. This is deliberate and is §3a's recommendation for the commission gap
    // ("让 `pos.margin` 再短那一截", i.e. let the silo carry it, and do NOT open a second
    // insurance-fund path for it); the residue that reaches the wallet is the case with no silo to
    // carry it at all — chiefly a PURE CLOSE (`opening_qty == 0` ⇒ `opening_margin == 0`) whose own
    // proceeds were eaten by an insolvent close. Binance's wallet goes negative in exactly this
    // one place too, and only this one. That is why `perp_wallet_balance` stays `i64` with a
    // clamped ABI view (`types::account`), and why the negative-wallet gates
    // (`derived_available_balance` refusing money-out, a deposit netting against it) are still
    // live: they are now belt and braces for a commission-sized transient rather than the primary
    // home of a structural deficit.
    let opening_margin_i64 = checked_u64_to_i64(
        fill_outcome.opening_margin,
        "settlement: maker opening margin",
    )?;
    trial_wallet = trial_wallet
        .checked_sub(opening_margin_i64)
        .ok_or_else(|| perp_err("perp wallet: balance underflow"))?;

    // Open-into-insolvency guard (K9): a fill may not open/increase the maker's position below
    // maintenance margin at the current mark. Skipped when mark == 0. Closing/reducing is never
    // gated — its realized loss beyond margin is legitimate bad debt (absorbed on accept).
    if fill.opening_qty > 0
        && mark_price > 0
        && !is_above_maintenance_margin(
            &market.tiers,
            mark_price,
            trial_pos.amount,
            trial_pos.v_quote_balance,
            trial_pos.margin,
            market.base_decimals,
            market.price_decimals,
        )?
    {
        return Ok(MakerFillCore::RejectedInsolvent);
    }

    // Per-open margin-tier guard — the maker-side twin of the taker guard in
    // `finalize_core`. Not redundant with K9 (which only checks solvency at mark);
    // a no-op under the single-tier default, so enabling multi-tier is a config change.
    // Evaluated on the TRIAL copy, before anything is adopted → pre-write.
    if fill.opening_qty > 0 {
        let abs_notional = calc_value_i64(
            mark_price,
            trial_pos.amount,
            market.base_decimals,
            market.price_decimals,
        )?
        .checked_abs()
        .ok_or_else(|| perp_err("settlement: tier notional abs overflow"))?;
        let tier_cap = max_leverage_for_notional(&market.tiers, abs_notional);
        if trial_pos.leverage.max(1) > tier_cap as u64 {
            return Ok(MakerFillCore::RejectedInsolvent);
        }
    }

    // Accept: adopt the trial result verbatim.
    *pos = trial_pos;
    account.perp_wallet_balance = trial_wallet;

    // Apply the entry reduce planned above (the lists were untouched in between, so `entry_idx`
    // is still valid), and shrink `Bid`/`Ask` by exactly the term the reduce removes. Two points:
    //   * at the entry's FROZEN `assuming_price`, not its limit price — the order's margin basis
    //     does not change because it partially filled, only its `amount` does. This is the site that
    //     makes "freeze the PRICE, not the contribution" work;
    //   * as the DIFFERENCE of the two per-order-floored `calc_value`s (not one call on `fill_qty`),
    //     which is what keeps the aggregate byte-identical to a fresh fold over the reduced list —
    //     the equality the registry flush asserts.
    {
        let entries = match maker_side {
            Side::Buy => &mut *buy_entries,
            Side::Sell => &mut *sell_entries,
        };
        let e = entries[entry_idx];
        let (bd, pd) = (market.base_decimals, market.price_decimals);
        let notional_delta = e
            .margin_notional(bd, pd)?
            .checked_sub(calc_value(e.assuming_price, entry_new_amount, bd, pd)?)
            .ok_or_else(|| {
                perp_invariant_err("settlement: maker entry notional delta underflow")
            })?;
        if entry_new_amount == 0 {
            entries.remove(entry_idx);
        } else {
            entries[entry_idx].amount = entry_new_amount;
        }
        super::remove_entry_from_side_aggregates(pos, maker_side, fill_qty, notional_delta)?;
    }
    // No reservation recompute and no `net_release`: the escrow those maintained is gone, and the
    // fill's effect on the maker's remaining open-order requirement is picked up on the next
    // `ooIM` evaluation from the `Bid`/`Ask` just updated and the new `pos.amount`.

    Ok(MakerFillCore::Filled {
        maker_fee,
        bad_debt: fill_outcome.bad_debt,
        realized_pnl: fill_outcome.realized_pnl,
        closed_quantity: fill.closing_qty,
    })
}

/// Deducts a trading fee from the wallet.
///

/// Credits the trading fee to the protocol fee pool and the admin account.
fn credit_fee_recipient<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    amount: u64,
) -> Result<(), PerpError> {
    if amount == 0 {
        return Ok(());
    }
    // commit-only #23: validate (admin set + credit fits) BEFORE any write. Previously
    // add_market_fee_total wrote before the admin==ZERO reject → a stranded fee-total bump.
    let admin = storage::load_admin(context)?;
    if admin == Address::ZERO {
        return Err(perp_err("placeOrder: fee recipient not initialised"));
    }
    // Validate the credit fits as a READ-ONLY check (mirrors credit_perp's guard) so the fee-total
    // write below can't be stranded by a later overflow — without a load+save owned clone pair.
    {
        let a =
            i64::try_from(amount).map_err(|_| perp_err("perp wallet: amount exceeds i64::MAX"))?;
        storage::load_account_ref(context, admin)?
            .perp_wallet_balance
            .checked_add(a)
            .ok_or_else(|| perp_err("perp wallet: balance overflow"))?;
    }
    // ── APPLY ── (fee-total then account, same order as before). The credit is an in-place mutate
    // (zero-clone on the warm path); it cannot fail now (validated above).
    storage::add_market_fee_total(context, market_id, amount)?;
    // `Order` — see `MatchRegistry::flush`'s pending-credit branch.
    storage::mutate_account_balance(context, admin, AccountUpdateReason::Order, |a| {
        a.credit_perp(amount)
    })?
}

/// Output of [`split_position_fill`]: the closing and opening legs of a maker
/// fill already converted to notional values.
struct PositionFill {
    is_buy: bool,
    closing_qty: u64,
    closing_value: u64,
    opening_qty: u64,
    opening_value: u64,
}

/// Splits a maker fill into closing and opening legs given the maker's current
/// position.
///
/// This is the one-shot equivalent of [`TakerSettlement::record_fill`]: makers
/// are settled per fill so there is no running counter — the current
/// `position_amount` is used directly.
fn split_position_fill(
    position_amount: i64,
    side: Side,
    fill_price: u64,
    fill_qty: u64,
    market: &crate::types::Market,
) -> Result<PositionFill, PerpError> {
    let is_buy = side == Side::Buy;
    let closing_qty = if is_buy && position_amount < 0 {
        fill_qty.min((-position_amount) as u64)
    } else if !is_buy && position_amount > 0 {
        fill_qty.min(position_amount as u64)
    } else {
        0
    };
    let opening_qty = fill_qty - closing_qty;
    // Conservation: single floor of the whole fill, opening derived by subtraction (see the
    // taker path in `finalize`). Both sides MUST attribute the same total quote to the fill
    // (closing + opening == calc_value(price, fill_qty)) or asymmetric fills leak ±1.
    let fill_value = calc_value(
        fill_price,
        fill_qty,
        market.base_decimals,
        market.price_decimals,
    )?;
    let closing_value = calc_value(
        fill_price,
        closing_qty,
        market.base_decimals,
        market.price_decimals,
    )?;
    let opening_value = fill_value
        .checked_sub(closing_value)
        .ok_or_else(|| perp_err("split_position_fill: opening value underflow"))?;

    Ok(PositionFill {
        is_buy,
        closing_qty,
        closing_value,
        opening_qty,
        opening_value,
    })
}

/// Verifies the taker's AVAILABLE balance can cover this fill's cash requirement (opening margin
/// plus the part of the fee margin could not absorb), auto-cancelling same-side open orders (LIFO)
/// to free it if not.
///
/// Runs POST-FLUSH, so storage is authoritative: the registry has already written the fills'
/// positions/accounts and the consumed order entries, hence `Σ ooIM` here is already the POST-fill
/// value.
///
/// # Why the final reject is UNREACHABLE (and therefore raises `perp_invariant_err`)
///
/// Re-verified 2026-08-31 while fixing the val0 leak, because this is the other post-flush reject on
/// the place path and "it is unreachable" was asserted rather than argued. Two legs, matching
/// `finalize_compute`'s two branches:
///
/// 1. **Fast path** (`available >= total_required`, so the cover simulation is skipped): that is
///    literally the predicate the FIRST `taker_margin_is_covered` below re-asks, on the same state
///    and at the same wallet, so it returns early. (It used to be `available >= total_required +
///    rest_delta`, which needed an extra `rest_delta >= 0` step; the rest is no longer gated here at
///    all — `rest_in_book` decides it once, pre-flush.)
/// 2. **Cover path**: `finalize_compute` already ran this LIFO loop, on the same state, with the
///    same predicate (`derived_can_afford(available, total_required)`) and the same per-cancel
///    arithmetic — its `sim` calls `release_open_order_margin_core`, and the real cancel below routes
///    through `release_open_order_margin`, which ends in the same
///    `remove_entry_from_side_aggregates(pos, side, entry.amount, entry.margin_notional(..))`. It
///    raises pre-flush if the loop runs out of orders, so a survivor is covered by construction.
///
/// ⚠️ The two legs above are load-bearing for the commit-only #23 discipline, not commentary: if
/// either ever stops holding, this becomes a genuine second leak of the same shape as the val0 one
/// (the fills are already flushed by the time it raises). Keep them true — or move the cover loop
/// itself in front of the write barrier, which is the resolution `rest_in_book` now uses.
///
/// # What a cancel frees, now that nothing is escrowed
///
/// Under the escrow a cancel credited cash back to the wallet, which is how this loop used to
/// work. It does not any more — a cancel moves no money. It removes the order from `Bid`/`Ask`,
/// which lowers `Σ ooIM`, which RAISES `available = wallet − Σ ooIM`. Same loop, same termination,
/// different mechanism; and it is why the check has to be on the derived available rather than on
/// `perp_wallet_balance` (against which cancelling would achieve nothing, making the loop a
/// pointless order-shredder that then rejected anyway).
///
/// Only same-side orders are cancelled: the opposite side is what NETS against the position the
/// fill just built, so tearing it down could raise the requirement rather than lower it.
fn ensure_taker_wallet_can_cover_margin<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    side: Side,
    required_margin: u64,
    market: &crate::types::Market,
) -> Result<(), PerpError> {
    if required_margin == 0 {
        return Ok(());
    }

    if taker_margin_is_covered(context, user, required_margin)? {
        return Ok(());
    }

    cancel_same_side_orders_until_wallet_covers(
        context,
        user,
        market_id,
        side,
        required_margin,
        market,
    )?;

    if !taker_margin_is_covered(context, user, required_margin)? {
        // INVARIANT, not a user error — and labelled as one now (it used to raise `perp_err`).
        //
        // This sits inside `finalize_apply`, i.e. AFTER `registry.flush`, so it is structurally a
        // write-then-reject site: reaching it leaks the fills exactly the way the val0 leak at block
        // 1,098,719 did. The difference is that this one is provably out of reach (see the two-leg
        // argument on the doc comment above), so the right response to reaching it is "a supposedly
        // exact simulation diverged from the real loop — that is a BUG", not "the user is short of
        // margin". `perp_err` said the latter, which both mislabelled it and hid it from every test
        // that greps for the `[INVARIANT] ` prefix.
        //
        // Genuine insufficiency is refused in the COMPUTE phase, pre-flush and write-clean: the
        // fills gate in `finalize_compute` and its cover-loop `Err` above it. (The REST's own
        // insufficiency is refused by `rest_in_book`, also pre-flush now.)
        return Err(perp_invariant_err(
            "taker margin cover diverged from the compute-phase simulation",
        ));
    }
    Ok(())
}

/// `derived_available >= required_margin`, read from storage. The single predicate both the
/// cover loop and its bracketing checks use, so they cannot drift apart.
fn taker_margin_is_covered<H: PerpHost>(
    context: &mut H,
    user: Address,
    required_margin: u64,
) -> Result<bool, PerpError> {
    let available = crate::margin_view::derived_available_balance(context, user)?;
    Ok(crate::margin_view::derived_can_afford(
        available,
        required_margin as i128,
    ))
}

/// Cancels same-side open orders one at a time (last-placed first) until the AVAILABLE balance
/// covers `required_margin`, or no orders remain.
///
/// LIFO cancellation preserves earlier orders at better price priority.
/// If the account is still short after all orders are exhausted the loop exits
/// silently; the caller is responsible for the final sufficiency check.
fn cancel_same_side_orders_until_wallet_covers<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    side: Side,
    required_margin: u64,
    market: &crate::types::Market,
) -> Result<(), PerpError> {
    while !taker_margin_is_covered(context, user, required_margin)? {
        let order_id = match side {
            Side::Buy => storage::load_buy_orders_ref(context, user, market_id)?
                .back()
                .map(|e| e.order_id),
            Side::Sell => storage::load_sell_orders_ref(context, user, market_id)?
                .back()
                .map(|e| e.order_id),
        };
        let Some(order_id) = order_id else {
            break;
        };

        let order = storage::load_order(context, &order_id)?.ok_or_else(|| {
            perp_invariant_err(format!(
                "open order entry {:?} missing during margin release",
                order_id
            ))
        })?;
        if !matches!(
            order.status,
            OrderStatus::Open | OrderStatus::PartiallyFilled
        ) {
            return Err(perp_invariant_err(format!(
                "open order entry {:?} has terminal status {:?}",
                order_id, order.status
            )));
        }

        execute_order_cancellation(
            context,
            user,
            market_id,
            order_id,
            order,
            market,
            // Runs mid-matching (taker margin-cover): the BBO cache lags the book.
            super::remove_from_book_during_match,
            crate::types::CancelReason::TakerMarginCover,
        )?;
    }

    Ok(())
}

/// Applies the net closing and opening legs of a fill to a position in place.
///
/// # VQuote model
///
/// `v_quote_balance` tracks the signed entry notional of the position:
/// - Long: negative (the position "owes" the notional it was entered at)
/// - Short: positive
///
/// On close, `vq_fraction` (the proportional share being closed) is released.
/// Combined with `margin_release` and `close_quote_delta` (the fill-price
/// value of the closed quantity), this yields the realised PnL:
///
/// ```text
/// realised = margin_release + vq_fraction + close_quote_delta
/// ```
///
/// A positive `realised` is a profit credited to the wallet; negative is a
/// loss debited.  Negative equity (underwater position) is currently absorbed
/// by the wallet via `saturating_sub` — proper bankruptcy handling is a TODO.
///
/// # Margin and vq fractions
///
/// Both `margin_release` and `vq_fraction` are computed from the **remaining**
/// quantity rather than the closing ratio to avoid rounding drift across
/// partial fills:
///
/// ```text
/// remaining_margin = floor(pos.margin * remaining_qty / pos_abs)
/// margin_release   = pos.margin - remaining_margin
/// ```
///
/// This ensures `pos.margin` is always exactly proportional to the remaining
/// position size after each fill.
///
/// # Return value
///
/// Returns the opening margin, bad debt, and gross realized PnL:
/// - `opening_margin` — margin the new position leg was ACTUALLY funded with; the caller
///   deducts exactly this from the wallet. Equal to the requirement `opening_value / L` under
///   [`OpeningMarginFunding::Requirement`], and to `min(requirement, cash at hand)` under
///   [`OpeningMarginFunding::CappedAtCashAtHand`].
/// - `bad_debt` — isolated-margin shortfall: when a close realizes a loss that
///   exceeds the closed slice's collateral, the deficit is drawn from the
///   position's REMAINING margin first; anything still uncovered is `bad_debt`,
///   which the caller routes DIRECTLY to the Insurance Fund. A realized loss is
///   never debited from the wallet or another position's margin.
/// - `realized_pnl` — gross close PnL, excluding released margin, fees, and funding.
pub(super) struct PositionFillOutcome {
    pub(super) opening_margin: u64,
    pub(super) bad_debt: u64,
    pub(super) realized_pnl: i64,
}

/// How much of the opening leg's initial-margin REQUIREMENT this fill is allowed to fund —
/// the M1/M1′ decision of `derived-ooim-plan.md` §3a.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum OpeningMarginFunding {
    /// Fund the requirement IN FULL. For callers that have already gated the draw against
    /// `available` and therefore KNOW the cash is there — the taker path, whose
    /// `finalize_compute` refuses (or covers, then refuses) unless
    /// `derived_can_afford(available, total_required)`. Inert for a pure close
    /// (`opening_qty == 0`), so the ADL legs pass it too.
    Requirement,
    /// Fund `min(requirement, cash at hand)` and let `pos.margin` be SHORT by the remainder —
    /// model **M1**, MEASURED on Binance by R11 (`derived-ooim-plan.md` §3a: silo
    /// `63.10632800 == W0 + realized`, digit-for-digit, against an IM-implied `64.16451380`, i.e.
    /// deliberately `1.05818580` short). Used by the maker fill, which is NOT gated at fill time:
    /// nothing is escrowed at placement and admission was checked once, when the order rested.
    CappedAtCashAtHand,
}

// 8 args: the fill's two legs are four of them, and `funding` (the M1 cap) has to be a parameter
// because the requirement is computed and applied to `pos.margin` in here — a caller cannot cap it
// after the fact without re-deriving it. Same allow as `settle_maker_fill_core` / `adl_fill`.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply_position_fill(
    pos: &mut crate::types::PerpPosition,
    wallet: &mut i64,
    closing_qty: u64,
    closing_value: u64,
    opening_qty: u64,
    opening_value: u64,
    is_buy: bool,
    funding: OpeningMarginFunding,
) -> Result<PositionFillOutcome, PerpError> {
    let mut bad_debt = 0u64;
    let mut realized_pnl = 0i64;
    if closing_qty > 0 {
        let pos_abs = pos.amount.unsigned_abs() as u128;
        let remaining_qty = pos_abs - closing_qty as u128;
        let remaining_margin =
            i64::try_from(pos.margin.max(0) as u128 * remaining_qty / pos_abs)
                .map_err(|_| perp_err("settlement: remaining margin exceeds i64::MAX"))?;
        let margin_release = pos.margin.max(0) - remaining_margin;
        let remaining_vq = pos.v_quote_balance as i128 * remaining_qty as i128 / pos_abs as i128;
        let remaining_vq = i64::try_from(remaining_vq)
            .map_err(|_| perp_err("settlement: remaining vQuote exceeds i64 range"))?;
        let vq_fraction = pos.v_quote_balance - remaining_vq;
        let closing_value = checked_u64_to_i64(closing_value, "settlement: closing value")?;
        let close_quote_delta: i64 = if is_buy {
            -closing_value
        } else {
            closing_value
        };
        realized_pnl = vq_fraction
            .checked_add(close_quote_delta)
            .ok_or_else(|| perp_err("settlement: realized PnL overflow"))?;
        let settlement_credit = margin_release
            .checked_add(realized_pnl)
            .ok_or_else(|| perp_err("settlement: close credit overflow"))?;
        // Release the closed slice's margin out of the position regardless of PnL;
        // `pos.margin` now holds only the remaining (un-closed) margin.
        pos.margin = pos
            .margin
            .checked_sub(margin_release)
            .ok_or_else(|| perp_err("settlement: margin overflow"))?;
        if settlement_credit >= 0 {
            // Solvent close: the closed slice's leftover collateral + profit
            // returns to the wallet.
            *wallet = wallet.saturating_add(settlement_credit);
        } else {
            // Insolvent close: the loss exceeds the closed slice's collateral.
            // ISOLATED MARGIN — do NOT touch the wallet. Draw the deficit from the
            // position's REMAINING margin first; any shortfall beyond the whole
            // position's margin is bad debt, routed to the Insurance Fund by the
            // caller. A realized loss never reaches the wallet or another position.
            let deficit = settlement_credit.unsigned_abs();
            let remaining_margin = pos.margin.max(0) as u64;
            let from_margin = deficit.min(remaining_margin);
            pos.margin = pos
                .margin
                .checked_sub(checked_u64_to_i64(
                    from_margin,
                    "settlement: margin drawdown",
                )?)
                .ok_or_else(|| perp_err("settlement: margin overflow"))?;
            bad_debt = deficit - from_margin;
        }

        if is_buy {
            let closing_qty = checked_u64_to_i64(closing_qty, "settlement: closing quantity")?;
            pos.amount = pos
                .amount
                .checked_add(closing_qty)
                .ok_or_else(|| perp_err("settlement: position amount overflow"))?;
        } else {
            let closing_qty = checked_u64_to_i64(closing_qty, "settlement: closing quantity")?;
            pos.amount = pos
                .amount
                .checked_sub(closing_qty)
                .ok_or_else(|| perp_err("settlement: position amount overflow"))?;
        }
        pos.v_quote_balance = pos
            .v_quote_balance
            .checked_sub(vq_fraction)
            .ok_or_else(|| perp_err("settlement: vQuote overflow"))?;
    }

    let opening_margin = if opening_qty > 0 {
        let requirement = opening_value / pos.leverage.max(1);
        // ── M1: the silo gets all the cash there is, and not a satoshi more ──
        //
        // "Cash at hand" is `*wallet` READ HERE, i.e. after the closing leg above has already
        // credited `margin_release + realized_pnl` into it and before any caller-side debit. That
        // is the right cap and not merely the convenient one:
        //
        // * the close's released margin and its realized PnL are THIS fill's own proceeds and
        //   legitimately fund its opening leg — a flip is one trade, and R11's measured silo is
        //   literally `W0 + realized` (the old silo, released, plus the loss taken on it);
        // * it is pre-fee, which is what makes the fee land where §3a wants it: with
        //   `fee_from_margin = min(fee, opening_margin)` the commission comes out of this capped
        //   opening margin, so the silo is short by the commission too, and NO insurance-fund path
        //   is opened for it. (The wallet only pays a commission the opening margin could not
        //   absorb, which is the one narrow way it can still go negative — bounded by the fee.
        //   Binance's wallet does exactly that, transiently, before `INSURANCE_CLEAR`.)
        // * it is NOT `available = wallet − Σ ooIM`: ooIM is a LIEN, never a debit, and the lien
        //   being consumed here is this very order's. Netting other resting orders' liens out of
        //   the cap would starve the silo of money that has not left.
        //
        // `.max(0)`: an already-negative wallet has no cash to give, so such a fill opens a
        // position with ZERO margin rather than deepening the deficit.
        let initial_margin = match funding {
            OpeningMarginFunding::Requirement => requirement,
            OpeningMarginFunding::CappedAtCashAtHand => requirement.min((*wallet).max(0) as u64),
        };
        let initial_margin_i64 = checked_u64_to_i64(initial_margin, "settlement: initial margin")?;
        let opening_qty_i64 = checked_u64_to_i64(opening_qty, "settlement: opening quantity")?;
        let opening_value_i64 = checked_u64_to_i64(opening_value, "settlement: opening value")?;
        pos.margin = pos
            .margin
            .checked_add(initial_margin_i64)
            .ok_or_else(|| perp_err("settlement: margin overflow"))?;

        if is_buy {
            pos.amount = pos
                .amount
                .checked_add(opening_qty_i64)
                .ok_or_else(|| perp_err("settlement: position amount overflow"))?;
            pos.v_quote_balance = pos
                .v_quote_balance
                .checked_sub(opening_value_i64)
                .ok_or_else(|| perp_err("settlement: vQuote overflow"))?;
        } else {
            pos.amount = pos
                .amount
                .checked_sub(opening_qty_i64)
                .ok_or_else(|| perp_err("settlement: position amount overflow"))?;
            pos.v_quote_balance = pos
                .v_quote_balance
                .checked_add(opening_value_i64)
                .ok_or_else(|| perp_err("settlement: vQuote overflow"))?;
        }
        initial_margin
    } else {
        0
    };

    Ok(PositionFillOutcome {
        opening_margin,
        bad_debt,
        realized_pnl,
    })
}

// ── Order entry updates after fill ───────────────────────────────────────────

/// READ-ONLY plan for shrinking a maker's book entry by `fill_qty`. Returns
/// `(index, post-fill amount, this fill's maker fee)` and touches nothing, so the caller can price
/// and GATE the fill (K9 runs on a position the fee has already left) before committing to the
/// mutation — and so a K9 reject leaves the entry intact for the cancel path to release.
///
/// The fee is the DIFFERENCE between the whole order's fee at its pre- and post-fill amounts,
/// never `calc_trading_fee(fill notional)`: differencing two whole-order fees makes the per-fill
/// fees of a partially-filled order sum EXACTLY to the order's total fee, with no floor-composition
/// drift across partial fills. (Under the old escrow this same quantity was the amount RELEASED
/// from `pos.fee_reserved`; it is unchanged — only its funding source moved.)
#[allow(clippy::too_many_arguments)]
fn entry_fill_plan(
    entries: &std::collections::VecDeque<crate::types::OrderEntry>,
    order_id: &[u8; 32],
    price: u64,
    buy_side: bool,
    fill_qty: u64,
    market: &crate::types::Market,
    side_label: &str,
) -> Result<(usize, u64, u64), PerpError> {
    // #B: binary-search to the maker's price (known = the matched level) then scan the tiny
    // same-price run — O(log n) instead of the O(n) id scan; full-fill removal is O(1)-ish
    // VecDeque::remove instead of the O(n) retain.
    let idx = super::find_entry_by_price_id(entries, order_id, price, buy_side).ok_or_else(|| {
        perp_invariant_err(format!(
            "{side_label} entry for order {order_id:?} not found during fill update"
        ))
    })?;
    let (e_price, e_amount, e_fee_bps) =
        (entries[idx].price, entries[idx].amount, entries[idx].maker_fee_bps);
    if e_amount < fill_qty {
        return Err(perp_invariant_err(format!(
            "{side_label} entry for order {order_id:?} has insufficient amount during fill update"
        )));
    }
    let old_order_fee = calc_maker_fee_for_order_qty_with_bps(e_price, e_amount, e_fee_bps, market)?;
    let new_amount = e_amount.saturating_sub(fill_qty);
    let new_order_fee =
        calc_maker_fee_for_order_qty_with_bps(e_price, new_amount, e_fee_bps, market)?;
    Ok((idx, new_amount, old_order_fee.saturating_sub(new_order_fee)))
}

/// Routes position **bad debt** — a realized loss beyond the position's own margin —
/// directly to the Insurance Fund, WITHOUT touching any wallet (isolated margin).
///
/// The IF balance is reduced by as much of `bad_debt` as it can cover; any uncovered
/// remainder is socialized bad debt (logged via `InsuranceFundDepleted`). Unlike the
/// old `resolve_maker_wallet_deficit`, this does NOT credit a wallet — the loss was
/// already contained to the position's margin in `apply_position_fill`, so the wallet
/// is never involved. Shared by every close path (maker fill, taker fill, liquidation).
pub(super) fn absorb_bad_debt_into_insurance_fund<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    bad_debt: u64,
) -> Result<(), PerpError> {
    if bad_debt == 0 {
        return Ok(());
    }
    let (absorbed, remaining) = storage::absorb_from_insurance_fund(context, bad_debt)?;
    if absorbed > 0 {
        let new_if_balance = storage::load_insurance_fund(context)?;
        let absorbed_i64 = checked_u64_to_i64(absorbed, "settlement: bad-debt IF absorption")?;
        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::InsuranceFundChanged {
                delta: -absorbed_i64,
                newBalance: new_if_balance,
            }
            .to_log_data(),
        });
    }
    if remaining > 0 {
        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::InsuranceFundDepleted {
                marketId: market_id,
                badDebt: remaining,
            }
            .to_log_data(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod isolated_margin_tests {
    use super::{apply_position_fill, OpeningMarginFunding};
    use crate::types::PerpPosition;

    /// Long `amount` units, entry value `-v_quote_balance`, isolated `margin`, leverage 1.
    fn long(amount: i64, v_quote_balance: i64, margin: i64) -> PerpPosition {
        PerpPosition {
            amount,
            v_quote_balance,
            margin,
            leverage: 1,
            ..PerpPosition::default()
        }
    }

    #[test]
    fn solvent_full_close_credits_profit_no_bad_debt() {
        // Long 10 @ entry value 1000, margin 100; close all at exit value 1200 (profit).
        // Gross PnL is -1000 + 1200 = +200; wallet credit also returns margin 100.
        let mut p = long(10, -1000, 100);
        let mut wallet = 0i64;
        let outcome = apply_position_fill(
            &mut p,
            &mut wallet,
            10,
            1200,
            0,
            0,
            false,
            OpeningMarginFunding::Requirement,
        )
        .unwrap();
        assert_eq!(outcome.opening_margin, 0);
        assert_eq!(outcome.bad_debt, 0);
        assert_eq!(outcome.realized_pnl, 200);
        assert_eq!(wallet, 300); // returned margin 100 + realized profit 200
        assert_eq!(p.margin, 0);
        assert_eq!(p.amount, 0);
    }

    #[test]
    fn underwater_full_close_routes_bad_debt_and_leaves_wallet_untouched() {
        // Gross PnL is -1000 + 600 = -400. Margin covers 100; remaining 300 is bad debt.
        let mut p = long(10, -1000, 100);
        let mut wallet = 500i64; // free balance that MUST NOT be touched
        let outcome = apply_position_fill(
            &mut p,
            &mut wallet,
            10,
            600,
            0,
            0,
            false,
            OpeningMarginFunding::Requirement,
        )
        .unwrap();
        assert_eq!(outcome.opening_margin, 0);
        assert_eq!(outcome.bad_debt, 300);
        assert_eq!(outcome.realized_pnl, -400);
        assert_eq!(
            wallet, 500,
            "isolated margin: a position loss never debits the wallet"
        );
        assert_eq!(p.margin, 0);
        assert_eq!(p.amount, 0);
    }

    #[test]
    fn underwater_partial_close_draws_remaining_margin_then_bad_debt() {
        // Close 4 of 10 at exit value 240 (loss). remaining 6.
        // Gross PnL is -400 + 240 = -160; released margin 40 leaves a 120 deficit.
        // remaining margin after release = 60; draw all 60; bad_debt = 60.
        let mut p = long(10, -1000, 100);
        let mut wallet = 500i64;
        let outcome = apply_position_fill(
            &mut p,
            &mut wallet,
            4,
            240,
            0,
            0,
            false,
            OpeningMarginFunding::Requirement,
        )
        .unwrap();
        assert_eq!(outcome.opening_margin, 0);
        assert_eq!(outcome.bad_debt, 60);
        assert_eq!(outcome.realized_pnl, -160);
        assert_eq!(wallet, 500, "wallet untouched");
        assert_eq!(
            p.margin, 0,
            "remaining margin fully drawn down to cover the deficit"
        );
        assert_eq!(p.amount, 6);
        assert_eq!(p.v_quote_balance, -600);
    }

    #[test]
    fn underwater_partial_close_fully_covered_by_remaining_margin() {
        // Close 2 of 10 at exit value 120 (loss). remaining 8.
        // Gross PnL is -200 + 120 = -80; released margin 20 leaves a 60 deficit.
        // remaining margin after release = 80; draw 60; bad_debt 0; 20 margin left.
        let mut p = long(10, -1000, 100);
        let mut wallet = 500i64;
        let outcome = apply_position_fill(
            &mut p,
            &mut wallet,
            2,
            120,
            0,
            0,
            false,
            OpeningMarginFunding::Requirement,
        )
        .unwrap();
        assert_eq!(outcome.opening_margin, 0);
        assert_eq!(outcome.bad_debt, 0);
        assert_eq!(outcome.realized_pnl, -80);
        assert_eq!(wallet, 500, "wallet untouched");
        assert_eq!(
            p.margin, 20,
            "deficit covered by remaining margin; 20 left backing the rest"
        );
        assert_eq!(p.amount, 8);
    }

    #[test]
    fn break_even_close_reports_zero_pnl_and_returns_margin() {
        let mut p = long(10, -1000, 100);
        let mut wallet = 0i64;

        let outcome = apply_position_fill(
            &mut p,
            &mut wallet,
            10,
            1000,
            0,
            0,
            false,
            OpeningMarginFunding::Requirement,
        )
        .unwrap();

        assert_eq!(outcome.realized_pnl, 0);
        assert_eq!(outcome.bad_debt, 0);
        assert_eq!(wallet, 100);
    }
}

#[cfg(test)]
mod split_floor_conservation_tests {
    use super::split_position_fill;
    use crate::{
        math::calc_value,
        types::{MarginTiers, Market, Side},
    };

    fn mkt(base_decimals: u32, price_decimals: u32) -> Market {
        Market {
            market_id: 1,
            base_decimals,
            price_decimals,
            tick_size: 0,
            step_size: 0,
            min_quantity: 0,
            max_quantity: u64::MAX,
            max_price: u64::MAX,
            price_update_interval: 0,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 0,
            mark_price: 0,
            tiers: MarginTiers::default(),
        }
    }

    // Deterministic xorshift (no std rng in the precompile crate).
    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// Split-floor conservation: a fill's closing + opening quote is the SINGLE floor of the
    /// whole fill — `calc_value(price, fill_qty)` — regardless of where the position's
    /// close/open boundary falls. That is exactly what makes the taker and maker (who split
    /// the SAME (price, qty) at DIFFERENT boundaries) attribute the SAME total quote, so the
    /// virtual-quote ledger is conserved across the two counterparties. The pre-fix code
    /// floored the closing and opening legs independently, so this sum was off by 1 whenever
    /// the partition lost a sub-unit — a ±1 phantom mint/burn per asymmetric fill. Fuzzed over
    /// high-decimal markets where truncation is common; this test FAILS on the pre-fix
    /// (double-floor) code and passes on the single-floor-derive-by-subtraction fix.
    #[test]
    fn closing_plus_opening_equals_single_floor_so_both_sides_agree() {
        let mut s: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..50_000 {
            let bd = (next(&mut s) % 9) as u32; // 0..=8 base decimals
            let pd = (next(&mut s) % 9) as u32; // 0..=8 price decimals
            let m = mkt(bd, pd);
            let price = next(&mut s) % 2_000_000 + 1; // non-zero
            let qty = next(&mut s) % 1_000_000 + 1; // non-zero
            let fill_value = calc_value(price, qty, bd, pd).unwrap();

            // Two counterparties splitting the SAME (price, qty) at DIFFERENT boundaries:
            // a long hit by a Sell, and a short hit by a Buy, each of random size in [0, qty].
            let long_amt = (next(&mut s) % (qty + 1)) as i64;
            let short_amt = -((next(&mut s) % (qty + 1)) as i64);
            let sell = split_position_fill(long_amt, Side::Sell, price, qty, &m).unwrap();
            let buy = split_position_fill(short_amt, Side::Buy, price, qty, &m).unwrap();

            // Each side's total attributed quote == the single floor of the whole fill ...
            assert_eq!(sell.closing_value + sell.opening_value, fill_value);
            assert_eq!(buy.closing_value + buy.opening_value, fill_value);
            // ... hence the two counterparties agree EXACTLY — no ±1 phantom between them.
            assert_eq!(
                sell.closing_value + sell.opening_value,
                buy.closing_value + buy.opening_value
            );
            // The derived opening leg never underflows (closing value ≤ whole-fill value).
            assert!(sell.closing_value <= fill_value);
            assert!(buy.closing_value <= fill_value);
        }
    }
}
