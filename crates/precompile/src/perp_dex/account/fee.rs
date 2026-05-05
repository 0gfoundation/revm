use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use context::{ContextTr, JournalTr};
use primitives::{Address, Bytes, Log};

use crate::{
    perp_dex::{
        errors::perp_err,
        interface::IPerpDex::{
            self, getUserFeeRatesCall, getUserFeeRatesReturn, setUserFeeRatesCall,
        },
        math::FEE_BPS_DENOMINATOR,
        storage,
        types::UserFeeRates,
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};

pub fn run_set_user_fee_rates<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = setUserFeeRatesCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setUserFeeRates: invalid calldata"))?;

    require_admin(caller, context)?;
    if args.user == Address::ZERO {
        return Err(perp_err("setUserFeeRates: user cannot be zero address"));
    }
    if args.makerFeeBps > FEE_BPS_DENOMINATOR || args.takerFeeBps > FEE_BPS_DENOMINATOR {
        return Err(perp_err("setUserFeeRates: fee bps exceeds 100%"));
    }

    let rates = UserFeeRates {
        maker_fee_bps: args.makerFeeBps,
        taker_fee_bps: args.takerFeeBps,
    };
    storage::save_user_fee_rates(context, args.user, rates)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::UserFeeRatesUpdated {
            user: args.user,
            makerFeeBps: rates.maker_fee_bps,
            takerFeeBps: rates.taker_fee_bps,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

pub fn run_get_user_fee_rates<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
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

fn require_admin<CTX: ContextTr>(
    caller: Address,
    context: &mut CTX,
) -> Result<(), PrecompileError> {
    let admin = storage::load_admin(context)?;
    if admin == Address::ZERO {
        return Err(perp_err("not authorised: admin not initialised"));
    }
    if caller != admin {
        return Err(perp_err("not authorised: caller is not admin"));
    }
    Ok(())
}
