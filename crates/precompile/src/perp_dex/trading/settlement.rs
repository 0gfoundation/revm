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
            calc_value, checked_u64_to_i64,
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
    ) -> Result<Self, PrecompileError> {
        let rates = storage::load_user_fee_rates(context, user)?;
        Ok(Self {
            user,
            market_id,
            fills: Vec::new(),
            taker_fee_bps: rates.taker_fee_bps,
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
            let fill_closing_value = calc_value(
                fill.price,
                fill_closing_qty,
                market.base_decimals,
                market.price_decimals,
            )?;
            let fill_opening_value = calc_value(
                fill.price,
                fill_opening_qty,
                market.base_decimals,
                market.price_decimals,
            )?;
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

        let opening_margin_required = apply_position_fill(
            &mut pos,
            &mut account.perp_wallet_balance,
            closing_qty,
            closing_value,
            opening_qty,
            opening_value,
            is_buy,
        )?;

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
pub(super) fn settle_maker_fill<CTX: ContextTr>(
    context: &mut CTX,
    maker: Address,
    maker_order_id: &[u8; 32],
    market_id: u64,
    fill_price: u64,
    fill_qty: u64,
    taker_side: Side,
    market: &crate::perp_dex::types::Market,
) -> Result<MakerFillResult, PrecompileError> {
    let maker_side = taker_side.opposite();
    let mut pos = storage::load_position(context, maker, market_id)?;
    let mut account = storage::load_account(context, maker)?;
    // Settle accrued funding on the maker's pre-fill position before its size changes.
    crate::perp_dex::funding::settle_position_funding(
        context,
        maker,
        market,
        &mut pos,
        &mut account.perp_wallet_balance,
    )?;
    // Snapshot before mutations — used to verify and release the pre-fill
    // reservation. MUST be the flip-aware reservation (pos.margin_reserved), the
    // same quantity new_reserved is recomputed as below: comparing a flip-aware
    // new_reserved against a max-of-side old_reserved would mismatch the two ends
    // of the deficit test and make it fire spuriously.
    let old_reserved = pos.margin_reserved;

    let maker_fee = reduce_maker_order_entry_for_fill(
        context,
        maker,
        market_id,
        maker_side,
        maker_order_id,
        fill_qty,
        market,
    )?;

    let fill = split_position_fill(pos.amount, maker_side, fill_price, fill_qty, market)?;
    let opening_margin = apply_position_fill(
        &mut pos,
        &mut account.perp_wallet_balance,
        fill.closing_qty,
        fill.closing_value,
        fill.opening_qty,
        fill.opening_value,
        fill.is_buy,
    )?;

    // Must run after apply_position_fill so pos.amount reflects the new size;
    // cross-side netting depends on the updated position.
    let new_reserved =
        recompute_maker_order_reserve_after_fill(context, maker, market_id, &mut pos, market)?;

    pos.fee_reserved = pos.fee_reserved.saturating_sub(maker_fee);

    // opening_margin ≤ old_reserved is guaranteed; this is how much of old_reserved
    // is free to cover new_reserved after pos.margin is funded.
    let max_sustainable_reserved = old_reserved.saturating_sub(opening_margin);

    if new_reserved > max_sustainable_reserved {
        // Deficit: position sign-flip shifted cross-side netting so remaining
        // orders cost more MR than was pre-paid. Debit the full deficit first,
        // allowing a temporary negative wallet; ordinary cancellations can then
        // release MR against a fully funded reservation model.
        let reserve_deficit = new_reserved - max_sustainable_reserved;
        account.debit_perp(reserve_deficit)?;

        storage::save_position(context, maker, market_id, &pos)?;
        storage::save_account(context, maker, account)?;

        let expired_order_ids =
            cancel_maker_orders_until_wallet_nonnegative(context, maker, market_id, market)?;
        resolve_maker_wallet_deficit(context, maker, market_id)?;
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

        return Ok(MakerFillResult {
            maker_fee,
            expired_order_ids,
        });
    } else {
        let net_release = old_reserved
            .saturating_sub(new_reserved)
            .saturating_sub(opening_margin);
        account.credit_perp(net_release)?;
        storage::save_position(context, maker, market_id, &pos)?;
        storage::save_account(context, maker, account)?;
    }

    resolve_maker_wallet_deficit(context, maker, market_id)?;
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

    Ok(MakerFillResult {
        maker_fee,
        expired_order_ids: Vec::new(),
    })
}

pub(super) struct MakerFillResult {
    pub maker_fee: u64,
    pub expired_order_ids: Vec<[u8; 32]>,
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
    storage::add_market_fee_total(context, market_id, amount)?;
    let admin = storage::load_admin(context)?;
    if admin == Address::ZERO {
        return Err(perp_err("placeOrder: fee recipient not initialised"));
    }
    let mut account = storage::load_account(context, admin)?;
    account.credit_perp(amount)?;
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
    let closing_value = calc_value(
        fill_price,
        closing_qty,
        market.base_decimals,
        market.price_decimals,
    )?;
    let opening_value = calc_value(
        fill_price,
        opening_qty,
        market.base_decimals,
        market.price_decimals,
    )?;

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

    if storage::load_account(context, user)?.has_available_perp(required_margin) {
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

    if !storage::load_account(context, user)?.has_available_perp(required_margin) {
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
    while !storage::load_account(context, user)?.has_available_perp(required_margin) {
        let order_id = match side {
            Side::Buy => storage::load_buy_orders(context, user, market_id)?
                .last()
                .map(|e| e.order_id),
            Side::Sell => storage::load_sell_orders(context, user, market_id)?
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
/// Returns the margin required to open the new position leg.  The caller is
/// responsible for deducting this from the wallet.
fn apply_position_fill(
    pos: &mut crate::perp_dex::types::PerpPosition,
    wallet: &mut i64,
    closing_qty: u64,
    closing_value: u64,
    opening_qty: u64,
    opening_value: u64,
    is_buy: bool,
) -> Result<u64, PrecompileError> {
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
        if realised > 0 {
            *wallet = wallet.saturating_add(realised);
        } else if realised < 0 {
            // Wallet absorbs the loss; if it goes negative the caller is responsible
            // for routing the deficit to the Insurance Fund.
            *wallet = wallet.saturating_sub(-realised);
        }
        pos.margin = pos
            .margin
            .checked_sub(margin_release)
            .ok_or_else(|| perp_err("settlement: margin overflow"))?;

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

    Ok(opening_margin)
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
        Side::Buy => {
            let mut entries = storage::load_buy_orders(context, user, market_id)?;
            let released = match entries.iter_mut().find(|e| &e.order_id == order_id) {
                Some(e) => {
                    if e.amount < fill_qty {
                        return Err(perp_invariant_err(format!(
                            "buy entry for order {:?} has insufficient amount during fill update",
                            order_id
                        )));
                    }
                    let old_order_fee = calc_maker_fee_for_order_qty_with_bps(
                        e.price,
                        e.amount,
                        e.maker_fee_bps,
                        market,
                    )?;
                    e.amount = e.amount.saturating_sub(fill_qty);
                    let new_order_fee = calc_maker_fee_for_order_qty_with_bps(
                        e.price,
                        e.amount,
                        e.maker_fee_bps,
                        market,
                    )?;
                    let fee_released = old_order_fee.saturating_sub(new_order_fee);
                    if e.amount == 0 {
                        entries.retain(|e| &e.order_id != order_id);
                    }
                    fee_released
                }
                None => {
                    return Err(perp_invariant_err(format!(
                        "buy entry for order {:?} not found during fill update",
                        order_id
                    )))
                }
            };
            storage::save_buy_orders(context, user, market_id, &entries)?;
            Ok(released)
        }
        Side::Sell => {
            let mut entries = storage::load_sell_orders(context, user, market_id)?;
            let released = match entries.iter_mut().find(|e| &e.order_id == order_id) {
                Some(e) => {
                    if e.amount < fill_qty {
                        return Err(perp_invariant_err(format!(
                            "sell entry for order {:?} has insufficient amount during fill update",
                            order_id
                        )));
                    }
                    let old_order_fee = calc_maker_fee_for_order_qty_with_bps(
                        e.price,
                        e.amount,
                        e.maker_fee_bps,
                        market,
                    )?;
                    e.amount = e.amount.saturating_sub(fill_qty);
                    let new_order_fee = calc_maker_fee_for_order_qty_with_bps(
                        e.price,
                        e.amount,
                        e.maker_fee_bps,
                        market,
                    )?;
                    let fee_released = old_order_fee.saturating_sub(new_order_fee);
                    if e.amount == 0 {
                        entries.retain(|e| &e.order_id != order_id);
                    }
                    fee_released
                }
                None => {
                    return Err(perp_invariant_err(format!(
                        "sell entry for order {:?} not found during fill update",
                        order_id
                    )))
                }
            };
            storage::save_sell_orders(context, user, market_id, &entries)?;
            Ok(released)
        }
    }
}

/// Cancels the maker's open orders (dominant side first, LIFO) until
/// `pos.margin_reserved ≤ max_reserved` or all orders are gone.
///
/// Called when a maker fill creates a deficit — the remaining orders require
/// more MR than was pre-paid after funding the new position.  Cancelling
/// orders frees MR and credits the wallet, restoring the conservation invariant
/// without touching `pos.margin`.
fn cancel_maker_orders_until_wallet_nonnegative<CTX: ContextTr>(
    context: &mut CTX,
    maker: Address,
    market_id: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<Vec<[u8; 32]>, PrecompileError> {
    let mut expired_order_ids = Vec::new();
    while storage::load_account(context, maker)?.perp_wallet_balance < 0 {
        let pos = storage::load_position(context, maker, market_id)?;
        let prefer_buy = pos.buy_side_margin_reserved >= pos.sell_side_margin_reserved;
        let buy_entries = storage::load_buy_orders(context, maker, market_id)?;
        let sell_entries = storage::load_sell_orders(context, maker, market_id)?;
        let order_id = if prefer_buy {
            buy_entries
                .last()
                .map(|e| e.order_id)
                .or_else(|| sell_entries.last().map(|e| e.order_id))
        } else {
            sell_entries
                .last()
                .map(|e| e.order_id)
                .or_else(|| buy_entries.last().map(|e| e.order_id))
        };
        let Some(order_id) = order_id else { break };
        let order = storage::load_order(context, &order_id)?.ok_or_else(|| {
            perp_invariant_err(format!(
                "maker order {:?} missing during deficit resolution",
                order_id
            ))
        })?;
        execute_order_cancellation(
            context,
            maker,
            market_id,
            order_id,
            order,
            OrderStatus::Expired,
            market,
            // Runs inside match_order's sweep (maker auto-cancel-for-deficit): the
            // BBO cache is deliberately stale until the post-sweep refresh.
            super::remove_from_book_during_match,
        )?;
        expired_order_ids.push(order_id);
    }
    Ok(expired_order_ids)
}

/// Absorbs any remaining wallet deficit from the Insurance Fund.
///
/// Called after all order-cancellation steps are exhausted. The IF covers as much
/// of the shortfall as possible; anything beyond its capacity becomes bad debt
/// (wallet clamped to 0, shortfall written off by the protocol).
fn resolve_maker_wallet_deficit<CTX: ContextTr>(
    context: &mut CTX,
    maker: Address,
    market_id: u64,
) -> Result<(), PrecompileError> {
    let mut account = storage::load_account(context, maker)?;
    if account.perp_wallet_balance >= 0 {
        return Ok(());
    }
    let deficit = (-account.perp_wallet_balance) as u64;
    let (absorbed, remaining) = storage::absorb_from_insurance_fund(context, deficit)?;
    let new_if_balance = storage::load_insurance_fund(context)?;
    account.credit_perp(absorbed)?;
    if remaining > 0 {
        account.perp_wallet_balance = 0;
    }
    storage::save_account(context, maker, account)?;
    if absorbed > 0 {
        let absorbed_i64 = checked_u64_to_i64(absorbed, "settlement: IF absorption delta")?;
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
    let buy_entries = storage::load_buy_orders(context, user, market_id)?;
    let sell_entries = storage::load_sell_orders(context, user, market_id)?;
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
