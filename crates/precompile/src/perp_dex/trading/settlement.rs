use alloy_primitives::IntoLogData;
use context::{ContextTr, JournalTr};
use primitives::{Address, FixedBytes, Log};

use super::{release_margin_for_cancelled_order, remove_from_book};
use crate::{
    perp_dex::{
        errors::{perp_err, perp_invariant_err},
        interface::IPerpDex,
        math::{
            calc_buy_side_reserved_notional, calc_sell_side_reserved_notional, calc_trading_fee,
            calc_value,
        },
        storage,
        types::{OrderStatus, Side},
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};

fn calc_maker_fee_for_order_qty_with_bps(
    price: u64,
    qty: u64,
    maker_fee_bps: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<u64, PrecompileError> {
    let notional = calc_value(price, qty, market.base_decimals, market.price_decimals)?;
    calc_trading_fee(notional, maker_fee_bps)
}

// ── Position settlement ───────────────────────────────────────────────────────

pub(super) struct TakerSettlement {
    user: Address,
    market_id: u64,
    pos: crate::perp_dex::types::PerpPosition,
    account: crate::perp_dex::types::UserAccount,
    opening_margin_required: u64,
    taker_fee_bps: u64,
    fee: u64,
    has_fill: bool,
}

impl TakerSettlement {
    pub(super) fn load<CTX: ContextTr>(
        context: &mut CTX,
        user: Address,
        market_id: u64,
    ) -> Result<Self, PrecompileError> {
        let rates = storage::load_user_fee_rates(context, user)?;
        Ok(Self {
            user,
            market_id,
            pos: storage::load_position(context, user, market_id)?,
            account: storage::load_account(context, user)?,
            opening_margin_required: 0,
            taker_fee_bps: rates.taker_fee_bps,
            fee: 0,
            has_fill: false,
        })
    }

    pub(super) fn record_fill(
        &mut self,
        fill_price: u64,
        fill_qty: u64,
        taker_side: Side,
        market: &crate::perp_dex::types::Market,
    ) -> Result<(), PrecompileError> {
        self.has_fill = true;
        let is_buy = taker_side == Side::Buy;
        let fill_value = calc_value(
            fill_price,
            fill_qty,
            market.base_decimals,
            market.price_decimals,
        )?;
        self.fee = self
            .fee
            .checked_add(calc_trading_fee(fill_value, self.taker_fee_bps)?)
            .ok_or_else(|| perp_err("placeOrder: taker fee overflow"))?;

        let closing_qty = calc_closing_qty(&self.pos, fill_qty, is_buy);
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

        apply_taker_closing_fill_to_position(
            &mut self.pos,
            &mut self.account.perp_wallet_balance,
            closing_qty,
            closing_value,
            is_buy,
        )?;

        if opening_qty > 0 {
            apply_taker_opening_fill_to_position(
                &mut self.pos,
                &mut self.account.perp_wallet_balance,
                opening_qty,
                opening_value,
                is_buy,
                false,
            )?;
            self.opening_margin_required = self
                .opening_margin_required
                .checked_add(opening_value / self.pos.leverage.max(1))
                .ok_or_else(|| perp_err("placeOrder: opening margin overflow"))?;
        }

        Ok(())
    }

    pub(super) fn finalize<CTX: ContextTr>(
        mut self,
        context: &mut CTX,
        taker_side: Side,
        market: &crate::perp_dex::types::Market,
    ) -> Result<(), PrecompileError> {
        if !self.has_fill {
            return Ok(());
        }

        storage::save_position(context, self.user, self.market_id, &self.pos)?;
        storage::save_account(context, self.user, self.account)?;

        ensure_taker_wallet_can_cover_margin(
            context,
            self.user,
            self.market_id,
            taker_side,
            self.opening_margin_required,
            market,
        )?;

        self.pos = storage::load_position(context, self.user, self.market_id)?;
        self.account = storage::load_account(context, self.user)?;
        if self.account.perp_wallet_balance < self.opening_margin_required {
            return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
        }
        self.account.perp_wallet_balance -= self.opening_margin_required;

        charge_trading_fee(
            &mut self.pos,
            &mut self.account.perp_wallet_balance,
            self.fee,
        )?;
        storage::save_position(context, self.user, self.market_id, &self.pos)?;
        storage::save_account(context, self.user, self.account)?;
        credit_fee_recipient(context, self.fee)?;

        context.journal_mut().log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::PositionChanged {
                user: self.user,
                marketId: self.market_id,
                amount: self.pos.amount,
                vQuoteBalance: self.pos.v_quote_balance,
                margin: self.pos.margin,
                leverage: self.pos.leverage,
            }
            .to_log_data(),
        });

        Ok(())
    }
}

