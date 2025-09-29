//! DA Signers precompiles
use std::{collections::HashMap, sync::OnceLock};

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use ark_bn254::{G1Affine, G1Projective};
use ark_ec::{AffineRepr, CurveGroup};
use ark_ff::Zero;
use context::{Block, Cfg, ContextTr, JournalTr, Transaction};
use primitives::{address, hash_set::HashSet, keccak256, Address, Bytes, Log, B256, U256};
use rmp_serde::{Deserializer as RMPDeserializer, Serializer as RMPSerializer};
use serde::{Deserialize, Serialize};

use crate::{
    as_bin::AsBinStr,
    contract_interface::{
        DASigners::{
            self, epochNumberCall, getAggPkG1Call, getAggPkG1Return, getQuorumCall,
            getQuorumRowCall, getSignerCall, isSignerCall, makeEpochCall, makeEpochReturn,
            paramsCall, quorumCountCall, registerNextEpochCall, registerNextEpochReturn,
            registerSignerCall, registerSignerReturn, registeredEpochCall, updateSocketCall,
            updateSocketReturn, NewSigner, SocketUpdated,
        },
        IDASigners::{Params, SignerDetail},
    },
    da_signers::{
        curve::{
            epoch_registration_hash, left_pad_to_fixed_size, serialize_g1, serialize_g1_point,
            signer_registration_hash, validate_signature,
        },
        key::{
            epoch_block_key, epoch_number_key, epoch_registered_signer_key, epoch_registration_key,
            quorum_count_key, quorum_key, registration_key, signer_key, votes_key,
        },
        types::IDASignersSignerDetail,
    },
    journal::{load_bytes, store_bytes},
    stateful_precompiles::convert_db_err,
    PrecompileError, PrecompileOutput, PrecompileResult,
};

mod curve;
mod key;
mod types;

/// DA Signers precompile address
pub const DA_SIGNERS_ADDRESS: Address = address!("0000000000000000000000000000000000001000");
/// DA Signers registry address
const DA_REGISTRY_ADDRESS: Address = address!("0x20f30b2584f3096ea0d6c18c3b5cacc0585e12fc");
/// Max socket length
const MAX_SOCKET_LENGTH: usize = 96;

/// selector => (gas cost, is static)
static SELECTORS: OnceLock<HashMap<[u8; 4], (u64, bool)>> = OnceLock::new();

fn selectors_map() -> &'static HashMap<[u8; 4], (u64, bool)> {
    SELECTORS.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert(DASigners::paramsCall::SELECTOR, (1_000, true));
        m.insert(DASigners::epochNumberCall::SELECTOR, (1_000, true));
        m.insert(DASigners::quorumCountCall::SELECTOR, (1_000, true));
        m.insert(DASigners::getSignerCall::SELECTOR, (100_000, true));
        m.insert(DASigners::getQuorumCall::SELECTOR, (100_000, true));
        m.insert(DASigners::getQuorumRowCall::SELECTOR, (10_000, true));
        m.insert(DASigners::registerSignerCall::SELECTOR, (100_000, false));
        m.insert(DASigners::updateSocketCall::SELECTOR, (50_000, false));
        m.insert(DASigners::registerNextEpochCall::SELECTOR, (100_000, false));
        m.insert(DASigners::getAggPkG1Call::SELECTOR, (1_000_000, true));
        m.insert(DASigners::isSignerCall::SELECTOR, (10_000, true));
        m.insert(DASigners::registeredEpochCall::SELECTOR, (10_000, true));
        m.insert(DASigners::makeEpochCall::SELECTOR, (100_000, false));

        m
    })
}

