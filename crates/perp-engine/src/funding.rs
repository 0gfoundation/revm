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
use crate::host::PerpHost;
use primitives::{Address, Log};

use crate::{
        errors::perp_err,
    interface::IPerpDex,
    math::{calc_funding_payment, checked_u64_to_i64},
    storage,
    types::{Market, PerpPosition},
    PERP_DEX_ADDRESS,
    PerpError,
};

/// A funding settlement computed in memory but NOT yet written (commit-only #23). The
/// wallet→margin waterfall has already been applied to the caller's in-memory `pos`/`wallet`; the
/// insurance-fund charge + `FundingSettled` payload are carried here for a later
/// [`apply_funding_settlement`]. This lets a caller REJECT (before any storage write) between the
/// funding computation and its commit — so a rejected op never draws from the insurance fund.
pub(crate) struct PendingFunding {
    if_charge: u64,
    payment: i64,
    funding_rate: i64,
    user: Address,
    market_id: u64,
}

/// Pure funding computation: reads funding state, applies the funding payment to the in-memory
/// `pos`/`wallet` (wallet→margin waterfall), re-anchors `last_funding_index`, and returns the
/// pending insurance-fund charge + log payload (`None` if there was no funding event). Performs NO
/// storage writes, so it is safe to call before a validation reject.
pub(crate) fn compute_funding_settlement<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    pos: &mut PerpPosition,
    wallet: &mut i64,
) -> Result<Option<PendingFunding>, PerpError> {
    let funding = storage::load_funding_state(context, market.market_id)?;
    let index = funding.cumulative_funding_index;

    let mut pending = None;
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
        if payment != 0 {
            let if_charge = if payment > 0 {
                // Credit: the position receives funding into the perp wallet.
                *wallet = wallet
                    .checked_add(payment)
                    .ok_or_else(|| perp_err("funding: wallet credit overflow"))?;
                0
            } else {
                // Charge: wallet (down to 0) → position margin → insurance-fund remainder.
                let mut charge = (-(payment as i128)) as u64;
                let from_wallet = ((*wallet).max(0) as i128).min(charge as i128) as u64;
                *wallet -= from_wallet as i64;
                charge -= from_wallet;
                if charge > 0 {
                    let from_margin = (pos.margin.max(0) as i128).min(charge as i128) as u64;
                    pos.margin -= from_margin as i64;
                    charge -= from_margin;
                }
                charge
            };
            pending = Some(PendingFunding {
                if_charge,
                payment,
                funding_rate: funding.last_funding_rate,
                user,
                market_id: market.market_id,
            });
        }
    }

    // Always re-anchor — covers flat positions and freshly opened legs so they
    // do not retroactively accrue funding from before they existed.
    pos.last_funding_index = index;
    Ok(pending)
}

/// Commits a [`PendingFunding`]: absorbs the insurance-fund shortfall (the ONLY storage write in
/// the funding path) and emits the `InsuranceFund*` / `FundingSettled` logs. MUST run only after
/// the caller has decided to commit (all rejects passed).
pub(crate) fn apply_funding_settlement<H: PerpHost>(
    context: &mut H,
    p: PendingFunding,
) -> Result<(), PerpError> {
    if p.if_charge > 0 {
        let (absorbed, bad_debt) = storage::absorb_from_insurance_fund(context, p.if_charge)?;
        let new_if = storage::load_insurance_fund(context)?;
        if absorbed > 0 {
            let absorbed_i64 = checked_u64_to_i64(absorbed, "funding: IF absorption delta")?;
            context.log(Log {
                address: PERP_DEX_ADDRESS,
                data: IPerpDex::InsuranceFundChanged {
                    delta: -absorbed_i64,
                    newBalance: new_if,
                }
                .to_log_data(),
            });
        }
        if bad_debt > 0 {
            context.log(Log {
                address: PERP_DEX_ADDRESS,
                data: IPerpDex::InsuranceFundDepleted {
                    marketId: p.market_id,
                    badDebt: bad_debt,
                }
                .to_log_data(),
            });
        }
    }

    let mark_price = storage::load_mark_price(context, p.market_id)?;
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::FundingSettled {
            marketId: p.market_id,
            user: p.user,
            fundingRate: p.funding_rate,
            amount: p.payment,
            markPrice: mark_price,
        }
        .to_log_data(),
    });
    Ok(())
}

/// Behaviour-preserving combined settle (compute + immediate apply). For callers that do NOT need
/// to reject between the two (matching settlement, liquidation). Migrated callers (margin
/// add/remove) instead use [`compute_funding_settlement`] + [`apply_funding_settlement`] with their
/// rejects in between so a rejected op leaves the insurance fund untouched.
pub(crate) fn settle_position_funding<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    pos: &mut PerpPosition,
    wallet: &mut i64,
) -> Result<(), PerpError> {
    if let Some(pending) = compute_funding_settlement(context, user, market, pos, wallet)? {
        apply_funding_settlement(context, pending)?;
    }
    Ok(())
}