pub(super) fn settle_maker_fill<CTX: ContextTr>(
    context: &mut CTX,
    taker: Address,
    maker: Address,
    taker_order_id: &[u8; 32],
    maker_order_id: &[u8; 32],
    market_id: u64,
    fill_price: u64,
    fill_qty: u64,
    taker_side: Side,
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let fill_value = calc_value(
        fill_price,
        fill_qty,
        market.base_decimals,
        market.price_decimals,
    )?;
    let maker_rates = storage::load_user_fee_rates(context, maker)?;
    let maker_fee = calc_trading_fee(fill_value, maker_rates.maker_fee_bps)?;
    let maker_side = taker_side.opposite();
    let released_margin = release_margin_for_maker_fill(
        context,
        maker,
        market_id,
        maker_side,
        maker_order_id,
        fill_qty,
        maker_fee,
        market,
    )?;
    let mut pos = storage::load_position(context, maker, market_id)?;
    let mut account = storage::load_account(context, maker)?;
    apply_maker_fill_to_position(
        &mut pos,
        &mut account.perp_wallet_balance,
        fill_qty,
        fill_value,
        maker_side == Side::Buy,
        released_margin.margin_available,
        released_margin.wallet_freed,
    )?;
    storage::save_position(context, maker, market_id, &pos)?;
    storage::save_account(context, maker, account)?;
    credit_fee_recipient(context, maker_fee)?;

    let trade_id = storage::next_trade_id(context, market_id)?;
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::Trade {
            marketId: market_id,
            tradeId: trade_id,
            takerOrderId: FixedBytes(*taker_order_id),
            makerOrderId: FixedBytes(*maker_order_id),
            taker,
            maker,
            price: fill_price,
            quantity: fill_qty,
            takerSide: taker_side as u8,
        }
        .to_log_data(),
    });

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

    Ok(())
}

fn charge_trading_fee(
    pos: &mut crate::perp_dex::types::PerpPosition,
    wallet: &mut u64,
    fee: u64,
) -> Result<(), PrecompileError> {
    if fee == 0 {
        return Ok(());
    }
    if *wallet >= fee {
        *wallet -= fee;
        return Ok(());
    }

    let remainder = fee - *wallet;
    *wallet = 0;
    if pos.margin >= remainder as i64 {
        pos.margin -= remainder as i64;
        Ok(())
    } else {
        Err(perp_err("placeOrder: insufficient balance for trading fee"))
    }
}

fn credit_fee_recipient<CTX: ContextTr>(
    context: &mut CTX,
    amount: u64,
) -> Result<(), PrecompileError> {
    if amount == 0 {
        return Ok(());
    }
    let admin = storage::load_admin(context)?;
    if admin == Address::ZERO {
        return Err(perp_err("placeOrder: fee recipient not initialised"));
    }
    let mut account = storage::load_account(context, admin)?;
    account.perp_wallet_balance = account
        .perp_wallet_balance
        .checked_add(amount)
        .ok_or_else(|| perp_err("placeOrder: fee recipient balance overflow"))?;
    storage::save_account(context, admin, account)
}

fn calc_opening_margin_for_fill(
    pos: &crate::perp_dex::types::PerpPosition,
    fill_qty: u64,
    fill_value: u64,
    is_buy: bool,
) -> u64 {
    let closing_qty = if is_buy && pos.amount < 0 {
        fill_qty.min((-pos.amount) as u64)
    } else if !is_buy && pos.amount > 0 {
        fill_qty.min(pos.amount as u64)
    } else {
        0
    };
    let opening_qty = fill_qty - closing_qty;
    if opening_qty == 0 {
        return 0;
    }

    let leverage = pos.leverage.max(1);
    let open_value = (fill_value as u128 * opening_qty as u128 / fill_qty.max(1) as u128) as u64;
    open_value / leverage
}

fn calc_closing_qty(
    pos: &crate::perp_dex::types::PerpPosition,
    fill_qty: u64,
    is_buy: bool,
) -> u64 {
    if is_buy && pos.amount < 0 {
        fill_qty.min((-pos.amount) as u64)
    } else if !is_buy && pos.amount > 0 {
        fill_qty.min(pos.amount as u64)
    } else {
        0
    }
}

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

    if storage::load_account(context, user)?.perp_wallet_balance >= required_margin {
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

    if storage::load_account(context, user)?.perp_wallet_balance < required_margin {
        return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
    }
    Ok(())
}