/// run function call to DASigners precompile
pub fn run_da_signers_call<CTX: ContextTr>(
    input_bytes: &[u8],
    gas_limit: u64,
    caller: Address,
    value: U256,
    is_static: bool,
    context: &mut CTX,
) -> PrecompileResult {
    let selector: [u8; 4] =
        input_bytes[..4].try_into().map_err(|_e| PrecompileError::StatefulInvalidInput)?;
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
        DASigners::paramsCall::SELECTOR => {
            Ok(stateful_output(gas_used, run_params_call(input_bytes, caller, value, context)?))
        }
        DASigners::epochNumberCall::SELECTOR => Ok(stateful_output(
            gas_used,
            run_epoch_number_call(input_bytes, caller, value, context)?,
        )),
        DASigners::quorumCountCall::SELECTOR => Ok(stateful_output(
            gas_used,
            run_quorum_count_call(input_bytes, caller, value, context)?,
        )),
        DASigners::getSignerCall::SELECTOR => {
            Ok(stateful_output(gas_used, run_get_signer_call(input_bytes, caller, value, context)?))
        }
        DASigners::getQuorumCall::SELECTOR => {
            Ok(stateful_output(gas_used, run_get_quorum_call(input_bytes, caller, value, context)?))
        }
        DASigners::getQuorumRowCall::SELECTOR => Ok(stateful_output(
            gas_used,
            run_get_quorum_row_call(input_bytes, caller, value, context)?,
        )),
        DASigners::registerSignerCall::SELECTOR => Ok(stateful_output(
            gas_used,
            run_register_signer_call(input_bytes, caller, value, context)?,
        )),
        DASigners::updateSocketCall::SELECTOR => Ok(stateful_output(
            gas_used,
            run_update_socket_call(input_bytes, caller, value, context)?,
        )),
        DASigners::registerNextEpochCall::SELECTOR => Ok(stateful_output(
            gas_used,
            run_register_next_epoch_call(input_bytes, caller, value, context)?,
        )),
        DASigners::getAggPkG1Call::SELECTOR => Ok(stateful_output(
            gas_used,
            run_get_agg_pk_g1_call(input_bytes, caller, value, context)?,
        )),
        DASigners::isSignerCall::SELECTOR => {
            Ok(stateful_output(gas_used, run_is_signer_call(input_bytes, caller, value, context)?))
        }
        DASigners::registeredEpochCall::SELECTOR => Ok(stateful_output(
            gas_used,
            run_registered_epoch_call(input_bytes, caller, value, context)?,
        )),
        DASigners::makeEpochCall::SELECTOR => {
            Ok(stateful_output(gas_used, run_make_epoch_call(input_bytes, caller, value, context)?))
        }
        _ => Err(PrecompileError::StatefulInvalidInput),
    }
}

fn stateful_output(gas_used: u64, bytes: Bytes) -> PrecompileOutput {
    PrecompileOutput::new(gas_used, bytes, 0)
}

fn params() -> Params {
    Params {
        tokensPerVote: U256::from(30),
        maxVotesPerSigner: U256::from(102400),
        maxQuorums: U256::from(10),
        epochBlocks: U256::from(28800),
        encodedSlices: U256::from(3072),
    }
}

fn run_params_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    _context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let _ = paramsCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    Ok(Bytes::from(paramsCall::abi_encode_returns(&params())))
}

fn epoch_number<CTX: ContextTr>(context: &mut CTX) -> Result<U256, PrecompileError> {
    Ok(context
        .journal_mut()
        .sload(DA_SIGNERS_ADDRESS, epoch_number_key().into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data)
}

fn run_epoch_number_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let _ = epochNumberCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;

    Ok(Bytes::from(epochNumberCall::abi_encode_returns(&epoch_number(context)?)))
}

fn quorum_count<CTX: ContextTr>(context: &mut CTX, epoch: u64) -> Result<U256, PrecompileError> {
    Ok(context
        .journal_mut()
        .sload(DA_SIGNERS_ADDRESS, quorum_count_key(epoch).into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data)
}

fn run_quorum_count_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = quorumCountCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    Ok(Bytes::from(quorumCountCall::abi_encode_returns(&quorum_count(
        context,
        args._epoch.try_into().map_err(|_e| PrecompileError::StatefulInvalidInput)?,
    )?)))
}

