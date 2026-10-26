//! Negative-balance clearance (cross margin P3; owner decisions Q8 and R4) — Binance's
//! `INSURANCE_CLEAR`.
//!
//! A cross wallet may go below zero (a cross loss settles into it whatever its sign — see
//! `types::UserAccount::perp_wallet_balance`), and so may an isolated-only one, by a maker
//! commission its close had nothing left to pay. While the account holds ANY position that deficit
//! is a receivable: the account's other positions may still close in profit, an isolated silo will
//! hand its close credit back to this very wallet, and `A < 0` / `X = 0` keep every open and every
//! outflow shut in the meantime. Binance's rule for clearing it (cross report §1.6 item 9, F23)
//! is the account holding NO open position, cross or isolated; we apply it at the moment that first
//! becomes true — the end of the call that closed the last one (deviation D6: no ten-minute timer
//! on chain), whatever route closed it.
//!
//! # Who pays
//!
//! The insurance fund covers `−W` and the wallet is set to exactly 0. When the fund holds less, it
//! covers what it has and the rest is SOCIALISED, logged by `InsuranceFundDepleted` — the
//! convention every other deficit the fund cannot absorb already follows (isolated bad debt,
//! `trading::settlement::absorb_bad_debt_into_insurance_fund`; R4). The wallet still ends at 0:
//! the shortfall is the protocol's, not a debt left on an account that has nothing open to collect
//! it from.
//!
//! # Events
//!
//! `InsuranceFundChanged(−covered, fund)` when the fund paid anything, `InsuranceFundDepleted(market,
//! uncovered)` when it could not pay everything — `market` is the one whose close made the account
//! flat — and the account's own `AccountBalanceChanged` with reason `INSURANCE_CLEAR` (7), a
//! 0-position group: Binance's `ACCOUNT_UPDATE` with `m = INSURANCE_CLEAR`. It follows the row the
//! call published for the account's last state, so a consumer sees the deficit appear and then
//! cleared (Binance R11 shows exactly that pair three seconds apart).
//!
//! # Total
//!
//! It runs at the end of every successful call, after the liquidation sweep, ADL and every fill,
//! so it must not fail: the only `Err` is the storage layer's. Every bound is a clamp.

use alloy_primitives::IntoLogData;
use primitives::{Address, Log};

use crate::{
    host::PerpHost, interface::IPerpDex, storage, types::AccountUpdateReason, PerpError,
    PERP_DEX_ADDRESS,
};

/// Clear `user`'s negative perp wallet against the insurance fund if the account holds no position
/// any more — see the module docs. `market_id` is the market whose close made it flat (the
/// `InsuranceFundDepleted` subject). Writes and publishes nothing when there is nothing to clear:
/// a position is open again (the call opened one after the close), or the wallet is not below
/// zero.
pub(crate) fn clear_negative_wallet<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<(), PerpError> {
    let deficit = {
        let account = storage::load_account_ref(context, user)?;
        if account.open_position_count != 0 || account.perp_wallet_balance >= 0 {
            return Ok(());
        }
        // `|i64::MIN| = 2^63` fits a `u64`.
        account.perp_wallet_balance.unsigned_abs()
    };
    let fund = storage::load_insurance_fund(context)?;
    let covered = deficit.min(fund);
    let uncovered = deficit - covered;
    if covered > 0 {
        storage::save_insurance_fund(context, fund - covered)?;
    }
    // Silent writer: the row below is published directly, with its own reason.
    storage::mutate_account(context, user, |a| a.perp_wallet_balance = 0)?;
    if covered > 0 {
        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::InsuranceFundChanged {
                // `covered ≤ 2^63`; only `2^63` itself has no positive `i64`, and its negation is
                // exactly `i64::MIN`.
                delta: i64::try_from(covered).map(|c| -c).unwrap_or(i64::MIN),
                newBalance: fund - covered,
            }
            .to_log_data(),
        });
    }
    if uncovered > 0 {
        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::InsuranceFundDepleted { marketId: market_id, badDebt: uncovered }
                .to_log_data(),
        });
    }
    storage::publish_account_snapshot_now(context, user, AccountUpdateReason::InsuranceClear)
}
