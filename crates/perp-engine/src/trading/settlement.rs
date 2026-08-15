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
    types::{OrderStatus, Side},
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
        });
        Ok(())
    }

    /// Commits all accumulated fills to storage in a single atomic sequence.
    ///
    /// # Settlement order
    ///
    /// 1. **Apply position fill** — mutates `pos` and credits realised PnL from
    ///    the closing leg to the wallet.
    /// 2. **Recompute margin_reserved** — `pos.amount` changed; cross-side
    ///    netting for the taker's remaining open orders must be updated so that
    ///    subsequent order placement sees accurate available margin.
    /// 3. **Compute fee** — based solely on traded notional.
    /// 4. **Save** — persists all position and PnL changes in one write so that
    ///    the next step reads the correct wallet balance from storage.
    /// 5. **Ensure wallet covers opening margin + fee** — if short, same-side
    ///    open orders are auto-cancelled (LIFO) to free reserved margin.
    ///    `release_margin_for_cancelled_order` saves its own pos updates, so
    ///    only account needs to be reloaded after this step.
    /// 6. **Deduct opening margin and fee from wallet** — both deducted cleanly
    ///    from the wallet; position margin is never touched for fee payment.
    /// 7. **Emit log** — single `PositionChanged` event for the full order.
    pub(super) fn finalize_compute<H: PerpHost>(
        self,
        context: &mut H,
        reg: &mut MatchRegistry,
        taker_side: Side,
        market: &crate::types::Market,
        // commit-only #23 (atomic-reject, Harry 2026-07-20): when the caller will REST the taker's
        // remainder (GTC), the resting order's margin must be affordable from the post-fill wallet
        // TOO — otherwise the fills would commit and the subsequent rest_in_book would revert,
        // leaking the fills. Validated here, pre-flush, so an unaffordable fills+rest order rejects
        // atomically with zero writes (matching the pre-commit-only whole-order revert).
        rest: Option<RestReq>,
    ) -> Result<Option<TakerPlan>, PerpError> {
        // An EMPTY fill set must NOT skip the rest validation. `match_order` flushes the registry
        // immediately after this call, and the walk records writes even when nothing filled (a
        // `SaveLevel` for every level it entered, plus the `DeleteOrder`/`OrderCancelled`/
        // `RemovePrice`/`SaveBest` of a maker the K9 guard rejected). So a rest that `rest_in_book`
        // would later refuse has to be refused HERE, pre-flush — otherwise those writes commit
        // under an order the caller then rejects (a leak in the single-order path, a spurious
        // `Aborted` in a batch). NOTHING fill-specific runs on this path: no fee, no position
        // change, no registry join — only the rest's affordability is measured, and the affordable
        // case still returns `Ok(None)` (there is no fill to plan).
        if self.fills.is_empty() {
            let Some(r) = &rest else {
                return Ok(None);
            };
            // Measure against the state `rest_in_book` will see AFTER the flush: the taker's
            // registry working copy if the walk already touched it (a self-match maker the K9 guard
            // cancelled — the flush writes exactly that copy, funding included), else storage.
            let affordable = match reg.user_work(self.user) {
                Some(w) => rest_is_affordable(
                    &w.pos,
                    &w.account,
                    &w.buy_entries,
                    &w.sell_entries,
                    taker_side,
                    r,
                    market,
                )?,
                None => {
                    let pos = storage::load_position_ref(context, self.user, self.market_id)?;
                    let account = storage::load_account_ref(context, self.user)?;
                    let buy = storage::load_buy_orders_ref(context, self.user, self.market_id)?;
                    let sell = storage::load_sell_orders_ref(context, self.user, self.market_id)?;
                    rest_is_affordable(&pos, &account, &buy, &sell, taker_side, r, market)?
                }
            };
            if !affordable {
                return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
            }
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
            &w.buy_entries,
            &w.sell_entries,
            &self.fills,
            taker_side,
            mark,
            self.taker_fee_bps,
            market,
        )?;

        // commit-only #23 perf (Level 2): the resting remainder's reservation delta is computed
        // CLONE-FREE — temporarily insert the entry into the taker's REAL list, measure, then
        // remove it (Vec insert+remove at the same index is an exact identity restore), cloning
        // only an O(1) PerpPosition. The list is left pristine for flush.
        let rest_delta = match &rest {
            Some(r) => {
                let entry = crate::types::OrderEntry {
                    order_id: [0u8; 32],
                    price: r.price,
                    amount: r.qty,
                    maker_fee_bps: r.maker_fee_bps,
                };
                let old_mr = w.pos.margin_reserved;
                let idx = match taker_side {
                    Side::Buy => {
                        let i = w.buy_entries.partition_point(|e| e.price > r.price);
                        w.buy_entries.insert(i, entry);
                        i
                    }
                    Side::Sell => {
                        let i = w.sell_entries.partition_point(|e| e.price < r.price);
                        w.sell_entries.insert(i, entry);
                        i
                    }
                };
                let res = crate::math::calc_reservation_notionals_it(
                    w.buy_entries.iter().copied(),
                    w.sell_entries.iter().copied(),
                    market.base_decimals,
                    market.price_decimals,
                    w.pos.amount,
                );
                // Restore the list BEFORE propagating any error, so w stays pristine for flush.
                match taker_side {
                    Side::Buy => {
                        w.buy_entries.remove(idx);
                    }
                    Side::Sell => {
                        w.sell_entries.remove(idx);
                    }
                }
                let (bn, sn, cn) = res?;
                let mut tp = w.pos.clone();
                tp.set_reservations(bn, sn, cn, w.pos.leverage);
                let margin_delta = tp.margin_reserved.saturating_sub(old_mr);
                let order_fee =
                    calc_maker_fee_for_order_qty_with_bps(r.price, r.qty, r.maker_fee_bps, market)?;
                margin_delta
                    .checked_add(order_fee)
                    .ok_or_else(|| perp_err("placeOrder: reserve delta overflow"))?
            }
            None => 0,
        };
        let need = core
            .total_required
            .checked_add(rest_delta)
            .ok_or_else(|| perp_err("placeOrder: fills+rest requirement overflow"))?;

        // LEVEL 1 fast path (the common case): the taker's wallet already covers fills + rest with
        // NO same-side cancels → produce the plan with ZERO order-list clones. Correct because
        // has_available(total_required + rest_delta) implies no cover is needed AND the post-fill
        // leftover (wallet − total_required) ≥ rest_delta, so finalize_apply's real cover loop does
        // nothing and rest_in_book's check passes. Only a genuinely tight taker falls to the cover
        // simulation below.
        if !w.account.has_available_perp(need) {
            // Cover needed (rare): simulate the LIFO same-side cancels on clones, reusing
            // release_margin_core so the sim cannot diverge from finalize_apply's real loop. Rest
            // feasibility is re-checked on the POST-cover sim list (cover shrinks the taker side,
            // changing the rest reservation — so rest_delta above, computed pre-cover, is only used
            // for the fast-path check; the cover branch re-derives it post-cover).
            let mut sim_pos = w.pos.clone();
            let mut sim_account = w.account.clone();
            let mut sim_buy = w.buy_entries.clone();
            let mut sim_sell = w.sell_entries.clone();
            while !sim_account.has_available_perp(core.total_required) {
                let next = match taker_side {
                    Side::Buy => sim_buy.back().map(|e| e.order_id),
                    Side::Sell => sim_sell.back().map(|e| e.order_id),
                };
                let Some(oid) = next else {
                    return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
                };
                super::release_margin_core(
                    &mut sim_pos,
                    &mut sim_account,
                    &mut sim_buy,
                    &mut sim_sell,
                    taker_side,
                    &oid,
                    market,
                )?;
            }
            sim_account.debit_perp(core.total_required)?;
            if let Some(r) = &rest {
                let new_entry = crate::types::OrderEntry {
                    order_id: [0u8; 32],
                    price: r.price,
                    amount: r.qty,
                    maker_fee_bps: r.maker_fee_bps,
                };
                match taker_side {
                    Side::Buy => {
                        let i = sim_buy.partition_point(|e| e.price > r.price);
                        sim_buy.insert(i, new_entry);
                    }
                    Side::Sell => {
                        let i = sim_sell.partition_point(|e| e.price < r.price);
                        sim_sell.insert(i, new_entry);
                    }
                }
                let (bn, sn, cn) = crate::math::calc_reservation_notionals_it(
                    sim_buy.iter().copied(),
                    sim_sell.iter().copied(),
                    market.base_decimals,
                    market.price_decimals,
                    sim_pos.amount,
                )?;
                let old_reserved = sim_pos.margin_reserved;
                sim_pos.set_reservations(bn, sn, cn, sim_pos.leverage);
                let margin_delta = sim_pos.margin_reserved.saturating_sub(old_reserved);
                let order_fee =
                    calc_maker_fee_for_order_qty_with_bps(r.price, r.qty, r.maker_fee_bps, market)?;
                let delta = margin_delta
                    .checked_add(order_fee)
                    .ok_or_else(|| perp_err("placeOrder: reserve delta overflow"))?;
                if !sim_account.has_available_perp(delta) {
                    return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
                }
            }
        }

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
        }))
    }
}

