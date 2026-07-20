use alloy_primitives::IntoLogData;
use context::{ContextTr, JournalTr};
use primitives::{Address, Log};

use super::execute_order_cancellation;
use crate::{
    perp_dex::{
        errors::{perp_err, perp_invariant_err},
        interface::IPerpDex,
        math::{
            calc_maker_fee_for_order_qty_with_bps, calc_reservation_notionals, calc_trading_fee,
            calc_value, checked_u64_to_i64, is_above_maintenance_margin,
        },
        storage,
        types::{OrderStatus, Side},
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
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
    pub(super) fn load<CTX: ContextTr>(
        context: &mut CTX,
        user: Address,
        market_id: u64,
        waive_taker_fee: bool,
    ) -> Result<Self, PrecompileError> {
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
        _market: &crate::perp_dex::types::Market,
    ) -> Result<(), PrecompileError> {
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
    pub(super) fn finalize<CTX: ContextTr>(
        self,
        context: &mut CTX,
        taker_side: Side,
        market: &crate::perp_dex::types::Market,
    ) -> Result<(), PrecompileError> {
        if self.fills.is_empty() {
            return Ok(());
        }

        let mut pos = storage::load_position(context, self.user, self.market_id)?;
        let mut account = storage::load_account(context, self.user)?;
        // Settle accrued funding on the pre-fill position before its size changes.
        crate::perp_dex::funding::settle_position_funding(
            context,
            self.user,
            market,
            &mut pos,
            &mut account.perp_wallet_balance,
        )?;
        let mut remaining_closing_qty = pos.amount.unsigned_abs();
        let mut closing_qty = 0u64;
        let mut closing_value = 0u64;
        let mut opening_qty = 0u64;
        let mut opening_value = 0u64;
        let is_buy = taker_side == Side::Buy;

        for fill in &self.fills {
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
            // Conservation: floor the WHOLE matched quantity ONCE and derive the opening
            // leg by subtraction, so the taker and maker attribute the SAME total quote to
            // this fill (closing + opening == calc_value(price, fill.quantity)). Flooring the
            // closing and opening legs independently lets the two parties' different
            // close/open split boundaries floor to a different sum → a ±1 phantom mint/burn
            // per asymmetric fill (Σ v_quote no longer conserved).
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

        let (opening_margin_required, bad_debt) = apply_position_fill(
            &mut pos,
            &mut account.perp_wallet_balance,
            closing_qty,
            closing_value,
            opening_qty,
            opening_value,
            is_buy,
        )?;

        // Open-into-insolvency guard (K9): a taker may not open/increase a
        // position that is already below maintenance margin at the current mark.
        // Together with the placement band this blocks manufacturing an insolvent
        // position (the insurance-fund mint). Returning Err reverts the whole
        // placeOrder tx. Skipped when mark is unset (0 — save_market fixtures;
        // addMarket-created markets always have a mark). Closing/reducing is never
        // gated: its realized loss beyond margin is legitimate bad debt (below).
        if opening_qty > 0 {
            let mark = storage::load_mark_price(context, self.market_id)?;
            if mark > 0
                && !is_above_maintenance_margin(
                    mark,
                    pos.amount,
                    pos.v_quote_balance,
                    pos.margin,
                    market.base_decimals,
                    market.price_decimals,
                )?
            {
                return Err(perp_err("placeOrder: open would breach maintenance margin"));
            }
        }

        // Isolated margin: a realized loss beyond the position's own margin is bad
        // debt routed DIRECTLY to the Insurance Fund — never the taker's wallet.
        // apply_position_fill already contained the loss within pos.margin.
        absorb_bad_debt_into_insurance_fund(context, self.market_id, bad_debt)?;

        // pos.amount changed; recompute margin_reserved so cross-side netting
        // for the taker's remaining open orders reflects the new position size.
        // MR delta is reconciled with wallet via mr_credit/mr_extra below;
        // without this, W + M + MR is not conserved across the fill.
        let old_mr = pos.margin_reserved;
        recompute_maker_order_reserve_after_fill(
            context,
            self.user,
            self.market_id,
            &mut pos,
            market,
        )?;
        let new_mr = pos.margin_reserved;
        let mr_credit = old_mr.saturating_sub(new_mr); // MR decreased: freed margin back to wallet
        let mr_extra = new_mr.saturating_sub(old_mr); // MR increased: wallet must cover the gap
        account.credit_perp(mr_credit)?;

        let fee_notional = closing_value
            .checked_add(opening_value)
            .ok_or_else(|| perp_err("placeOrder: taker fee notional overflow"))?;
        let fee = calc_trading_fee(fee_notional, self.taker_fee_bps)?;
        let total_required = opening_margin_required
            .checked_add(fee)
            .ok_or_else(|| perp_err("placeOrder: opening margin + fee overflow"))?
            .checked_add(mr_extra)
            .ok_or_else(|| perp_err("placeOrder: total required overflow"))?;

        // Single save covers all position mutations (apply_position_fill +
        // recompute) and the PnL credit to wallet.
        storage::save_position(context, self.user, self.market_id, &pos)?;
        storage::save_account(context, self.user, account)?;

        ensure_taker_wallet_can_cover_margin(
            context,
            self.user,
            self.market_id,
            taker_side,
            total_required,
            market,
        )?;

        // Reload account only: cancellations may have changed the wallet balance.
        // pos is already correct in storage — release_margin_for_cancelled_order
        // saves its own pos updates, and the log fields (amount/margin/v_quote)
        // are not touched by cancellations.
        account = storage::load_account(context, self.user)?;
        account.debit_perp(total_required)?;
        storage::save_account(context, self.user, account)?;
        credit_fee_recipient(context, self.market_id, fee)?;

        context.journal_mut().log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::PositionChanged {
                user: self.user,
                marketId: self.market_id,
                amount: pos.amount,
                vQuoteBalance: pos.v_quote_balance,
                margin: pos.margin,
                leverage: pos.leverage,
            }
            .to_log_data(),
        });

        Ok(())
    }
}

/// Settles one fill event on the maker side.
///
/// Unlike the taker path, makers are settled one fill at a time — there is no
/// accumulator because each maker order is a distinct price level that the
/// matcher processes independently.
///
/// # Margin accounting
///
/// The maker pre-reserved margin (`old_reserved`) is captured **before** any
/// mutation so it reflects the full pre-fill reservation.  After the fill:
/// - the order entry shrinks (or disappears),
/// - the position changes size,
/// - `new_reserved` is recomputed from scratch (not incrementally) to avoid
///   drift, and
/// - `opening_margin` covers any new position exposure.
///
/// `old_reserved` distributes across three destinations after the fill:
///
/// ```text
/// old_reserved = opening_margin + new_reserved + net_release
/// ```
///
/// - `opening_margin` → transitions from MR into `pos.margin`.  Always
///   covered: `opening_margin ≤ fill_margin ≤ side_margin ≤ old_reserved`.
/// - `new_reserved`   → stays locked in MR for remaining open orders.
/// - `net_release`    → returned to wallet (≥ 0 in the common case).
///
/// When a fill **crosses the position sign** (long → short or vice versa),
/// cross-side netting shifts and `new_reserved` can exceed
/// `old_reserved − opening_margin` (deficit).  In that case the maker's open
/// orders are auto-cancelled (dominant side first, LIFO) until MR fits within
/// what was pre-paid — mirroring the taker's `ensure` logic.
///
/// A maker's trading fee is pre-reserved and released from `pos.fee_reserved`,
/// not charged from the wallet.
/// Outcome of attempting to settle one maker fill.
pub(super) enum MakerFillOutcome {
    /// The fill was applied; carries the maker's trading fee.
    Filled { maker_fee: u64 },
    /// Filling this maker would have opened/increased its position below the
    /// maintenance-margin threshold at the current mark (K9). The fill was NOT
    /// applied; the caller must cancel the maker order. Funding accrued on the
    /// maker's position IS settled and persisted (it is owed regardless of the
    /// fill, and may already have touched the Insurance Fund inline).
    RejectedInsolvent,
}

pub(super) fn settle_maker_fill<CTX: ContextTr>(
    context: &mut CTX,
    maker: Address,
    maker_order_id: &[u8; 32],
    market_id: u64,
    fill_price: u64,
    fill_qty: u64,
    taker_side: Side,
    market: &crate::perp_dex::types::Market,
) -> Result<MakerFillOutcome, PrecompileError> {
    let maker_side = taker_side.opposite();
    let mut pos = storage::load_position(context, maker, market_id)?;
    let mut account = storage::load_account(context, maker)?;
    // Funding is owed independent of the fill outcome, so it persists on BOTH the filled and
    // rejected paths — computed in memory here (commit-only #23), applied below.
    let pending_funding = crate::perp_dex::funding::compute_funding_settlement(
        context,
        maker,
        market,
        &mut pos,
        &mut account.perp_wallet_balance,
    )?;
    let mut buy_entries = storage::load_buy_orders(context, maker, market_id)?;
    let mut sell_entries = storage::load_sell_orders(context, maker, market_id)?;
    let mark = storage::load_mark_price(context, market_id)?;

    let core = settle_maker_fill_core(
        &mut pos,
        &mut account,
        &mut buy_entries,
        &mut sell_entries,
        mark,
        maker_side,
        maker_order_id,
        fill_price,
        fill_qty,
        market,
    )?;

    let (maker_fee, bad_debt) = match core {
        MakerFillCore::RejectedInsolvent => {
            // Reject: persist ONLY the funding-settled (pre-fill) position — the fill is
            // skipped and the caller cancels the order. No entry reduce, no bad-debt absorb.
            if let Some(p) = pending_funding {
                crate::perp_dex::funding::apply_funding_settlement(context, p)?;
            }
            storage::save_position(context, maker, market_id, &pos)?;
            storage::save_account(context, maker, account)?;
            return Ok(MakerFillOutcome::RejectedInsolvent);
        }
        MakerFillCore::Filled {
            maker_fee,
            bad_debt,
        } => (maker_fee, bad_debt),
    };

    // Accept path — same write/log sequence as before the core extraction: funding (IF + logs),
    // maker-side entry list, bad-debt absorb (IF + logs), pos/account, fee credit, PositionChanged.
    if let Some(p) = pending_funding {
        crate::perp_dex::funding::apply_funding_settlement(context, p)?;
    }
    match maker_side {
        Side::Buy => storage::save_buy_orders(context, maker, market_id, &buy_entries)?,
        Side::Sell => storage::save_sell_orders(context, maker, market_id, &sell_entries)?,
    }
    absorb_bad_debt_into_insurance_fund(context, market_id, bad_debt)?;
    storage::save_position(context, maker, market_id, &pos)?;
    storage::save_account(context, maker, account)?;
    credit_fee_recipient(context, market_id, maker_fee)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user: maker,
            marketId: market_id,
            amount: pos.amount,
            vQuoteBalance: pos.v_quote_balance,
            margin: pos.margin,
            leverage: pos.leverage,
        }
        .to_log_data(),
    });

    // No order is auto-cancelled on a maker fill any more (isolated margin), so the
    // maker fill returns only the maker fee — no expired-order bookkeeping.
    Ok(MakerFillOutcome::Filled { maker_fee })
}