fn get_signer<CTX: ContextTr>(
    context: &mut CTX,
    account: Address,
) -> Result<Option<SignerDetail>, PrecompileError> {
    let buf = load_bytes(context, DA_SIGNERS_ADDRESS, signer_key(account))?;
    if buf.is_empty() {
        return Ok(None);
    }
    let mut de = RMPDeserializer::new(&buf[..]);
    let decoded: IDASignersSignerDetail = Deserialize::deserialize(&mut de)
        .map_err(|_e| PrecompileError::Other("deserialization failed".to_string()))?;
    Ok(Some(decoded.into()))
}

fn run_get_signer_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getSignerCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let mut signers = vec![];
    for account in args._account.into_iter() {
        if let Some(signer) = get_signer(context, account)? {
            signers.push(signer);
        } else {
            return Err(PrecompileError::Other("signer not found".to_string()));
        }
    }
    Ok(Bytes::from(getSignerCall::abi_encode_returns(&signers)))
}

fn get_quorum<CTX: ContextTr>(
    context: &mut CTX,
    epoch: u64,
    quorum_id: u64,
) -> Result<Vec<Address>, PrecompileError> {
    if epoch_number(context)? < U256::from(epoch) {
        return Err(PrecompileError::Other("epoch out of bound".to_string()));
    }
    if quorum_count(context, epoch)? <= U256::from(quorum_id) {
        return Err(PrecompileError::Other("quorum id out of bound".to_string()));
    }
    let buf = load_bytes(context, DA_SIGNERS_ADDRESS, quorum_key(epoch, quorum_id))?;
    let mut de = RMPDeserializer::new(&buf[..]);
    let decoded: Vec<AsBinStr> = Deserialize::deserialize(&mut de)
        .map_err(|_e| PrecompileError::Other("deserialization failed".to_string()))?;
    Ok(decoded.into_iter().map(Address::from).collect())
}

fn run_get_quorum_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getQuorumCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let epoch: u64 = args._epoch.try_into().map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let quorum_id: u64 =
        args._quorumId.try_into().map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    Ok(Bytes::from(getQuorumCall::abi_encode_returns(&get_quorum(context, epoch, quorum_id)?)))
}

fn run_get_quorum_row_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getQuorumRowCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let epoch: u64 = args._epoch.try_into().map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let quorum_id: u64 =
        args._quorumId.try_into().map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let quorum = get_quorum(context, epoch, quorum_id)?;
    if args._rowIndex >= quorum.len() as u32 {
        return Err(PrecompileError::Other("row id out of bound".to_string()));
    }
    Ok(Bytes::from(getQuorumRowCall::abi_encode_returns(&quorum[args._rowIndex as usize])))
}

fn set_signer<CTX: ContextTr>(
    context: &mut CTX,
    signer: SignerDetail,
) -> Result<(), PrecompileError> {
    let mut buf = Vec::new();
    let account = signer.signer;
    IDASignersSignerDetail::from(signer)
        .serialize(&mut RMPSerializer::new(&mut buf).with_struct_map())
        .map_err(|_e| PrecompileError::Other("serialization failed".to_string()))?;
    store_bytes(context, DA_SIGNERS_ADDRESS, signer_key(account), &buf)
}

fn run_register_signer_call<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = registerSignerCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    // validation
    if caller != DA_REGISTRY_ADDRESS {
        return Err(PrecompileError::Other("sender not registry".to_string()));
    }
    if args._signer.socket.len() > MAX_SOCKET_LENGTH {
        return Err(PrecompileError::Other("socket too long".to_string()));
    }
    // execute
    // validate sender
    // staked value is checked in registry contract
    if (get_signer(context, args._signer.signer)?).is_some() {
        Err(PrecompileError::Other("signer exists".to_string()))
    } else {
        // validate signature
        let hash = signer_registration_hash(args._signer.signer, context.cfg().chain_id());
        if !validate_signature(&args._signer, hash, args._signature.into()) {
            return Err(PrecompileError::Other("invalid signature".to_string()));
        }
        // save signer
        set_signer(context, args._signer.clone())?;
        // emit event
        context.journal_mut().log(Log {
            address: DA_SIGNERS_ADDRESS,
            data: NewSigner {
                signer: args._signer.signer,
                pkG1: args._signer.pkG1,
                pkG2: args._signer.pkG2,
            }
            .to_log_data(),
        });
        context.journal_mut().log(Log {
            address: DA_SIGNERS_ADDRESS,
            data: SocketUpdated { signer: args._signer.signer, socket: args._signer.socket }
                .to_log_data(),
        });

        Ok(Bytes::from(registerSignerCall::abi_encode_returns(&registerSignerReturn {})))
    }
}

