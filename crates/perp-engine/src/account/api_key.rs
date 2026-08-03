//! API key management: register / revoke / query ed25519 signing keys.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use crate::host::PerpHost;
use primitives::{Address, Bytes, FixedBytes, Log};

use crate::{
        errors::perp_err,
    interface::IPerpDex::{
        self, getApiKeyCall, getApiKeyReturn, getApiKeysCall, getApiKeysReturn,
        registerApiKeyCall, revokeApiKeyCall,
    },
    storage,
    types::ApiKey,
    PERP_DEX_ADDRESS,
    PerpError,
};

/// `registerApiKey(uint8 keyId, bytes32 pubkey, uint64 expiry)`
pub fn run_register_api_key<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = registerApiKeyCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("registerApiKey: invalid calldata"))?;

    let pubkey: [u8; 32] = args.pubkey.0;
    if pubkey == [0u8; 32] {
        return Err(perp_err("registerApiKey: pubkey cannot be zero"));
    }

    storage::save_api_key(
        context,
        caller,
        args.keyId,
        ApiKey {
            pubkey,
            expiry: args.expiry,
        },
    )?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::ApiKeyRegistered {
            user: caller,
            keyId: args.keyId,
            pubkey: args.pubkey,
            expiry: args.expiry,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `revokeApiKey(uint8 keyId)`
pub fn run_revoke_api_key<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = revokeApiKeyCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("revokeApiKey: invalid calldata"))?;

    storage::delete_api_key(context, caller, args.keyId)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::ApiKeyRevoked {
            user: caller,
            keyId: args.keyId,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `getApiKey(address user, uint8 keyId) returns (bytes32 pubkey, uint64 expiry)`
pub fn run_get_api_key<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getApiKeyCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getApiKey: invalid calldata"))?;

    let (pubkey, expiry) = storage::load_api_key(context, args.user, args.keyId)?
        .map(|k| (FixedBytes(k.pubkey), k.expiry))
        .unwrap_or_default();

    Ok(Bytes::from(getApiKeyCall::abi_encode_returns(
        &getApiKeyReturn { pubkey, expiry },
    )))
}

/// `getApiKeys(address user) returns (uint8[] keyIds, bytes32[] pubkeys, uint64[] expiries)`
pub fn run_get_api_keys<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getApiKeysCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getApiKeys: invalid calldata"))?;

    let ids = storage::load_api_key_ids(context, args.user)?;
    let mut pubkeys = Vec::with_capacity(ids.len());
    let mut expiries = Vec::with_capacity(ids.len());

    for &id in &ids {
        let (pk, exp) = storage::load_api_key(context, args.user, id)?
            .map(|k| (FixedBytes(k.pubkey), k.expiry))
            .unwrap_or_default();
        pubkeys.push(pk);
        expiries.push(exp);
    }

    Ok(Bytes::from(getApiKeysCall::abi_encode_returns(
        &getApiKeysReturn {
            keyIds: ids,
            pubkeys,
            expiries,
        },
    )))
}
