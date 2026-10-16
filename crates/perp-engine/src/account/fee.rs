use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use crate::host::PerpHost;
use primitives::{Address, Bytes, Log};

use crate::{
        errors::perp_err,
    interface::IPerpDex::{
        self, getMaxMakerRebateCall, getUserFeeRatesCall, getUserFeeRatesReturn,
        setMaxMakerRebateCall, setUserFeeRatesCall,
    },
    math::{MAX_FEE_PPM, MAX_MAKER_REBATE_CEILING_PPM},
    storage,
    types::{derive_sub_account, UserFeeRates},
    PERP_DEX_ADDRESS,
    PerpError,
};

/// `setUserFeeRates(address user, int64 makerFeePpm, uint64 takerFeePpm)` — admin only.
///
/// Rates are in ppm of notional (`math::FEE_PPM_DENOMINATOR`): `0 ≤ taker ≤ MAX_FEE_PPM` and
/// `−R ≤ maker ≤ MAX_FEE_PPM`, with `R` = [`storage::load_max_maker_rebate`] read NOW.
///
/// Deliberately NO rule between the two rates, and none across accounts (no "taker ≥ R", no
/// "min taker + min maker ≥ 0"). Fee tiers are designed by hand so that a rebate never exceeds
/// the taker fee that funds it; the code's only solvency guarantee is the always-on per-fill cap
/// in `settlement::entry_fill_plan` (`rebate_i ≤ the fill's taker-fee share`), which holds
/// whatever is configured. Lowering `R` later leaves rates already set below `−R` untouched for
/// the same reason — the cap, not `R`, is what bounds what they can draw.
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
    if args.takerFeePpm > MAX_FEE_PPM {
        return Err(perp_err("setUserFeeRates: taker fee exceeds 100000 ppm (10%)"));
    }
    if args.makerFeePpm > MAX_FEE_PPM as i64 {
        return Err(perp_err("setUserFeeRates: maker fee exceeds 100000 ppm (10%)"));
    }
    // Clamped to the setter's ceiling before the signed negation: the setter never stores more,
    // but the loader decodes any u64, and a bare `as i64` on a corrupt value would wrap.
    let max_rebate =
        storage::load_max_maker_rebate(context)?.min(MAX_MAKER_REBATE_CEILING_PPM);
    if args.makerFeePpm < -(max_rebate as i64) {
        return Err(perp_err(format!(
            "setUserFeeRates: maker rebate exceeds maxMakerRebate ({max_rebate} ppm)"
        )));
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
        maker_fee_ppm: args.makerFeePpm,
        taker_fee_ppm: args.takerFeePpm,
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
                makerFeePpm: rates.maker_fee_ppm,
                takerFeePpm: rates.taker_fee_ppm,
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
            makerFeePpm: rates.maker_fee_ppm,
            takerFeePpm: rates.taker_fee_ppm,
        },
    )))
}

/// `setMaxMakerRebate(uint64 ppm)` — admin only. Sets the bound `R` that `setUserFeeRates`
/// checks a negative maker rate against, `0 ≤ R ≤ MAX_MAKER_REBATE_CEILING_PPM`.
///
/// Prospective only: no stored rate and no resting order's snapshot is re-validated, so lowering
/// `R` leaves every existing rebate in place — and a sub-account created later inherits its
/// master's rate as it is (`createSubAccount` copies it unchecked). That is safe because no fill's rebate can exceed its
/// own taker-fee share (`settlement::entry_fill_plan`), whatever `R` is or was.
pub fn run_set_max_maker_rebate<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setMaxMakerRebateCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setMaxMakerRebate: invalid calldata"))?;
    require_admin(caller, context)?;
    if args.maxMakerRebatePpm > MAX_MAKER_REBATE_CEILING_PPM {
        return Err(perp_err("setMaxMakerRebate: exceeds 10000 ppm (1%)"));
    }
    storage::save_max_maker_rebate(context, args.maxMakerRebatePpm)?;
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MaxMakerRebateUpdated {
            maxMakerRebatePpm: args.maxMakerRebatePpm,
        }
        .to_log_data(),
    });
    Ok(Bytes::new())
}

/// `getMaxMakerRebate()` — the current maker-rebate bound `R` in ppm (0 when never set).
pub fn run_get_max_maker_rebate<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    getMaxMakerRebateCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getMaxMakerRebate: invalid calldata"))?;
    let ppm = storage::load_max_maker_rebate(context)?;
    Ok(Bytes::from(getMaxMakerRebateCall::abi_encode_returns(&ppm)))
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
