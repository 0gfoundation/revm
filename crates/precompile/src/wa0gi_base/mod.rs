//! WA0GI Base precompiles
use std::{collections::HashMap, sync::OnceLock};

use alloy_sol_types::SolCall;
use context::{ContextTr, JournalTr};
use primitives::{address, Address, Bytes, U256};
use rmp_serde::{Deserializer as RMPDeserializer, Serializer as RMPSerializer};
use serde::{Deserialize, Serialize};

use crate::{
    contract_interface::WrappedA0GIBase::{
        self, burnCall, burnReturn, getWA0GICall, mintCall, mintReturn, minterSupplyCall,
        setMinterCapCall, setMinterCapReturn, Supply,
    },
    journal::{load_bytes, store_bytes},
    stateful_precompiles::convert_db_err,
    wa0gi_base::{key::supply_key, types::MinterSupply},
    PrecompileError, PrecompileOutput, PrecompileResult,
};

mod key;
mod types;

/// WA0GI Base precompile address
pub const WA0GI_BASE_ADDRESS: Address = address!("0000000000000000000000000000000000001002");
/// WA0GI address
pub const WA0GI_ADDRESS: Address = address!("0x1cd0690ff9a693f5ef2dd976660a8dafc81a109c");
/// Agency address
pub const WA0GI_AGENCY_ADDRESS: Address = address!("0xe1a5162f99e075f8c6681ae28191ab3ac250b468");

/// selector => (gas cost, is static)
static SELECTORS: OnceLock<HashMap<[u8; 4], (u64, bool)>> = OnceLock::new();

fn selectors_map() -> &'static HashMap<[u8; 4], (u64, bool)> {
    SELECTORS.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert(WrappedA0GIBase::mintCall::SELECTOR, (100_000, false));
        m.insert(WrappedA0GIBase::burnCall::SELECTOR, (100_000, false));
        m.insert(
            WrappedA0GIBase::setMinterCapCall::SELECTOR,
            (100_000, false),
        );
        m.insert(WrappedA0GIBase::getWA0GICall::SELECTOR, (5_000, true));
        m.insert(WrappedA0GIBase::minterSupplyCall::SELECTOR, (10_000, true));

        m
    })
}

/// run function call to WA0GI Base precompile
pub fn run_wa0gi_base_call<CTX: ContextTr>(
    input_bytes: &[u8],
    gas_limit: u64,
    caller: Address,
    value: U256,
    is_static: bool,
    context: &mut CTX,
) -> PrecompileResult {
    let selector: [u8; 4] = input_bytes[..4]
        .try_into()
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    // check gas cost and static restriction
    let gas_used = match selectors_map().get(&selector) {
        Some(&(gas_cost, can_be_static)) => {
            if gas_cost > gas_limit {
                return Err(PrecompileError::OutOfGas);
            }
            if is_static && !can_be_static {
                return Err(PrecompileError::StaticRestrictionViolation);
            }
            gas_cost
        }
        None => {
            return Err(PrecompileError::StatefulInvalidInput);
        }
    };
    // call corresponding function
    match selector {
        WrappedA0GIBase::mintCall::SELECTOR => Ok(PrecompileOutput::new(
            gas_used,
            run_mint_call(input_bytes, caller, value, context)?,
        )),
        WrappedA0GIBase::burnCall::SELECTOR => Ok(PrecompileOutput::new(
            gas_used,
            run_burn_call(input_bytes, caller, value, context)?,
        )),
        WrappedA0GIBase::setMinterCapCall::SELECTOR => Ok(PrecompileOutput::new(
            gas_used,
            run_set_minter_cap_call(input_bytes, caller, value, context)?,
        )),
        WrappedA0GIBase::getWA0GICall::SELECTOR => Ok(PrecompileOutput::new(
            gas_used,
            run_get_wa0gi_call(input_bytes, caller, value, context)?,
        )),
        WrappedA0GIBase::minterSupplyCall::SELECTOR => Ok(PrecompileOutput::new(
            gas_used,
            run_minter_supply_call(input_bytes, caller, value, context)?,
        )),
        _ => Err(PrecompileError::StatefulInvalidInput),
    }
}

