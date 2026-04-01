//! Storage key derivation for the PerpDEX precompile.
//!
//! All keys are `B256` values derived via `keccak256` so that every module
//! can share a single address space (the `PERP_DEX_ADDRESS` storage) without
//! collision.  The 4-byte prefix distinguishes key families.

use primitives::{keccak256, Address, B256};

// ── Key-family prefixes ─────────────────────────────────────────────────────

/// Prefix for user account storage keys.
const PREFIX_ACCOUNT: &[u8] = b"acct";

// Future prefixes (add as modules are implemented):
// const PREFIX_ORDER:    &[u8] = b"ord\x00";
// const PREFIX_POSITION: &[u8] = b"pos\x00";
// const PREFIX_MARKET:   &[u8] = b"mkt\x00";

// ── Public key constructors ─────────────────────────────────────────────────

/// Storage key for `user`'s `UserAccount` inside the DEX.
///
/// `keccak256(b"acct" ++ user_address)`
pub fn account_key(user: Address) -> B256 {
    keccak256([PREFIX_ACCOUNT, user.as_slice()].concat())
}

/// ERC-20 balance storage slot for `account` inside the `token` contract.
///
/// Standard OpenZeppelin ERC-20 keeps `mapping(address => uint256) _balances`
/// at storage **slot 0**.  The key for a particular address is:
///
/// ```text
/// keccak256(abi.encode(account, uint256(0)))
///         = keccak256( account_padded_32 ++ slot_0_padded_32 )
/// ```
pub fn erc20_balance_slot(account: Address) -> B256 {
    let mut buf = [0u8; 64];
    // address right-aligned in the first word (left-padded with zeros)
    buf[12..32].copy_from_slice(account.as_slice());
    // second word = slot index 0 (already zero-initialised)
    keccak256(buf)
}