fn run_update_socket_call<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = updateSocketCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    // validation
    if caller != context.tx().caller() {
        return Err(PrecompileError::Other("sender not origin".to_string()));
    }
    if args._socket.len() > MAX_SOCKET_LENGTH {
        return Err(PrecompileError::Other("socket too long".to_string()));
    }
    // execute
    if let Some(mut signer) = get_signer(context, caller)? {
        // save signer
        signer.socket = args._socket.clone();
        set_signer(context, signer)?;
        // emit log

        context.journal_mut().log(Log {
            address: DA_SIGNERS_ADDRESS,
            data: SocketUpdated { signer: caller, socket: args._socket }.to_log_data(),
        });

        Ok(Bytes::from(updateSocketCall::abi_encode_returns(&updateSocketReturn {})))
    } else {
        Err(PrecompileError::Other("signer not found".to_string()))
    }
}

fn get_registration<CTX: ContextTr>(
    context: &mut CTX,
    epoch: u64,
    account: Address,
) -> Result<U256, PrecompileError> {
    let h = context
        .journal_mut()
        .sload(DA_SIGNERS_ADDRESS, registration_key(epoch, account).into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data;
    Ok(h)
}

fn get_votes<CTX: ContextTr>(
    context: &mut CTX,
    epoch: u64,
    account: Address,
) -> Result<U256, PrecompileError> {
    let h = context
        .journal_mut()
        .sload(DA_SIGNERS_ADDRESS, votes_key(epoch, account).into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data;
    Ok(h)
}

fn epoch_registration<CTX: ContextTr>(
    context: &mut CTX,
    epoch: u64,
) -> Result<U256, PrecompileError> {
    let h = context
        .journal_mut()
        .sload(DA_SIGNERS_ADDRESS, epoch_registration_key(epoch).into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data;
    Ok(h)
}

fn store_registration<CTX: ContextTr>(
    context: &mut CTX,
    epoch: u64,
    signer: Address,
    signature: &[u8; 64],
    votes: U256,
) -> Result<(), PrecompileError> {
    if !get_registration(context, epoch, signer)?.is_zero() {
        return Ok(());
    }
    // save signature hash
    context
        .journal_mut()
        .sstore(
            DA_SIGNERS_ADDRESS,
            registration_key(epoch, signer).into(),
            keccak256(signature).into(),
        )
        .map_err(convert_db_err::<CTX::Db>)?;
    // save votes
    context
        .journal_mut()
        .sstore(DA_SIGNERS_ADDRESS, votes_key(epoch, signer).into(), votes)
        .map_err(convert_db_err::<CTX::Db>)?;
    // increment epoch registration count
    let registration = epoch_registration(context, epoch)?;
    context
        .journal_mut()
        .sstore(
            DA_SIGNERS_ADDRESS,
            epoch_registration_key(epoch).into(),
            registration + U256::from(1),
        )
        .map_err(convert_db_err::<CTX::Db>)?;
    // save registered signer address
    context
        .journal_mut()
        .sstore(
            DA_SIGNERS_ADDRESS,
            epoch_registered_signer_key(epoch, registration.try_into().unwrap()).into(),
            U256::from_be_slice(&left_pad_to_fixed_size(signer.as_slice().to_vec(), 32)),
        )
        .map_err(convert_db_err::<CTX::Db>)?;
    Ok(())
}

fn run_register_next_epoch_call<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = registerNextEpochCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    // validation
    if caller != DA_REGISTRY_ADDRESS {
        return Err(PrecompileError::Other("sender not registry".to_string()));
    }
    // execute
    // get signer
    // staked value is checked in registry contract
    if let Some(signer) = get_signer(context, args.signer)? {
        // validate signature
        let mut epoch: u64 = epoch_number(context)?.try_into().unwrap();
        epoch += 1;
        let hash = epoch_registration_hash(signer.signer, epoch, context.cfg().chain_id());
        if !validate_signature(&signer, hash, args._signature.clone().into()) {
            return Err(PrecompileError::Other("invalid signature".to_string()));
        }
        store_registration(
            context,
            epoch,
            args.signer,
            &serialize_g1(args._signature.clone().into()),
            args.votes,
        )?;
        Ok(Bytes::from(registerNextEpochCall::abi_encode_returns(&registerNextEpochReturn {})))
    } else {
        Err(PrecompileError::Other("signer not found".to_string()))
    }
}

fn run_is_signer_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = isSignerCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let mut found = false;
    if (get_signer(context, args._account)?).is_some() {
        found = true;
    }
    Ok(Bytes::from(isSignerCall::abi_encode_returns(&found)))
}

fn run_registered_epoch_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = registeredEpochCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let epoch: u64 = args._epoch.try_into().map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let mut registered = false;
    if !get_registration(context, epoch, args._account)?.is_zero() {
        registered = true;
    }
    Ok(Bytes::from(registeredEpochCall::abi_encode_returns(&registered)))
}

