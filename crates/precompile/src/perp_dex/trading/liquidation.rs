use alloy_primitives::IntoLogData;
use context::{ContextTr, JournalTr};
use primitives::{Address, FixedBytes, Log};

use super::{match_order, next_order_id};
use crate::{
    perp_dex::{
        errors::{perp_err, perp_invariant_err},
        interface::IPerpDex,
        math::{calc_value, checked_u64_to_i64},
        storage,
        types::{Market, Order, OrderStatus, OrderType, Side, TimeInForce},
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};

/// Execute the liquidation close as an internal market IOC order.
///
/// Runs the IOC against the book and returns the unfilled quantity. If the
/// book absorbs the entire position (`remaining == 0`) the position storage is
/// cleaned up here. If the book can only partially fill, the caller is
/// responsible for settling the residual (see `settle_liquidation_residual_at_mark_price`).
pub(crate) fn execute_liquidation_market_order<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market: &Market,
    side: Side,
    quantity: u64,
) -> Result<u64, PrecompileError> {
    let order_id = next_order_id(context, user)?;
    let order = Order {
        owner: user.0 .0,
        market_id: market.market_id,
        side,
        price: 0,
        quantity,
        filled: 0,
        order_type: OrderType::Market,
        tif: TimeInForce::Ioc,
        status: OrderStatus::Open,
    };
    storage::save_order(context, &order_id, &order)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderPlaced {
            user,
            marketId: market.market_id,
            orderId: FixedBytes(order_id),
            side: side as u8,
            price: 0,
            quantity,
            orderType: OrderType::Market as u8,
            tif: TimeInForce::Ioc as u8,
            clientOrderId: FixedBytes::default(),
        }
        .to_log_data(),
    });

    let remaining = match_order(
        context,
        user,
        &order_id,
        market.market_id,
        side,
        0,
        quantity,
        OrderType::Market,
        TimeInForce::Ioc,
        market,
        // Liquidation close: waive the taker trading fee (the liquidated user pays
        // the clearance fee to the IF instead). Also prevents the close from
        // reverting when the underwater user cannot cover a taker fee.
        true,
    )?;

    if remaining == 0 {
        // Full fill: clean up any rounding residuals left in the position.
        let mut pos = storage::load_position(context, user, market.market_id)?;
        if pos.amount != 0 {
            return Err(perp_invariant_err(
                "liquidation market order: full fill but position not zero",
            ));
        }
        pos.v_quote_balance = 0;
        pos.margin = 0;
        storage::save_position(context, user, market.market_id, &pos)?;
    }

    Ok(remaining)
}

/// Closes the residual position (the part the orderbook could not absorb) at mark price.
///
/// Called when `execute_liquidation_market_order` returns `remaining > 0`. The position
/// still holds the proportional `margin` and `v_quote_balance` for the residual. This
/// function applies those to the wallet and zeroes the position.
///
/// Isolated margin: the loss is contained to the position's margin; any shortfall
/// beyond it is bad debt routed directly to the Insurance Fund here. The wallet is
/// never debited by the residual loss (only credited if the residual is solvent).
pub(crate) fn settle_liquidation_residual_at_mark_price<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market: &Market,
    liquidation_side: Side,
    mark_price: u64,
) -> Result<(), PrecompileError> {
    let mut pos = storage::load_position(context, user, market.market_id)?;
    let mut account = storage::load_account(context, user)?;

    let residual_value = calc_value(
        mark_price,
        pos.amount.unsigned_abs(),
        market.base_decimals,
        market.price_decimals,
    )?;
    let residual_value_i64 = checked_u64_to_i64(residual_value, "liquidation: residual value")?;

    // Selling a long → receive quote (+); buying a short → pay quote (-).
    let close_quote_delta = if liquidation_side == Side::Sell {
        residual_value_i64
    } else {
        -residual_value_i64
    };

    // realized = margin_release + vq_fraction + close_quote_delta
    // For a full residual close, all remaining margin and v_quote are consumed.
    let realized = pos
        .margin
        .checked_add(pos.v_quote_balance)
        .and_then(|v| v.checked_add(close_quote_delta))
        .ok_or_else(|| perp_err("liquidation: residual PnL overflow"))?;

    // Isolated margin: a profitable/solvent residual returns equity to the wallet; an
    // insolvent residual (loss exceeds the position's remaining margin) does NOT debit
    // the wallet — the shortfall is bad debt routed directly to the Insurance Fund.
    let bad_debt = if realized >= 0 {
        account.perp_wallet_balance = account.perp_wallet_balance.saturating_add(realized);
        0u64
    } else {
        realized.unsigned_abs()
    };

    pos.amount = 0;
    pos.v_quote_balance = 0;
    pos.margin = 0;

    storage::save_position(context, user, market.market_id, &pos)?;
    storage::save_account(context, user, account)?;

    super::settlement::absorb_bad_debt_into_insurance_fund(context, market.market_id, bad_debt)?;

    Ok(())
}