/// Outcome of [`settle_maker_fill_core`]: the pure maker-fill decision + its effect summary.
pub(super) enum MakerFillCore {
    Filled { maker_fee: u64, bad_debt: u64 },
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
    pos: &mut crate::perp_dex::types::PerpPosition,
    account: &mut crate::perp_dex::types::UserAccount,
    buy_entries: &mut Vec<crate::perp_dex::types::OrderEntry>,
    sell_entries: &mut Vec<crate::perp_dex::types::OrderEntry>,
    mark_price: u64,
    maker_side: Side,
    maker_order_id: &[u8; 32],
    fill_price: u64,
    fill_qty: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<MakerFillCore, PrecompileError> {
    // Snapshot before mutations — used to verify and release the pre-fill reservation. MUST be
    // the flip-aware reservation (pos.margin_reserved), the same quantity new_reserved is
    // recomputed as below.
    let old_reserved = pos.margin_reserved;

    // Compute the fill on a TRIAL clone first (single computation — adopted verbatim on accept).
    let fill = split_position_fill(pos.amount, maker_side, fill_price, fill_qty, market)?;
    let mut trial_pos = pos.clone();
    let mut trial_wallet = account.perp_wallet_balance;
    let (opening_margin, bad_debt) = apply_position_fill(
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

    // Accept: adopt the trial result verbatim.
    *pos = trial_pos;
    account.perp_wallet_balance = trial_wallet;

    let (entries, label) = match maker_side {
        Side::Buy => (&mut *buy_entries, "buy"),
        Side::Sell => (&mut *sell_entries, "sell"),
    };
    let maker_fee = reduce_order_entry_core(entries, maker_order_id, fill_qty, market, label)?;

    // Flip-aware reserve recompute (must follow the entry reduce + reflect the new pos.amount).
    let (buy_notional, sell_notional, c_notional) = calc_reservation_notionals(
        buy_entries,
        sell_entries,
        market.base_decimals,
        market.price_decimals,
        pos.amount,
    )?;
    pos.set_reservations(buy_notional, sell_notional, c_notional, pos.leverage);
    let new_reserved = pos.margin_reserved;

    pos.fee_reserved = pos.fee_reserved.saturating_sub(maker_fee);

    // opening_margin ≤ old_reserved is guaranteed; this is how much of old_reserved is free to
    // cover new_reserved after pos.margin is funded.
    let max_sustainable_reserved = old_reserved.saturating_sub(opening_margin);
    if new_reserved > max_sustainable_reserved {
        // Reserve-deficit (≤1-unit floor-rounding residual post formula-C): clamp the stored
        // reservation to what is actually backed so the cancel-release stays exact.
        pos.margin_reserved = max_sustainable_reserved;
    } else {
        let net_release = old_reserved
            .saturating_sub(new_reserved)
            .saturating_sub(opening_margin);
        account.credit_perp(net_release)?;
    }

    Ok(MakerFillCore::Filled {
        maker_fee,
        bad_debt,
    })
}

/// Deducts a trading fee from the wallet.
///

/// Credits the trading fee to the protocol fee pool and the admin account.
fn credit_fee_recipient<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    amount: u64,
) -> Result<(), PrecompileError> {
    if amount == 0 {
        return Ok(());
    }
    // commit-only #23: validate (admin set + credit fits) BEFORE any write. Previously
    // add_market_fee_total wrote before the admin==ZERO reject → a stranded fee-total bump.
    let admin = storage::load_admin(context)?;
    if admin == Address::ZERO {
        return Err(perp_err("placeOrder: fee recipient not initialised"));
    }
    let mut account = storage::load_account(context, admin)?;
    account.credit_perp(amount)?;
    // ── APPLY ── (fee-total then account, same order as before)
    storage::add_market_fee_total(context, market_id, amount)?;
    storage::save_account(context, admin, account)
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
    market: &crate::perp_dex::types::Market,
) -> Result<PositionFill, PrecompileError> {
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
fn ensure_taker_wallet_can_cover_margin<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    side: Side,
    required_margin: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
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
fn cancel_same_side_orders_until_wallet_covers<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    side: Side,
    required_margin: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    while !storage::load_account_ref(context, user)?.has_available_perp(required_margin) {
        let order_id = match side {
            Side::Buy => storage::load_buy_orders_ref(context, user, market_id)?
                .last()
                .map(|e| e.order_id),
            Side::Sell => storage::load_sell_orders_ref(context, user, market_id)?
                .last()
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
            OrderStatus::Expired,
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
/// Returns `(opening_margin, bad_debt)`:
/// - `opening_margin` — margin required to open the new position leg; the caller
///   deducts it from the wallet (maker: from reserved MR; taker: from the wallet).
/// - `bad_debt` — isolated-margin shortfall: when a close realizes a loss that
///   exceeds the closed slice's collateral, the deficit is drawn from the
///   position's REMAINING margin first; anything still uncovered is `bad_debt`,
///   which the caller routes DIRECTLY to the Insurance Fund. A realized loss is
///   never debited from the wallet or another position's margin.
fn apply_position_fill(
    pos: &mut crate::perp_dex::types::PerpPosition,
    wallet: &mut i64,
    closing_qty: u64,
    closing_value: u64,
    opening_qty: u64,
    opening_value: u64,
    is_buy: bool,
) -> Result<(u64, u64), PrecompileError> {
    let mut bad_debt = 0u64;
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
        let realised = margin_release
            .checked_add(vq_fraction)
            .and_then(|v| v.checked_add(close_quote_delta))
            .ok_or_else(|| perp_err("settlement: realised PnL overflow"))?;
        // Release the closed slice's margin out of the position regardless of PnL;
        // `pos.margin` now holds only the remaining (un-closed) margin.
        pos.margin = pos
            .margin
            .checked_sub(margin_release)
            .ok_or_else(|| perp_err("settlement: margin overflow"))?;
        if realised >= 0 {
            // Solvent close: the closed slice's leftover collateral + profit
            // returns to the wallet.
            *wallet = wallet.saturating_add(realised);
        } else {
            // Insolvent close: the loss exceeds the closed slice's collateral.
            // ISOLATED MARGIN — do NOT touch the wallet. Draw the deficit from the
            // position's REMAINING margin first; any shortfall beyond the whole
            // position's margin is bad debt, routed to the Insurance Fund by the
            // caller. A realized loss never reaches the wallet or another position.
            let deficit = realised.unsigned_abs();
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

    Ok((opening_margin, bad_debt))
}

// ── Order entry updates after fill ───────────────────────────────────────────

/// Shrinks the maker's order entry by `fill_qty` and returns the fee that was
/// pre-reserved for the filled portion (old fee − new fee).
///
/// The fee delta is computed as a difference of two `calc_trading_fee` calls
/// rather than a direct proportion so that rounding is consistent with how the
/// fee was originally reserved at order placement.
fn reduce_maker_order_entry_for_fill<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    side: Side,
    order_id: &[u8; 32],
    fill_qty: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<u64, PrecompileError> {
    match side {
        // #21 靶子2: update the maker entry's remaining amount IN PLACE (no load/store clone).
        Side::Buy => storage::mutate_buy_orders(context, user, market_id, |entries| {
            reduce_order_entry_core(entries, order_id, fill_qty, market, "buy")
        })?,
        Side::Sell => storage::mutate_sell_orders(context, user, market_id, |entries| {
            reduce_order_entry_core(entries, order_id, fill_qty, market, "sell")
        })?,
    }
}

/// PURE core of [`reduce_maker_order_entry_for_fill`] (commit-only #23, tranche-4 step 1):
/// operates on an in-memory entry list only — no storage access — so the match compute phase can
/// run it on working copies and the storage wrapper above runs it in the journal overlay. Shrinks
/// the entry by `fill_qty`, removes it at zero, returns the released fee reservation.
pub(super) fn reduce_order_entry_core(
    entries: &mut Vec<crate::perp_dex::types::OrderEntry>,
    order_id: &[u8; 32],
    fill_qty: u64,
    market: &crate::perp_dex::types::Market,
    side_label: &str,
) -> Result<u64, PrecompileError> {
    match entries.iter_mut().find(|e| &e.order_id == order_id) {
        Some(e) => {
            if e.amount < fill_qty {
                return Err(perp_invariant_err(format!(
                    "{side_label} entry for order {order_id:?} has insufficient amount during fill update"
                )));
            }
            let old_order_fee =
                calc_maker_fee_for_order_qty_with_bps(e.price, e.amount, e.maker_fee_bps, market)?;
            e.amount = e.amount.saturating_sub(fill_qty);
            let new_order_fee =
                calc_maker_fee_for_order_qty_with_bps(e.price, e.amount, e.maker_fee_bps, market)?;
            let fee_released = old_order_fee.saturating_sub(new_order_fee);
            if e.amount == 0 {
                entries.retain(|e| &e.order_id != order_id);
            }
            Ok(fee_released)
        }
        None => Err(perp_invariant_err(format!(
            "{side_label} entry for order {order_id:?} not found during fill update"
        ))),
    }
}

/// Routes position **bad debt** — a realized loss beyond the position's own margin —
/// directly to the Insurance Fund, WITHOUT touching any wallet (isolated margin).
///
/// The IF balance is reduced by as much of `bad_debt` as it can cover; any uncovered
/// remainder is socialized bad debt (logged via `InsuranceFundDepleted`). Unlike the
/// old `resolve_maker_wallet_deficit`, this does NOT credit a wallet — the loss was
/// already contained to the position's margin in `apply_position_fill`, so the wallet
/// is never involved. Shared by every close path (maker fill, taker fill, liquidation).
pub(super) fn absorb_bad_debt_into_insurance_fund<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    bad_debt: u64,
) -> Result<(), PrecompileError> {
    if bad_debt == 0 {
        return Ok(());
    }
    let (absorbed, remaining) = storage::absorb_from_insurance_fund(context, bad_debt)?;
    if absorbed > 0 {
        let new_if_balance = storage::load_insurance_fund(context)?;
        let absorbed_i64 = checked_u64_to_i64(absorbed, "settlement: bad-debt IF absorption")?;
        context.journal_mut().log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::InsuranceFundChanged {
                delta: -absorbed_i64,
                newBalance: new_if_balance,
            }
            .to_log_data(),
        });
    }
    if remaining > 0 {
        context.journal_mut().log(Log {
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
fn recompute_maker_order_reserve_after_fill<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    pos: &mut crate::perp_dex::types::PerpPosition,
    market: &crate::perp_dex::types::Market,
) -> Result<u64, PrecompileError> {
    let buy_entries = storage::load_buy_orders_ref(context, user, market_id)?;
    let sell_entries = storage::load_sell_orders_ref(context, user, market_id)?;
    let (buy_notional, sell_notional, c_notional) = calc_reservation_notionals(
        &buy_entries,
        &sell_entries,
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
    use crate::perp_dex::types::PerpPosition;

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
        // realised = margin_release(100) + vq_fraction(-1000) + close(+1200) = +300.
        let mut p = long(10, -1000, 100);
        let mut wallet = 0i64;
        let (opening, bad_debt) =
            apply_position_fill(&mut p, &mut wallet, 10, 1200, 0, 0, false).unwrap();
        assert_eq!((opening, bad_debt), (0, 0));
        assert_eq!(wallet, 300); // returned margin 100 + realized profit 200
        assert_eq!(p.margin, 0);
        assert_eq!(p.amount, 0);
    }

    #[test]
    fn underwater_full_close_routes_bad_debt_and_leaves_wallet_untouched() {
        // Close all at exit value 600 (loss): realised = 100 - 1000 + 600 = -300.
        // Full close → no remaining margin → entire 300 deficit is bad debt → IF.
        let mut p = long(10, -1000, 100);
        let mut wallet = 500i64; // free balance that MUST NOT be touched
        let (opening, bad_debt) =
            apply_position_fill(&mut p, &mut wallet, 10, 600, 0, 0, false).unwrap();
        assert_eq!((opening, bad_debt), (0, 300));
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
        // margin_release=40, vq_fraction=-400, realised=40-400+240=-120 → deficit 120.
        // remaining margin after release = 60; draw all 60; bad_debt = 60.
        let mut p = long(10, -1000, 100);
        let mut wallet = 500i64;
        let (opening, bad_debt) =
            apply_position_fill(&mut p, &mut wallet, 4, 240, 0, 0, false).unwrap();
        assert_eq!((opening, bad_debt), (0, 60));
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
        // margin_release=20, vq_fraction=-200, realised=20-200+120=-60 → deficit 60.
        // remaining margin after release = 80; draw 60; bad_debt 0; 20 margin left.
        let mut p = long(10, -1000, 100);
        let mut wallet = 500i64;
        let (opening, bad_debt) =
            apply_position_fill(&mut p, &mut wallet, 2, 120, 0, 0, false).unwrap();
        assert_eq!((opening, bad_debt), (0, 0));
        assert_eq!(wallet, 500, "wallet untouched");
        assert_eq!(
            p.margin, 20,
            "deficit covered by remaining margin; 20 left backing the rest"
        );
        assert_eq!(p.amount, 8);
    }
}

#[cfg(test)]
mod split_floor_conservation_tests {
    use super::split_position_fill;
    use crate::perp_dex::{
        math::calc_value,
        types::{Market, Side},
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
