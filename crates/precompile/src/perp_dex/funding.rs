//! Lazy per-position funding settlement.
//!
//! A per-market cumulative funding index (`FundingState.cumulative_funding_index`)
//! accumulates `mark_price * funding_rate` at each funding-epoch boundary (see
//! `risk::run_update_index_price`). Each position records the index value at its
//! last settlement (`PerpPosition.last_funding_index`).
//!
//! Funding owed since then is `calc_funding_payment(amount, index_delta, …)` and
//! is settled **lazily**: every operation that touches a position (trade fills,
//! margin add/remove, liquidation) calls [`settle_position_funding`] on the loaded
//! position *before* mutating it, so the entire accrual is realised against the
//! `amount` that was actually held over the period.
//!
//! Settlement direction (matches `calc_funding_payment`'s sign): a positive
//! payment is credited to the perp wallet; a negative payment (a charge) is taken
//! from the wallet first (down to 0), then the position's isolated `margin`, and
//! any remainder is absorbed from the insurance fund (bad debt is written off).

use alloy_primitives::IntoLogData;
use context::{ContextTr, JournalTr};
use primitives::{Address, Log};

use crate::{
    perp_dex::{
        errors::perp_err,
        interface::IPerpDex,
        math::{calc_funding_payment, checked_u64_to_i64},
        storage,
        types::{Market, PerpPosition},
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};

/// Settle accrued funding on `pos` against the market's cumulative funding index,
/// then re-anchor the position to the current index.
///
/// `wallet` is the owner's `perp_wallet_balance` (passed separately so the caller
/// keeps ownership of the rest of the account). The caller is responsible for
/// persisting `pos` and the account afterwards.
///
/// No-op (beyond re-anchoring) for a flat position or when the index has not moved.
pub(crate) fn settle_position_funding<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market: &Market,
    pos: &mut PerpPosition,
    wallet: &mut i64,
) -> Result<(), PrecompileError> {
    let funding = storage::load_funding_state(context, market.market_id)?;
    let index = funding.cumulative_funding_index;

    if pos.amount != 0 && pos.last_funding_index != index {
        let delta = index
            .checked_sub(pos.last_funding_index)
            .ok_or_else(|| perp_err("funding: index delta overflow"))?;
        let payment = calc_funding_payment(
            pos.amount,
            delta,
            market.base_decimals,
            market.price_decimals,
        )?;
        apply_funding_payment(
            context,
            user,
            market,
            pos,
            wallet,
            payment,
            funding.last_funding_rate,
        )?;
    }

    // Always re-anchor — covers flat positions and freshly opened legs so they
    // do not retroactively accrue funding from before they existed.
    pos.last_funding_index = index;
    Ok(())
}

/// Apply a non-anchoring funding payment to the wallet / margin / insurance fund
/// and emit `FundingSettled`.
#[allow(clippy::too_many_arguments)]
fn apply_funding_payment<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market: &Market,
    pos: &mut PerpPosition,
    wallet: &mut i64,
    payment: i64,
    funding_rate: i64,
) -> Result<(), PrecompileError> {
    if payment == 0 {
        return Ok(());
    }

    if payment > 0 {
        // Credit: the position receives funding into the perp wallet.
        *wallet = wallet
            .checked_add(payment)
            .ok_or_else(|| perp_err("funding: wallet credit overflow"))?;
    } else {
        // Charge: wallet (down to 0) → position margin → insurance fund.
        let mut charge = (-(payment as i128)) as u64;

        let from_wallet = ((*wallet).max(0) as i128).min(charge as i128) as u64;
        *wallet -= from_wallet as i64;
        charge -= from_wallet;

        if charge > 0 {
            let from_margin = (pos.margin.max(0) as i128).min(charge as i128) as u64;
            pos.margin -= from_margin as i64;
            charge -= from_margin;
        }

        if charge > 0 {
            // The counterparty side is still owed this funding (it is zero-sum),
            // so the shortfall is covered by the insurance fund; any uncovered
            // remainder is protocol bad debt.
            let (absorbed, bad_debt) = storage::absorb_from_insurance_fund(context, charge)?;
            let new_if = storage::load_insurance_fund(context)?;
            if absorbed > 0 {
                let absorbed_i64 = checked_u64_to_i64(absorbed, "funding: IF absorption delta")?;
                context.journal_mut().log(Log {
                    address: PERP_DEX_ADDRESS,
                    data: IPerpDex::InsuranceFundChanged {
                        delta: -absorbed_i64,
                        newBalance: new_if,
                    }
                    .to_log_data(),
                });
            }
            if bad_debt > 0 {
                context.journal_mut().log(Log {
                    address: PERP_DEX_ADDRESS,
                    data: IPerpDex::InsuranceFundDepleted {
                        marketId: market.market_id,
                        badDebt: bad_debt,
                    }
                    .to_log_data(),
                });
            }
        }
    }

    let mark_price = storage::load_mark_price(context, market.market_id)?;
    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::FundingSettled {
            marketId: market.market_id,
            user,
            fundingRate: funding_rate,
            amount: payment,
            markPrice: mark_price,
        }
        .to_log_data(),
    });
    Ok(())
}
