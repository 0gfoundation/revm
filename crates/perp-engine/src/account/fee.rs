use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use crate::host::PerpHost;
use primitives::{Address, Bytes, Log};

use crate::{
        errors::perp_err,
    interface::IPerpDex::{
        self, getUserFeeRatesCall, getUserFeeRatesReturn, setUserFeeRatesCall,
    },
    math::MAX_USER_FEE_BPS,
    storage,
    types::{derive_sub_account, UserFeeRates},
    PERP_DEX_ADDRESS,
    PerpError,
};

/// `setUserFeeRates(address user, uint64 makerFeeBps, uint64 takerFeeBps)` — admin only.
///
/// Sub-accounts carry their master's rates (copied at `createSubAccount`), and this is what keeps
/// them in step: a SUB is refused outright ("set the master's rates" — a sub priced differently
/// from its master would let a family route flow to whichever member is cheaper), and a MASTER's
/// new rates are written to each of its `sub_count` ≤ `MAX_SUB_ACCOUNTS` subs in the same call,
/// one `UserFeeRatesUpdated` per account written (master first, then subs in index order). Every
/// reject precedes the first write (commit-only #23).
pub fn run_set_user_fee_rates<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setUserFeeRatesCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setUserFeeRates: invalid calldata"))?;

    require_admin(caller, context)?;
    if args.user == Address::ZERO {
        return Err(perp_err("setUserFeeRates: user cannot be zero address"));
    }
    if args.makerFeeBps > MAX_USER_FEE_BPS || args.takerFeeBps > MAX_USER_FEE_BPS {
        return Err(perp_err("setUserFeeRates: fee bps exceeds 1000 (10%)"));
    }

    let sub_count = {
        let a = storage::load_account_ref(context, args.user)?;
        if a.master.is_some() {
            return Err(perp_err(
                "setUserFeeRates: user is a sub-account; set its master's rates",
            ));
        }
        a.sub_count
    };

    let rates = UserFeeRates {
        maker_fee_bps: args.makerFeeBps,
        taker_fee_bps: args.takerFeeBps,
    };
    // The master, then its subs (0 for an ordinary user) — the push-down.
    let family =
        core::iter::once(args.user).chain((0..sub_count).map(|i| derive_sub_account(args.user, i)));
    for user in family {
        storage::save_user_fee_rates(context, user, rates)?;
        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::UserFeeRatesUpdated {
                user,
                makerFeeBps: rates.maker_fee_bps,
                takerFeeBps: rates.taker_fee_bps,
            }
            .to_log_data(),
        });
    }

    Ok(Bytes::new())
}

pub fn run_get_user_fee_rates<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getUserFeeRatesCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getUserFeeRates: invalid calldata"))?;
    let rates = storage::load_user_fee_rates(context, args.user)?;
    Ok(Bytes::from(getUserFeeRatesCall::abi_encode_returns(
        &getUserFeeRatesReturn {
            makerFeeBps: rates.maker_fee_bps,
            takerFeeBps: rates.taker_fee_bps,
        },
    )))
}

fn require_admin<H: PerpHost>(
    caller: Address,
    context: &mut H,
) -> Result<(), PerpError> {
    let admin = storage::load_admin(context)?;
    if admin == Address::ZERO {
        return Err(perp_err("not authorised: admin not initialised"));
    }
    if caller != admin {
        return Err(perp_err("not authorised: caller is not admin"));
    }
    Ok(())
}
