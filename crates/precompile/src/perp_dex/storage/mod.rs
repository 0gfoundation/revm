//! Storage helpers for the PerpDEX precompile.
//!
//! Two layers:
//!   1. `UserAccount` — msgpack-encoded blob stored at the precompile's own
//!      address via the multi-slot `store_bytes` / `load_bytes` helpers.
//!   2. ERC-20 balances — direct `sload` / `sstore` on the token contract's
//!      storage, using the standard OpenZeppelin slot derivation.

pub mod keys;

use context::{ContextTr, JournalTr};
use primitives::{Address, U256};
use rmp_serde::{Deserializer as RMPDeserializer, Serializer as RMPSerializer};
use serde::{Deserialize, Serialize};

use crate::{
    journal::{load_bytes, store_bytes},
    perp_dex::{errors::perp_err, types::UserAccount, PERP_DEX_ADDRESS},
    stateful_precompiles::convert_db_err,
    PrecompileError,
};

use keys::{account_key, erc20_balance_slot};

// ── UserAccount ─────────────────────────────────────────────────────────────

/// Load the `UserAccount` for `user` from the DEX's own storage.
/// Returns a zeroed default if the account has never been written.
pub fn load_account<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
) -> Result<UserAccount, PrecompileError> {
    let buf = load_bytes(context, PERP_DEX_ADDRESS, account_key(user))?;
    if buf.is_empty() {
        return Ok(UserAccount::default());
    }
    let mut de = RMPDeserializer::new(&buf[..]);
    Deserialize::deserialize(&mut de).map_err(|_| perp_err("failed to decode UserAccount"))
}

/// Persist `account` for `user` into the DEX's own storage.
pub fn save_account<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    account: UserAccount,
) -> Result<(), PrecompileError> {
    let mut buf = Vec::new();
    account
        .serialize(&mut RMPSerializer::new(&mut buf).with_struct_map())
        .map_err(|_| perp_err("failed to encode UserAccount"))?;
    store_bytes(context, PERP_DEX_ADDRESS, account_key(user), &buf)
}

// ── ERC-20 balance helpers ───────────────────────────────────────────────────

/// Read `account`'s balance inside the `token` ERC-20 contract.
///
/// Directly reads the token contract's storage rather than making an
/// external call — only possible because this is a native precompile.
pub fn load_erc20_balance<CTX: ContextTr>(
    context: &mut CTX,
    token: Address,
    account: Address,
) -> Result<U256, PrecompileError> {
    let slot = erc20_balance_slot(account);
    let value = context
        .journal_mut()
        .sload(token, slot.into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data;
    Ok(value)
}

/// Write `balance` into `account`'s slot inside the `token` ERC-20 contract.
pub fn save_erc20_balance<CTX: ContextTr>(
    context: &mut CTX,
    token: Address,
    account: Address,
    balance: U256,
) -> Result<(), PrecompileError> {
    let slot = erc20_balance_slot(account);
    context
        .journal_mut()
        .sstore(token, slot.into(), balance)
        .map_err(convert_db_err::<CTX::Db>)?;
    Ok(())
}