/// The taker's intent to rest its unmatched remainder (commit-only #23 atomic-reject): the
/// resting order's price, quantity, and the taker's maker-fee bps — enough for [`finalize_compute`]
/// to pre-validate the rest's margin against the post-fill wallet.
pub(super) struct RestReq {
    pub(super) price: u64,
    pub(super) qty: u64,
    pub(super) maker_fee_bps: u64,
}

/// Can `rest` be funded from this (pos, account, order-list) state? The zero-fill arm of
/// [`TakerSettlement::finalize_compute`] uses this to raise the rest-margin reject BEFORE the
/// registry flush, so the walk's writes never commit under an order that is about to be refused.
///
/// The formula is `rest_in_book`'s own, evaluated CLONE-FREE over the hypothetical
/// "list ⊕ rest entry at its sorted slot" (`calc_reservation_notionals_it` folds the chained
/// iterator, byte-identically to inserting first): flip-aware `margin_reserved` delta + the
/// order's reserved maker fee, checked against the available wallet. Being the same formula on the
/// same state is what makes the pre-flush reject sound — `rest_in_book`'s later check cannot then
/// fire post-write.
fn rest_is_affordable(
    pos: &crate::types::PerpPosition,
    account: &crate::types::UserAccount,
    buy_entries: &std::collections::VecDeque<crate::types::OrderEntry>,
    sell_entries: &std::collections::VecDeque<crate::types::OrderEntry>,
    taker_side: Side,
    rest: &RestReq,
    market: &crate::types::Market,
) -> Result<bool, PerpError> {
    let entry = crate::types::OrderEntry {
        order_id: [0u8; 32],
        price: rest.price,
        amount: rest.qty,
        maker_fee_bps: rest.maker_fee_bps,
    };
    let (bd, pd) = (market.base_decimals, market.price_decimals);
    let (bn, sn, cn) = match taker_side {
        Side::Buy => {
            let i = buy_entries.partition_point(|e| e.price > rest.price);
            crate::math::calc_reservation_notionals_it(
                buy_entries
                    .range(..i)
                    .copied()
                    .chain(core::iter::once(entry))
                    .chain(buy_entries.range(i..).copied()),
                sell_entries.iter().copied(),
                bd,
                pd,
                pos.amount,
            )?
        }
        Side::Sell => {
            let i = sell_entries.partition_point(|e| e.price < rest.price);
            crate::math::calc_reservation_notionals_it(
                buy_entries.iter().copied(),
                sell_entries
                    .range(..i)
                    .copied()
                    .chain(core::iter::once(entry))
                    .chain(sell_entries.range(i..).copied()),
                bd,
                pd,
                pos.amount,
            )?
        }
    };
    let mut probe = pos.clone();
    probe.set_reservations(bn, sn, cn, pos.leverage);
    let margin_delta = probe.margin_reserved.saturating_sub(pos.margin_reserved);
    let order_fee =
        calc_maker_fee_for_order_qty_with_bps(rest.price, rest.qty, rest.maker_fee_bps, market)?;
    let delta = margin_delta
        .checked_add(order_fee)
        .ok_or_else(|| perp_err("placeOrder: reserve delta overflow"))?;
    Ok(account.has_available_perp(delta))
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
    storage::mutate_account_balance(context, plan.user, |a| a.debit_perp(plan.total_required))??;
    credit_fee_recipient(context, plan.market_id, plan.fee)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user: plan.user,
            marketId: plan.market_id,
            amount: plan.pos_log.amount,
            vQuoteBalance: plan.pos_log.v_quote_balance,
            margin: plan.pos_log.margin,
            leverage: plan.pos_log.leverage,
            realizedPnl: plan.realized_pnl,
            closedQuantity: plan.closed_quantity,
        }
        .to_log_data(),
    });

    Ok(())
}

