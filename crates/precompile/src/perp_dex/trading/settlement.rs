use alloy_primitives::IntoLogData;
use context::{ContextTr, JournalTr};
use primitives::{Address, FixedBytes, Log};

use super::{release_margin_for_cancelled_order, remove_from_book};
use crate::{
    perp_dex::{
        errors::{perp_err, perp_invariant_err},
        interface::IPerpDex,
        math::{calc_buy_side_margin_reserved, calc_sell_side_margin_reserved, calc_value},
        storage,
        types::{OrderStatus, Side},
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};

// ── Position settlement ───────────────────────────────────────────────────────

/// Apply a fill to both taker and maker positions and account wallets.
pub(super) fn settle_fill<CTX: ContextTr>(
    context: &mut CTX,
    taker: Address,
    maker: Address,
    taker_order_id: &[u8; 32],
    maker_order_id: &[u8; 32],
    market_id: u64,
    fill_price: u64,
    fill_qty: u64,
    taker_side: Side, // the taker's side (Buy = taker bought, maker sold)
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let fill_value = calc_value(
        fill_price,
        fill_qty,
        market.base_decimals,
        market.price_decimals,
    )?;

    // Taker side.
    let taker_pos = {
        ensure_taker_wallet_can_cover_opening_margin(
            context, taker, market_id, taker_side, fill_qty, fill_value, market,
        )?;
        let mut pos = storage::load_position(context, taker, market_id)?;
        let mut account = storage::load_account(context, taker)?;
        apply_taker_fill_to_position(
            &mut pos,
            &mut account.perp_wallet_balance,
            fill_qty,
            fill_value,
            taker_side == Side::Buy,
        )?;
        storage::save_position(context, taker, market_id, &pos)?;
        storage::save_account(context, taker, account)?;
        pos
    };

    // Maker side (opposite of taker).
    let maker_pos = {
        let maker_side = taker_side.opposite();
        let released_margin = release_margin_for_maker_fill(
            context,
            maker,
            market_id,
            maker_side,
            maker_order_id,
            fill_qty,
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
        pos
    };

    // Emit Trade event.
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

    // Emit PositionChanged for taker.
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user: taker,
            marketId: market_id,
            amount: taker_pos.amount,
            vQuoteBalance: taker_pos.v_quote_balance,
            margin: taker_pos.margin,
            leverage: taker_pos.leverage,
        }
        .to_log_data(),
    });

    // Emit PositionChanged for maker.
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user: maker,
            marketId: market_id,
            amount: maker_pos.amount,
            vQuoteBalance: maker_pos.v_quote_balance,
            margin: maker_pos.margin,
            leverage: maker_pos.leverage,
        }
        .to_log_data(),
    });

    Ok(())
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