fn cancel_same_side_orders_until_wallet_covers<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    side: Side,
    required_margin: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    while storage::load_account(context, user)?.perp_wallet_balance < required_margin {
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

        remove_from_book(context, market_id, side, order.price, &order_id)?;
        let remaining_qty = order.quantity - order.filled;
        release_margin_for_cancelled_order(
            context,
            user,
            market_id,
            side,
            &order_id,
            remaining_qty,
            market,
        )?;

        let mut cancelled = order;
        cancelled.status = OrderStatus::Cancelled;
        storage::save_order(context, &order_id, &cancelled)?;

        context.journal_mut().log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::OrderCancelled {
                user,
                orderId: FixedBytes(order_id),
                marketId: market_id,
            }
            .to_log_data(),
        });
    }

    Ok(())
}

fn apply_taker_closing_fill_to_position(
    pos: &mut crate::perp_dex::types::PerpPosition,
    wallet: &mut u64,
    closing_qty: u64,
    closing_value: u64,
    is_buy: bool,
) -> Result<(), PrecompileError> {
    if closing_qty == 0 {
        return Ok(());
    }

    let pos_abs = pos.amount.unsigned_abs() as u128;
    let margin_release = (pos.margin.max(0) as u128 * closing_qty as u128 / pos_abs) as i64;
    let vq_fraction = (pos.v_quote_balance as i128 * closing_qty as i128 / pos_abs as i128) as i64;
    let close_quote_delta: i64 = if is_buy {
        -(closing_value as i64)
    } else {
        closing_value as i64
    };
    let realised = margin_release + vq_fraction + close_quote_delta;
    if realised > 0 {
        *wallet = wallet.saturating_add(realised as u64);
    } else if realised < 0 {
        // TODO: Route negative isolated equity through bankruptcy handling instead
        // of silently consuming wallet balance.
        *wallet = wallet.saturating_sub((-realised) as u64);
    }
    pos.margin -= margin_release;

    if is_buy {
        pos.amount += closing_qty as i64;
    } else {
        pos.amount -= closing_qty as i64;
    }
    pos.v_quote_balance -= vq_fraction;

    Ok(())
}

fn apply_taker_opening_fill_to_position(
    pos: &mut crate::perp_dex::types::PerpPosition,
    wallet: &mut u64,
    opening_qty: u64,
    opening_value: u64,
    is_buy: bool,
    debit_wallet: bool,
) -> Result<(), PrecompileError> {
    if opening_qty == 0 {
        return Ok(());
    }

    let initial_margin = opening_value / pos.leverage.max(1);
    if debit_wallet {
        if *wallet < initial_margin {
            return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
        }
        *wallet -= initial_margin;
    }
    pos.margin += initial_margin as i64;

    if is_buy {
        pos.amount += opening_qty as i64;
        pos.v_quote_balance -= opening_value as i64;
    } else {
        pos.amount -= opening_qty as i64;
        pos.v_quote_balance += opening_value as i64;
    }

    Ok(())
}

fn apply_maker_fill_to_position(
    pos: &mut crate::perp_dex::types::PerpPosition,
    wallet: &mut u64,
    fill_qty: u64,
    fill_value: u64,
    is_buy: bool,
    margin_available: u64,
    wallet_freed: u64,
) -> Result<(), PrecompileError> {
    let initial_margin = calc_opening_margin_for_fill(pos, fill_qty, fill_value, is_buy);

    if initial_margin > 0 {
        if margin_available < initial_margin {
            return Err(perp_invariant_err(
                "maker fill margin was not covered by reserved margin",
            ));
        }
        if wallet_freed > initial_margin {
            *wallet = wallet.saturating_add(wallet_freed - initial_margin);
        }
        pos.margin += initial_margin as i64;
    } else {
        *wallet = wallet.saturating_add(wallet_freed);
    }

    apply_position_delta(pos, wallet, fill_qty, fill_value, is_buy)
}

