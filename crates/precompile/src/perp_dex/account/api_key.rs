//! API key management: register / revoke / query ed25519 signing keys.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use context::{ContextTr, JournalTr};
use primitives::{Address, Bytes, FixedBytes, Log};

use crate::{
    perp_dex::{
        errors::perp_err,
        interface::IPerpDex::{self, getApiKeyCall, getApiKeyReturn, registerApiKeyCall, revokeApiKeyCall},
        storage,
        types::ApiKey,
        PERP_DEX_ADDRESS,
    },
    PrecompileError,
};

/// `registerApiKey(bytes32 pubkey, uint64 expiry)` — store an ed25519 public key for the caller.
/// `expiry` is a Unix-second timestamp; pass 0 for no expiry.
pub fn run_register_api_key<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = registerApiKeyCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("registerApiKey: invalid calldata"))?;

    let pubkey: [u8; 32] = args.pubkey.0;
    if pubkey == [0u8; 32] {
        return Err(perp_err("registerApiKey: pubkey cannot be zero"));
    }

    storage::save_api_key(context, caller, ApiKey { pubkey, expiry: args.expiry })?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::ApiKeyRegistered {
            user: caller,
            pubkey: args.pubkey,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `revokeApiKey()` — remove the caller's registered API key.
pub fn run_revoke_api_key<CTX: ContextTr>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    revokeApiKeyCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("revokeApiKey: invalid calldata"))?;

    storage::delete_api_key(context, caller)?;

    context.journal_mut().log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::ApiKeyRevoked { user: caller }.to_log_data(),
    });

    Ok(Bytes::new())
}

/// `getApiKey(address user) returns (bytes32 pubkey, uint64 expiry)`
pub fn run_get_api_key<CTX: ContextTr>(
    input_bytes: &[u8],
    context: &mut CTX,
) -> Result<Bytes, PrecompileError> {
    let args = getApiKeyCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getApiKey: invalid calldata"))?;

    let (pubkey, expiry) = storage::load_api_key(context, args.user)?
        .map(|k| (FixedBytes(k.pubkey), k.expiry))
        .unwrap_or_default();

    Ok(Bytes::from(getApiKeyCall::abi_encode_returns(
        &getApiKeyReturn { pubkey, expiry },
    )))
}