fn ensure_taker_wallet_can_cover_opening_margin<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    side: Side,
    fill_qty: u64,
    fill_value: u64,
    market: &crate::perp_dex::types::Market,
) -> Result<(), PrecompileError> {
    let pos = storage::load_position(context, user, market_id)?;
    let required_margin =
        calc_opening_margin_for_fill(&pos, fill_qty, fill_value, side == Side::Buy);
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

fn apply_taker_fill_to_position(
    pos: &mut crate::perp_dex::types::PerpPosition,
    wallet: &mut u64,
    fill_qty: u64,
    fill_value: u64,
    is_buy: bool,
) -> Result<(), PrecompileError> {
    let leverage = pos.leverage.max(1);

    // How much of this fill is closing existing position vs opening new?
    let closing_qty: u64 = if is_buy && pos.amount < 0 {
        fill_qty.min((-pos.amount) as u64)
    } else if !is_buy && pos.amount > 0 {
        fill_qty.min(pos.amount as u64)
    } else {
        0
    };
    let opening_qty = fill_qty - closing_qty;

    // --- Closing portion: realise PnL, release margin ---
    if closing_qty > 0 {
        let pos_abs = pos.amount.unsigned_abs() as u128;
        let margin_release = (pos.margin.max(0) as u128 * closing_qty as u128 / pos_abs) as i64;
        let vq_fraction =
            (pos.v_quote_balance as i128 * closing_qty as i128 / pos_abs as i128) as i64;
        let close_quote_delta: i64 = if is_buy {
            -((fill_value as u128 * closing_qty as u128 / fill_qty as u128) as i64)
        } else {
            (fill_value as u128 * closing_qty as u128 / fill_qty as u128) as i64
        };
        let realised = margin_release + vq_fraction + close_quote_delta;
        if realised > 0 {
            *wallet = wallet.saturating_add(realised as u64);
        } else if realised < 0 {
            *wallet = wallet.saturating_sub((-realised) as u64);
        }
        pos.margin -= margin_release;
    }

    // --- Opening portion: deduct initial margin from wallet ---
    if opening_qty > 0 {
        let open_value =
            (fill_value as u128 * opening_qty as u128 / fill_qty.max(1) as u128) as u64;
        let initial_margin = open_value / leverage;
        if *wallet < initial_margin {
            return Err(perp_err("placeOrder: insufficient perp wallet for margin"));
        }
        *wallet -= initial_margin;
        pos.margin += initial_margin as i64;
    }

    // Apply to position fields.
    if is_buy {
        pos.amount += fill_qty as i64;
        pos.v_quote_balance -= fill_value as i64;
    } else {
        pos.amount -= fill_qty as i64;
        pos.v_quote_balance += fill_value as i64;
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
        let close_quote_delta: i64 = if is_buy {
            -((fill_value as u128 * closing_qty as u128 / fill_qty as u128) as i64)
        } else {
            (fill_value as u128 * closing_qty as u128 / fill_qty as u128) as i64
        };
        let realised = margin_release + vq_fraction + close_quote_delta;
        if realised > 0 {
            *wallet = wallet.saturating_add(realised as u64);
        } else if realised < 0 {
            *wallet = wallet.saturating_sub((-realised) as u64);
        }
        pos.margin -= margin_release;
    }

    if is_buy {
        pos.amount += fill_qty as i64;
        pos.v_quote_balance -= fill_value as i64;
    } else {
        pos.amount -= fill_qty as i64;
        pos.v_quote_balance += fill_value as i64;
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
    market: &crate::perp_dex::types::Market,
) -> Result<MakerFillReleasedMargin, PrecompileError> {
    let mut pos = storage::load_position(context, user, market_id)?;
    let old_max = pos
        .buy_side_margin_reserved
        .max(pos.sell_side_margin_reserved);

    let (new_max, margin_available) = match side {
        Side::Buy => {
            let old_side_reserved = pos.buy_side_margin_reserved;
            let mut entries = storage::load_buy_orders(context, user, market_id)?;
            match entries.iter_mut().find(|e| &e.order_id == order_id) {
                Some(e) => {
                    e.amount = e.amount.saturating_sub(fill_qty);
                    if e.amount == 0 {
                        entries.retain(|e| &e.order_id != order_id);
                    }
                }
                None => {
                    return Err(perp_invariant_err(format!(
                        "buy entry for order {:?} not found during fill update",
                        order_id
                    )))
                }
            }
            let new_reserved = calc_buy_side_margin_reserved(
                &entries,
                pos.leverage.max(1),
                market.base_decimals,
                market.price_decimals,
                pos.amount,
            )?;
            pos.buy_side_margin_reserved = new_reserved;
            storage::save_buy_orders(context, user, market_id, &entries)?;
            (
                new_reserved.max(pos.sell_side_margin_reserved),
                old_side_reserved.saturating_sub(new_reserved),
            )
        }
        Side::Sell => {
            let old_side_reserved = pos.sell_side_margin_reserved;
            let mut entries = storage::load_sell_orders(context, user, market_id)?;
            match entries.iter_mut().find(|e| &e.order_id == order_id) {
                Some(e) => {
                    e.amount = e.amount.saturating_sub(fill_qty);
                    if e.amount == 0 {
                        entries.retain(|e| &e.order_id != order_id);
                    }
                }
                None => {
                    return Err(perp_invariant_err(format!(
                        "sell entry for order {:?} not found during fill update",
                        order_id
                    )))
                }
            }
            let new_reserved = calc_sell_side_margin_reserved(
                &entries,
                pos.leverage.max(1),
                market.base_decimals,
                market.price_decimals,
                pos.amount,
            )?;
            pos.sell_side_margin_reserved = new_reserved;
            storage::save_sell_orders(context, user, market_id, &entries)?;
            (
                pos.buy_side_margin_reserved.max(new_reserved),
                old_side_reserved.saturating_sub(new_reserved),
            )
        }
    };

    let wallet_freed = old_max.saturating_sub(new_max);
    pos.margin_reserved = new_max;
    storage::save_position(context, user, market_id, &pos)?;
    Ok(MakerFillReleasedMargin {
        margin_available,
        wallet_freed,
    })
}