fn apply_position_delta(
    pos: &mut crate::perp_dex::types::PerpPosition,
    wallet: &mut u64,
    fill_qty: u64,
    fill_value: u64,
    is_buy: bool,
) -> Result<(), PrecompileError> {
    let closing_qty: u64 = if is_buy && pos.amount < 0 {
        fill_qty.min((-pos.amount) as u64)
    } else if !is_buy && pos.amount > 0 {
        fill_qty.min(pos.amount as u64)
    } else {
        0
    };

    if closing_qty > 0 {
        let pos_abs = pos.amount.unsigned_abs() as u128;
        let margin_release = (pos.margin.max(0) as u128 * closing_qty as u128 / pos_abs) as i64;
        let vq_fraction =
            (pos.v_quote_balance as i128 * closing_qty as i128 / pos_abs as i128) as i64;
        let closing_value = (fill_value as u128 * closing_qty as u128 / fill_qty as u128) as i64;
        let close_quote_delta: i64 = if is_buy {
            -closing_value
        } else {
            closing_value
        };
        let realised = margin_release + vq_fraction + close_quote_delta;
        if realised > 0 {
            *wallet = wallet.saturating_add(realised as u64);
        } else if realised < 0 {
            *wallet = wallet.saturating_sub((-realised) as u64);
        }
        pos.margin -= margin_release;
        pos.v_quote_balance -= vq_fraction;
    }

    let opening_qty = fill_qty - closing_qty;
    if opening_qty > 0 {
        let opening_value =
            (fill_value as u128 * opening_qty as u128 / fill_qty.max(1) as u128) as i64;
        if is_buy {
            pos.v_quote_balance -= opening_value;
        } else {
            pos.v_quote_balance += opening_value;
        }
    }

    if is_buy {
        pos.amount += fill_qty as i64;
    } else {
        pos.amount -= fill_qty as i64;
    }

    Ok(())
}

// ── Order entry updates after fill ───────────────────────────────────────────

struct MakerFillReleasedMargin {
    margin_available: u64,
    wallet_freed: u64,
}

fn release_margin_for_maker_fill<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    side: Side,
    order_id: &[u8; 32],
    fill_qty: u64,
    maker_fee: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<MakerFillReleasedMargin, PrecompileError> {
    let mut pos = storage::load_position(context, user, market_id)?;
    let old_max = pos
        .buy_side_margin_reserved
        .max(pos.sell_side_margin_reserved);

    let (new_max, margin_available, fee_released) = match side {
        Side::Buy => {
            let old_side_reserved = pos.buy_side_margin_reserved;
            let mut entries = storage::load_buy_orders(context, user, market_id)?;
            let fee_released = match entries.iter_mut().find(|e| &e.order_id == order_id) {
                Some(e) => {
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
            let new_notional = calc_buy_side_reserved_notional(
                &entries,
                market.base_decimals,
                market.price_decimals,
                pos.amount,
            )?;
            let new_reserved = new_notional / pos.leverage.max(1);
            pos.buy_side_reserved_notional = new_notional;
            pos.buy_side_margin_reserved = new_reserved;
            storage::save_buy_orders(context, user, market_id, &entries)?;
            (
                new_reserved.max(pos.sell_side_margin_reserved),
                old_side_reserved.saturating_sub(new_reserved),
                fee_released,
            )
        }
        Side::Sell => {
            let old_side_reserved = pos.sell_side_margin_reserved;
            let mut entries = storage::load_sell_orders(context, user, market_id)?;
            let fee_released = match entries.iter_mut().find(|e| &e.order_id == order_id) {
                Some(e) => {
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
            let new_notional = calc_sell_side_reserved_notional(
                &entries,
                market.base_decimals,
                market.price_decimals,
                pos.amount,
            )?;
            let new_reserved = new_notional / pos.leverage.max(1);
            pos.sell_side_reserved_notional = new_notional;
            pos.sell_side_margin_reserved = new_reserved;
            storage::save_sell_orders(context, user, market_id, &entries)?;
            (
                pos.buy_side_margin_reserved.max(new_reserved),
                old_side_reserved.saturating_sub(new_reserved),
                fee_released,
            )
        }
    };

    if fee_released < maker_fee {
        return Err(perp_invariant_err(
            "maker fill fee was not covered by reserved fee",
        ));
    }

    let wallet_freed = old_max.saturating_sub(new_max);
    pos.fee_reserved = pos.fee_reserved.saturating_sub(fee_released);
    pos.margin_reserved_notional = pos
        .buy_side_reserved_notional
        .max(pos.sell_side_reserved_notional);
    pos.margin_reserved = new_max;
    storage::save_position(context, user, market_id, &pos)?;
    Ok(MakerFillReleasedMargin {
        margin_available,
        wallet_freed,
    })
}