fn epoch_block<CTX: ContextTr>(context: &mut CTX, epoch: U256) -> Result<U256, PrecompileError> {
    Ok(context
        .journal_mut()
        .sload(DA_SIGNERS_ADDRESS, epoch_block_key(epoch.try_into().unwrap()).into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data)
}

fn epoch_registered_signer<CTX: ContextTr>(
    context: &mut CTX,
    epoch: u64,
    index: u64,
) -> Result<Address, PrecompileError> {
    let b: &[u8; 32] = &context
        .journal_mut()
        .sload(DA_SIGNERS_ADDRESS, epoch_registered_signer_key(epoch, index).into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data
        .to_be_bytes();
    Ok(Address::from_slice(&b[12..]))
}

fn run_make_epoch_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let _ = makeEpochCall::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let p = params();
    let mut epoch = epoch_number(context)?;
    let last_epoch_block = epoch_block(context, epoch)?;
    let block_height = context.block().number();
    if last_epoch_block > U256::from(0) && block_height < last_epoch_block + p.epochBlocks {
        // not yet to the next epoch
        return Ok(Bytes::from(makeEpochCall::abi_encode_returns(&makeEpochReturn {})));
    }
    // new epoch
    epoch += U256::from(1);
    let epoch_u64: u64 = epoch.try_into().unwrap();
    let cnt: u64 = epoch_registration(context, epoch_u64)?.try_into().unwrap();
    let mut ballots = vec![];
    for index in 0..cnt {
        let account = epoch_registered_signer(context, epoch_u64, index)?;
        let sig_hash = get_registration(context, epoch_u64, account)?;
        let mut votes = get_votes(context, epoch_u64, account)?;
        // MaxVotesPerSigner is hard limit
        if p.maxVotesPerSigner < votes {
            votes = p.maxVotesPerSigner
        }
        let votes_usize: usize = votes.try_into().unwrap();
        let mut content: B256 = sig_hash.into();
        for _ in 0..votes_usize {
            ballots.push((account, content));
            content = keccak256(content);
        }
    }
    ballots.sort_by(|a, b| a.1.as_slice().cmp(b.1.as_slice()));

    let mut quorums = vec![];
    let encoded_slices: usize = p.encodedSlices.try_into().unwrap();
    let max_quorums: usize = p.maxQuorums.try_into().unwrap();
    if ballots.len() >= encoded_slices {
        let mut i = 0;
        while i + encoded_slices <= ballots.len() {
            if max_quorums <= quorums.len() {
                break;
            }
            let mut quorum = Vec::with_capacity(encoded_slices);
            for j in 0..encoded_slices {
                quorum.push(AsBinStr::from(ballots[i + j].0));
            }
            quorums.push(quorum);
            i += encoded_slices;
        }

        if ballots.len() % encoded_slices != 0 && max_quorums > quorums.len() {
            let mut quorum = Vec::new();
            let start = ballots.len().saturating_sub(encoded_slices);
            for ballot in ballots.iter().skip(start) {
                quorum.push(AsBinStr::from(ballot.0));
            }
            quorums.push(quorum);
        }
    } else if !ballots.is_empty() {
        let mut quorum = Vec::with_capacity(encoded_slices);
        let n = ballots.len();
        for i in 0..encoded_slices {
            quorum.push(AsBinStr::from(ballots[i % n].0));
        }
        quorums.push(quorum);
    }

    for (index, quorum) in quorums.iter().enumerate() {
        let mut buf = Vec::new();
        quorum
            .serialize(&mut RMPSerializer::new(&mut buf).with_struct_map())
            .map_err(|_e| PrecompileError::Other("serialization failed".to_string()))?;
        store_bytes(context, DA_SIGNERS_ADDRESS, quorum_key(epoch_u64, index as u64), &buf)?;
    }
    context
        .journal_mut()
        .sstore(DA_SIGNERS_ADDRESS, quorum_count_key(epoch_u64).into(), U256::from(quorums.len()))
        .map_err(convert_db_err::<CTX::Db>)?;
    // save epoch number & block height
    context
        .journal_mut()
        .sstore(DA_SIGNERS_ADDRESS, epoch_number_key().into(), epoch)
        .map_err(convert_db_err::<CTX::Db>)?;
    context
        .journal_mut()
        .sstore(DA_SIGNERS_ADDRESS, epoch_block_key(epoch_u64).into(), block_height)
        .map_err(convert_db_err::<CTX::Db>)?;

    Ok(Bytes::from(makeEpochCall::abi_encode_returns(&makeEpochReturn {})))
}

fn run_get_agg_pk_g1_call<CTX: ContextTr>(
    input_bytes: &[u8],
    _caller: Address,
    _value: U256,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getAggPkG1Call::abi_decode_validate(input_bytes)
        .map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let epoch: u64 = args._epoch.try_into().map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let quorum_id: u64 =
        args._quorumId.try_into().map_err(|_e| PrecompileError::StatefulInvalidInput)?;
    let quorum = get_quorum(context, epoch, quorum_id)?;
    if quorum.len().div_ceil(8) != args._quorumBitmap.len() {
        return Err(PrecompileError::Other("quorum bitmap length mismatch".to_string()));
    }
    let mut agg_pubkey_g1 = G1Projective::zero();
    let mut hit = 0;
    let mut added = HashSet::new();

    for (i, signer_addr) in quorum.iter().enumerate() {
        if added.contains(signer_addr) {
            hit += 1;
            continue;
        }

        let b = args._quorumBitmap[i / 8] & (1 << (i % 8));
        if b == 0 {
            continue;
        }

        hit += 1;
        added.insert(*signer_addr);

        if let Some(signer) = get_signer(context, *signer_addr)? {
            let g1_point = Into::<G1Affine>::into(signer.pkG1).into_group();
            agg_pubkey_g1 += g1_point;
        } else {
            return Err(PrecompileError::Other("signer not found".to_string()));
        }
    }
    Ok(Bytes::from(getAggPkG1Call::abi_encode_returns(&getAggPkG1Return {
        aggPkG1: serialize_g1_point(agg_pubkey_g1.into_affine()),
        total: U256::from(quorum.len()),
        hit: U256::from(hit),
    })))
}