/// Outcome of attempting to settle one maker fill. A maker's trading fee is pre-reserved and
/// released from `pos.fee_reserved`, not charged from the wallet.
pub(super) enum MakerFillOutcome {
    /// The fill was applied; carries the maker's trading fee.
    Filled { maker_fee: u64 },
    /// Filling this maker would have opened/increased its position below the maintenance-margin
    /// threshold at the current mark (K9). The fill was NOT applied; the caller must cancel the
    /// maker order. Funding accrued on the maker's position IS settled and persisted (it is owed
    /// regardless of the fill, and may already have touched the Insurance Fund inline).
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

    /// The user's working copy **if they already joined this match** — never loads, never inserts.
    /// The flush writes exactly these copies, so for a user in the registry this IS the post-flush
    /// state (funding already folded in by [`Self::get_or_load`]); that is what the zero-fill rest
    /// pre-check must measure against on a self-match.
    fn user_work(&self, user: Address) -> Option<&UserWork> {
        self.users.iter().find(|(a, _)| *a == user).map(|(_, w)| w)
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
        let pending = crate::funding::compute_funding_settlement(
            context,
            user,
            market,
            &mut pos,
            &mut account.perp_wallet_balance,
        )?;
        if let Some(p) = pending {
            self.events.push(MatchEvent::ApplyFunding(p));
        }
        let buy_entries = storage::load_buy_orders(context, user, market_id)?;
        let sell_entries = storage::load_sell_orders(context, user, market_id)?;
        // Read-through: fees credited to the admin before they joined the registry are pending;
        // fold them into the copy so this user's wallet matches the old per-fill storage writes.
        if self.fee_admin == Some(user) && self.admin_credit_pending > 0 {
            account.credit_perp(self.admin_credit_pending)?;
            self.admin_credit_pending = 0;
        }
        self.users.push((
            user,
            UserWork {
                pos,
                account,
                buy_entries,
                sell_entries,
                dirty_buy: false,
                dirty_sell: false,
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
    pub(super) fn flush<H: PerpHost>(
        mut self,
        context: &mut H,
        market_id: u64,
    ) -> Result<(), PerpError> {
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
                        }
                        .to_log_data(),
                    });
                }
                MatchEvent::SaveOrder { order_id, order } => {
                    storage::save_order(context, &order_id, &order)?;
                }
                MatchEvent::DeleteOrder { order_id } => {
                    storage::delete_order(context, &order_id)?;
                }
                MatchEvent::OrderCancelled { user, order_id } => {
                    context.log(Log {
                        address: PERP_DEX_ADDRESS,
                        data: IPerpDex::OrderCancelled {
                            user,
                            orderId: primitives::FixedBytes(order_id),
                            marketId: market_id,
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
            storage::mutate_account_balance(context, admin, |a| a.credit_perp(self.admin_credit_pending))??;
        }
        // #A: base/price decimals for the reservation-aggregate recompute below (load once).
        let (bd, pd) = {
            let m = storage::load_market_ref(context, market_id)?
                .ok_or_else(|| perp_invariant_err("match flush: unknown market"))?;
            (m.base_decimals, m.price_decimals)
        };
        for (user, mut w) in self.users {
            if w.dirty_buy {
                storage::save_buy_orders(context, user, market_id, &w.buy_entries)?;
            }
            if w.dirty_sell {
                storage::save_sell_orders(context, user, market_id, &w.sell_entries)?;
            }
            // #A: the match may have filled/cancelled maker & taker orders — resync the maintained
            // reservation aggregates from the authoritative working-copy lists (recompute, not
            // incremental: the match path is rare and already re-serialises the whole list here).
            let (tbq, tbn) =
                crate::math::sum_side_totals(w.buy_entries.iter().copied(), bd, pd)?;
            let (tsq, tsn) =
                crate::math::sum_side_totals(w.sell_entries.iter().copied(), bd, pd)?;
            w.pos.total_buy_qty = tbq;
            w.pos.total_buy_notional = tbn;
            w.pos.total_sell_qty = tsq;
            w.pos.total_sell_notional = tsn;
            storage::save_position(context, user, market_id, &w.pos)?;
            storage::save_account(context, user, w.account)?;
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

    reg.push_event(MatchEvent::PositionChanged {
        user: maker,
        pos: pos_snapshot,
        realized_pnl,
        closed_quantity,
    });

    Ok(MakerFillOutcome::Filled { maker_fee })
}

/// Registry-backed K9 maker cancel: releases the rejected order's margin on the registry copies
/// (flushed later) and writes the order status + OrderCancelled log immediately (same positions
/// as today's cancel_rejected_maker).
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
    super::release_margin_core(
        &mut w.pos,
        &mut w.account,
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
/// (conservation-exact close/open split), position fill, K9 open-into-insolvency guard, flip-aware
/// reserve recompute + MR reconciliation, fee + total-required — over in-memory copies only, NO
/// storage access. Any `Err` (K9 reject, checked arithmetic) fires before the caller has written
/// anything. The entry lists are read-only here (the taker's lists are only mutated by the
/// wallet-cover cancels, which remain in the storage wrapper).
#[allow(clippy::too_many_arguments)]
fn finalize_core(
    pos: &mut crate::types::PerpPosition,
    account: &mut crate::types::UserAccount,
    buy_entries: &std::collections::VecDeque<crate::types::OrderEntry>,
    sell_entries: &std::collections::VecDeque<crate::types::OrderEntry>,
    fills: &[RecordedFill],
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

    for fill in fills {
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
    }

    let fill_outcome = apply_position_fill(
        pos,
        &mut account.perp_wallet_balance,
        closing_qty,
        closing_value,
        opening_qty,
        opening_value,
        is_buy,
    )?;

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

    // pos.amount changed; recompute margin_reserved so cross-side netting for the taker's
    // remaining open orders reflects the new position size. MR delta reconciles with the wallet
    // via mr_credit/mr_extra; without this, W + M + MR is not conserved across the fill.
    let old_mr = pos.margin_reserved;
    let (buy_notional, sell_notional, c_notional) =
        crate::math::calc_reservation_notionals_it(
            buy_entries.iter().copied(),
            sell_entries.iter().copied(),
        market.base_decimals,
        market.price_decimals,
        pos.amount,
    )?;
    pos.set_reservations(buy_notional, sell_notional, c_notional, pos.leverage);
    let new_mr = pos.margin_reserved;
    let mr_credit = old_mr.saturating_sub(new_mr); // MR decreased: freed margin back to wallet
    let mr_extra = new_mr.saturating_sub(old_mr); // MR increased: wallet must cover the gap
    account.credit_perp(mr_credit)?;

    let fee_notional = closing_value
        .checked_add(opening_value)
        .ok_or_else(|| perp_err("placeOrder: taker fee notional overflow"))?;
    let fee = calc_trading_fee(fee_notional, taker_fee_bps)?;
    let total_required = fill_outcome
        .opening_margin
        .checked_add(fee)
        .ok_or_else(|| perp_err("placeOrder: opening margin + fee overflow"))?
        .checked_add(mr_extra)
        .ok_or_else(|| perp_err("placeOrder: total required overflow"))?;

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
    // Snapshot before mutations — used to verify and release the pre-fill reservation. MUST be
    // the flip-aware reservation (pos.margin_reserved), the same quantity new_reserved is
    // recomputed as below.
    let old_reserved = pos.margin_reserved;

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
    )?;

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

    let (entries, label) = match maker_side {
        Side::Buy => (&mut *buy_entries, "buy"),
        Side::Sell => (&mut *sell_entries, "sell"),
    };
    // #B: fill_price == the resting maker's order price (a match executes at the maker's level), so
    // it is exactly the sort key reduce_order_entry_core binary-searches on.
    let maker_fee = reduce_order_entry_core(
        entries,
        maker_order_id,
        fill_price,
        matches!(maker_side, Side::Buy),
        fill_qty,
        market,
        label,
    )?;

    // Flip-aware reserve recompute (must follow the entry reduce + reflect the new pos.amount).
    let (buy_notional, sell_notional, c_notional) =
        crate::math::calc_reservation_notionals_it(
            buy_entries.iter().copied(),
            sell_entries.iter().copied(),
        market.base_decimals,
        market.price_decimals,
        pos.amount,
    )?;
    pos.set_reservations(buy_notional, sell_notional, c_notional, pos.leverage);
    let new_reserved = pos.margin_reserved;

    pos.fee_reserved = pos.fee_reserved.saturating_sub(maker_fee);

    // opening_margin ≤ old_reserved is guaranteed; this is how much of old_reserved is free to
    // cover new_reserved after pos.margin is funded.
    let max_sustainable_reserved = old_reserved.saturating_sub(fill_outcome.opening_margin);
    if new_reserved > max_sustainable_reserved {
        // Reserve-deficit (≤1-unit floor-rounding residual post formula-C): clamp the stored
        // reservation to what is actually backed so the cancel-release stays exact.
        pos.margin_reserved = max_sustainable_reserved;
    } else {
        let net_release = old_reserved
            .saturating_sub(new_reserved)
            .saturating_sub(fill_outcome.opening_margin);
        account.credit_perp(net_release)?;
    }

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
    storage::mutate_account_balance(context, admin, |a| a.credit_perp(amount))?
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

/// Verifies the taker's wallet can cover the opening margin requirement,
/// auto-cancelling same-side open orders (LIFO) to free reserved margin if not.
///
/// Only same-side orders are cancelled: opposite-side orders rely on their own
/// reserved margin for netting and cannot be safely freed here without
/// invalidating that accounting.
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

    if storage::load_account_ref(context, user)?.has_available_perp(required_margin) {
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

    if !storage::load_account_ref(context, user)?.has_available_perp(required_margin) {
        return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
    }
    Ok(())
}

/// Cancels same-side open orders one at a time (last-placed first) until the
/// wallet covers `required_margin`, or no orders remain.
///
/// LIFO cancellation preserves earlier orders at better price priority.
/// If the wallet is still short after all orders are exhausted the loop exits
/// silently; the caller is responsible for the final sufficiency check.
fn cancel_same_side_orders_until_wallet_covers<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    side: Side,
    required_margin: u64,
    market: &crate::types::Market,
) -> Result<(), PerpError> {
    while !storage::load_account_ref(context, user)?.has_available_perp(required_margin) {
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
/// - `opening_margin` — margin required to open the new position leg; the caller
///   deducts it from the wallet (maker: from reserved MR; taker: from the wallet).
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

pub(super) fn apply_position_fill(
    pos: &mut crate::types::PerpPosition,
    wallet: &mut i64,
    closing_qty: u64,
    closing_value: u64,
    opening_qty: u64,
    opening_value: u64,
    is_buy: bool,
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
        let initial_margin = opening_value / pos.leverage.max(1);
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

/// PURE core of [`reduce_maker_order_entry_for_fill`] (commit-only #23, tranche-4 step 1):
/// operates on an in-memory entry list only — no storage access — so the match compute phase can
/// run it on working copies and the storage wrapper above runs it in the journal overlay. Shrinks
/// the entry by `fill_qty`, removes it at zero, returns the released fee reservation.
#[allow(clippy::too_many_arguments)]
pub(super) fn reduce_order_entry_core(
    entries: &mut std::collections::VecDeque<crate::types::OrderEntry>,
    order_id: &[u8; 32],
    price: u64,
    buy_side: bool,
    fill_qty: u64,
    market: &crate::types::Market,
    side_label: &str,
) -> Result<u64, PerpError> {
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
    let fee_released = old_order_fee.saturating_sub(new_order_fee);
    if new_amount == 0 {
        entries.remove(idx);
    } else {
        entries[idx].amount = new_amount;
    }
    Ok(fee_released)
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

/// Recomputes the maker's order margin reservation from scratch after a fill.
///
/// A full recomputation (rather than an incremental update) avoids accumulated
/// rounding error across many partial fills.  The result is written back into
/// `pos` and also returned as `pos.margin_reserved`.
///
/// Cross-side netting: `margin_reserved` is the flip-aware worst-case
/// `max(S + B', B + S')` (see [`calc_reservation_notionals`]) — a long position
/// offsets sell-order exposure (and vice versa), but a fill that flips the
/// position re-prices the opposite side's opening leg, so the reservation must
/// cover the peak across that flip, not merely the larger side today. This must
/// be called **after** [`apply_position_fill`] so that `pos.amount` already
/// reflects the new size.
fn recompute_maker_order_reserve_after_fill<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    pos: &mut crate::types::PerpPosition,
    market: &crate::types::Market,
) -> Result<u64, PerpError> {
    let buy_entries = storage::load_buy_orders_ref(context, user, market_id)?;
    let sell_entries = storage::load_sell_orders_ref(context, user, market_id)?;
    let (buy_notional, sell_notional, c_notional) =
        crate::math::calc_reservation_notionals_it(
            buy_entries.iter().copied(),
            sell_entries.iter().copied(),
        market.base_decimals,
        market.price_decimals,
        pos.amount,
    )?;
    pos.set_reservations(buy_notional, sell_notional, c_notional, pos.leverage);
    Ok(pos.margin_reserved)
}

#[cfg(test)]
mod isolated_margin_tests {
    use super::apply_position_fill;
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
        let outcome = apply_position_fill(&mut p, &mut wallet, 10, 1200, 0, 0, false).unwrap();
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
        let outcome = apply_position_fill(&mut p, &mut wallet, 10, 600, 0, 0, false).unwrap();
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
        let outcome = apply_position_fill(&mut p, &mut wallet, 4, 240, 0, 0, false).unwrap();
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
        let outcome = apply_position_fill(&mut p, &mut wallet, 2, 120, 0, 0, false).unwrap();
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

        let outcome = apply_position_fill(&mut p, &mut wallet, 10, 1000, 0, 0, false).unwrap();

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