fn get_minter_supply<CTX: ContextTr>(
    context: &mut CTX,
    account: Address,
) -> Result<Supply, PrecompileError> {
    let buf = load_bytes(context, WA0GI_BASE_ADDRESS, supply_key(account))?;
    if buf.is_empty() {
        return Ok(Supply {
            cap: U256::ZERO,
            initialSupply: U256::ZERO,
            supply: U256::ZERO,
        });
    }
    let mut de = RMPDeserializer::new(&buf[..]);
    let decoded: MinterSupply = Deserialize::deserialize(&mut de)
        .map_err(|_e| PrecompileError::Other("deserialization failed".to_string()))?;
    Ok(decoded.into())
}

fn set_minter_supply<CTX: ContextTr>(
    context: &mut CTX,
    account: Address,
    supply: Supply,
) -> Result<(), PrecompileError> {
    let mut buf = Vec::new();
    MinterSupply::from(supply)
        .serialize(&mut RMPSerializer::new(&mut buf).with_struct_map())
        .map_err(|_e| PrecompileError::Other("serialization failed".to_string()))?;
    store_bytes(context, WA0GI_BASE_ADDRESS, supply_key(account), &buf)
}

fn run_mint_call<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = mintCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    // validation
    if caller != WA0GI_ADDRESS {
        return Err(PrecompileError::Other("sender is not WA0GI".to_string()));
    }
    // execute
    let mut supply = get_minter_supply(context, args.minter)?;
    supply.supply = supply.supply.saturating_add(args.amount);
    if supply.supply > supply.cap {
        return Err(PrecompileError::Other("insufficient mint cap".to_string()));
    }
    // update supply
    context
        .journal_mut()
        .balance_incr(WA0GI_ADDRESS, args.amount)
        .map_err(convert_db_err::<CTX::Db>)?;

    Ok(Bytes::from(mintCall::abi_encode_returns(&mintReturn {})))
}

fn run_burn_call<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = burnCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    // validation
    if caller != WA0GI_ADDRESS {
        return Err(PrecompileError::Other("sender is not WA0GI".to_string()));
    }
    // execute
    let mut supply = get_minter_supply(context, args.minter)?;
    if supply.supply < args.amount {
        return Err(PrecompileError::Other(
            "insufficient mint supply".to_string(),
        ));
    }
    supply.supply = supply.supply.saturating_sub(args.amount);
    if (context
        .journal_mut()
        .balance_decr(WA0GI_ADDRESS, args.amount)
        .map_err(convert_db_err::<CTX::Db>)?)
    .is_some()
    {
        return Err(PrecompileError::Other(
            "WA0GI balance insufficient".to_string(),
        ));
    }
    set_minter_supply(context, args.minter, supply)?;

    Ok(Bytes::from(burnCall::abi_encode_returns(&burnReturn {})))
}

fn run_set_minter_cap_call<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = setMinterCapCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    // validation
    if caller != WA0GI_AGENCY_ADDRESS {
        return Err(PrecompileError::Other("sender is not agency".to_string()));
    }
    // execute
    let mut supply = get_minter_supply(context, args.minter)?;
    match supply.initialSupply.cmp(&args.initialSupply) {
        std::cmp::Ordering::Greater => {
            // old > new -> add(diff)
            supply.supply = supply
                .supply
                .saturating_add(supply.initialSupply - args.initialSupply);
        }
        std::cmp::Ordering::Less => {
            // old < new -> sub(diff)
            supply.supply = supply
                .supply
                .saturating_sub(args.initialSupply - supply.initialSupply);
        }
        std::cmp::Ordering::Equal => {}
    }
    supply.cap = args.cap;
    supply.initialSupply = args.initialSupply;
    set_minter_supply(context, args.minter, supply)?;

    Ok(Bytes::from(setMinterCapCall::abi_encode_returns(
        &setMinterCapReturn {},
    )))
}

fn run_get_wa0gi_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    _context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let _ = getWA0GICall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    Ok(Bytes::from(getWA0GICall::abi_encode_returns(
        &WA0GI_ADDRESS,
    )))
}

fn run_minter_supply_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = minterSupplyCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let supply = get_minter_supply(context, args.minter)?;
    Ok(Bytes::from(minterSupplyCall::abi_encode_returns(&supply)))
}
