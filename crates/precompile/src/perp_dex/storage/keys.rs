//! Storage key derivation for the PerpDEX precompile.
//!
//! Every storage key is `B256 = keccak256(prefix ++ domain-fields)`.
//! Fixed 4-byte ASCII prefixes prevent cross-domain collisions within the
//! off-trie PerpDEX key space (the journal's perp section). These keys used to
//! address slots under `PERP_DEX_ADDRESS` in the state trie; perp blobs now live
//! off-trie, but the key derivation is unchanged.

use primitives::{b256, keccak256, Address, B256};

// ── Key-family prefixes ───────────────────────────────────────────────────
// The five parameterless prefixes (admn/orcl/mkgr/infd/cmit) are folded into
// precomputed `*_KEY` constants below; the prefix consts are kept so the
// `const_key_tests` pin can re-derive and compare.
#[cfg_attr(not(test), allow(dead_code))]
const PFX_ADMIN: &[u8] = b"admn";
const PFX_ACCOUNT: &[u8] = b"acct";
const PFX_USER_FEE: &[u8] = b"ufee";
const PFX_MARKET_FEE_TOTAL: &[u8] = b"mfee"; // per-market collected trading fee total
const PFX_TRADE_COUNT: &[u8] = b"tcnt"; // per-market sequential trade ID counter
const PFX_POSITION: &[u8] = b"pos\x00";
const PFX_BUY_ORDERS: &[u8] = b"bord"; // per-user buy order entries
const PFX_SELL_ORDERS: &[u8] = b"sord"; // per-user sell order entries
const PFX_ORDER: &[u8] = b"ord\x00"; // full Order struct by order_id
const PFX_USER_NONCE: &[u8] = b"nonc"; // per-user nonce for order-id generation
const PFX_MARKET: &[u8] = b"mkt\x00";
const PFX_MARK_PRICE: &[u8] = b"mktp";
const PFX_OPEN_INT: &[u8] = b"oint";
const PFX_BID_PRICES: &[u8] = b"bidp"; // sorted Vec<u64> of active bid prices
const PFX_ASK_PRICES: &[u8] = b"askp"; // sorted Vec<u64> of active ask prices
const PFX_BID_LEVEL: &[u8] = b"bidl"; // FIFO queue of order IDs at a bid price
const PFX_ASK_LEVEL: &[u8] = b"askl"; // FIFO queue of order IDs at an ask price
const PFX_BEST_BID: &[u8] = b"bbd\x00"; // cached best bid price (0 = empty)
const PFX_BEST_ASK: &[u8] = b"bak\x00"; // cached best ask price (0 = empty)
const PFX_API_KEY: &[u8] = b"apik"; // per-user per-slot ed25519 key
const PFX_API_KEY_IDS: &[u8] = b"akid"; // per-user list of registered key_ids
#[cfg_attr(not(test), allow(dead_code))]
const PFX_ORACLE: &[u8] = b"orcl"; // authorized oracle address (updateIndexPrice role)
#[cfg_attr(not(test), allow(dead_code))]
const PFX_MARKET_MANAGER: &[u8] = b"mkgr"; // authorized market manager address (addMarket/updateMarket role)
const PFX_INDEX_PRICE: &[u8] = b"idxp"; // per-market IndexPriceState
const PFX_INDEX_HISTORY: &[u8] = b"idxh"; // per-market IndexPriceHistory
const PFX_BASIS_WINDOW: &[u8] = b"bswn"; // per-market PriceBasisWindow (30s mid samples)
const PFX_LAST_TRADED: &[u8] = b"ltrd"; // per-market last traded price (contract price)
const PFX_FUNDING_STATE: &[u8] = b"fund"; // per-market FundingState
const PFX_PREMIUM_ACCUMULATOR: &[u8] = b"pacc"; // per-market PremiumIndexAccumulator
#[cfg_attr(not(test), allow(dead_code))]
const PFX_INSURANCE_FUND: &[u8] = b"infd"; // global insurance fund balance
#[cfg_attr(not(test), allow(dead_code))]
const PFX_COMMITMENT: &[u8] = b"cmit"; // global on-trie commitment over the off-trie perp write-stream

// ── ERC-20 helper (shared with deposit/withdraw) ──────────────────────────

/// Standard OpenZeppelin ERC-20 `_balances[account]` storage slot.
///
/// Both OZ v4 and OZ v5 non-upgradeable ERC20 store `_balances` as the first
/// state variable (slot 0).  EIP-7201 namespaced storage is only used by
/// `ERC20Upgradeable.sol`, not by the standard `ERC20.sol`.
///
/// Slot = `keccak256(abi.encode(account, uint256(0)))`.
pub fn erc20_balance_slot(account: Address) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(account.as_slice()); // left-pad address to 32 bytes
                                                     // buf[32..64] stays zero → mapping at slot 0
    keccak256(buf)
}

/// Global on-trie storage slot under 0x1003 holding the chained keccak commitment over the
/// off-trie PerpState write-stream. It is anchored ON the state trie (a normal account-storage
/// slot, distinct from the off-trie B256 domain keys and from the erc20 balance slots) so that
/// any perp-write divergence surfaces in the state root and is detected by consensus.
///
/// Precomputed `keccak256(b"cmit")` — this is hashed on every `store_blob` fold, so it must
/// not be recomputed per call. Pinned against the live derivation in `const_key_tests`.
pub const COMMITMENT_SLOT: B256 =
    b256!("0x5315529dd419e7000541b58e86740e824fd9f29774b5ca4423b92157a3c38b37");

/// See [`COMMITMENT_SLOT`].
#[inline]
pub fn commitment_slot() -> B256 {
    COMMITMENT_SLOT
}

// ── Admin ─────────────────────────────────────────────────────────────────

/// Single slot storing the admin address (20 bytes, zero = uninitialized).
///
/// Precomputed `keccak256(b"admn")`, pinned in `const_key_tests`.
pub const ADMIN_KEY: B256 =
    b256!("0x0cd72d51fc618f526ac2c21501d7abd9edf6395566bb827168c6d03aa2596b61");

/// See [`ADMIN_KEY`].
#[inline]
pub fn admin_key() -> B256 {
    ADMIN_KEY
}

// ── Global counters ───────────────────────────────────────────────────────

/// Per-market sequential trade ID counter.
pub fn trade_count_key(market_id: u64) -> B256 {
    keccak256([PFX_TRADE_COUNT, &market_id.to_be_bytes()].concat())
}

// ── Account ───────────────────────────────────────────────────────────────

pub fn account_key(user: Address) -> B256 {
    keccak256([PFX_ACCOUNT, user.as_slice()].concat())
}

pub fn user_fee_rates_key(user: Address) -> B256 {
    keccak256([PFX_USER_FEE, user.as_slice()].concat())
}

pub fn market_fee_total_key(market_id: u64) -> B256 {
    keccak256([PFX_MARKET_FEE_TOTAL, &market_id.to_be_bytes()].concat())
}

// ── Perp position ─────────────────────────────────────────────────────────

pub fn position_key(user: Address, market_id: u64) -> B256 {
    keccak256([PFX_POSITION, user.as_slice(), &market_id.to_be_bytes()].concat())
}

/// Buy-order entries for a user in a market (Vec<OrderEntry>, sorted price DESC).
pub fn user_buy_orders_key(user: Address, market_id: u64) -> B256 {
    keccak256([PFX_BUY_ORDERS, user.as_slice(), &market_id.to_be_bytes()].concat())
}

/// Sell-order entries for a user in a market (Vec<OrderEntry>, sorted price ASC).
pub fn user_sell_orders_key(user: Address, market_id: u64) -> B256 {
    keccak256([PFX_SELL_ORDERS, user.as_slice(), &market_id.to_be_bytes()].concat())
}

// ── Orders ────────────────────────────────────────────────────────────────

/// Full `Order` struct keyed by 32-byte order ID.
pub fn order_key(order_id: &[u8; 32]) -> B256 {
    keccak256([PFX_ORDER, order_id.as_slice()].concat())
}

/// Per-user nonce used to derive unique order IDs.
pub fn user_nonce_key(user: Address) -> B256 {
    keccak256([PFX_USER_NONCE, user.as_slice()].concat())
}

// ── Market ────────────────────────────────────────────────────────────────

pub fn market_key(market_id: u64) -> B256 {
    keccak256([PFX_MARKET, &market_id.to_be_bytes()].concat())
}

pub fn mark_price_key(market_id: u64) -> B256 {
    keccak256([PFX_MARK_PRICE, &market_id.to_be_bytes()].concat())
}

pub fn open_interest_key(market_id: u64) -> B256 {
    keccak256([PFX_OPEN_INT, &market_id.to_be_bytes()].concat())
}

// ── Order book ────────────────────────────────────────────────────────────

/// Sorted list of all active **bid** prices for a market (Vec<u64>, price DESC).
pub fn bid_prices_key(market_id: u64) -> B256 {
    keccak256([PFX_BID_PRICES, &market_id.to_be_bytes()].concat())
}

/// Sorted list of all active **ask** prices for a market (Vec<u64>, price ASC).
pub fn ask_prices_key(market_id: u64) -> B256 {
    keccak256([PFX_ASK_PRICES, &market_id.to_be_bytes()].concat())
}

/// FIFO queue of order IDs at a specific bid price level.
pub fn bid_level_key(market_id: u64, price: u64) -> B256 {
    keccak256(
        [
            PFX_BID_LEVEL,
            &market_id.to_be_bytes(),
            &price.to_be_bytes(),
        ]
        .concat(),
    )
}

/// FIFO queue of order IDs at a specific ask price level.
pub fn ask_level_key(market_id: u64, price: u64) -> B256 {
    keccak256(
        [
            PFX_ASK_LEVEL,
            &market_id.to_be_bytes(),
            &price.to_be_bytes(),
        ]
        .concat(),
    )
}

/// Cached best bid price for a market (0 = no bids).
pub fn best_bid_key(market_id: u64) -> B256 {
    keccak256([PFX_BEST_BID, &market_id.to_be_bytes()].concat())
}

/// Cached best ask price for a market (0 = no asks).
pub fn best_ask_key(market_id: u64) -> B256 {
    keccak256([PFX_BEST_ASK, &market_id.to_be_bytes()].concat())
}

// ── API key (ed25519 signed orders) ──────────────────────────────────────────

/// ed25519 key for a specific (user, key_id) slot.
pub fn api_key_key(user: Address, key_id: u8) -> B256 {
    keccak256([PFX_API_KEY, user.as_slice(), &[key_id]].concat())
}

/// List of registered key_ids for a user (Vec<u8>).
pub fn api_key_ids_key(user: Address) -> B256 {
    keccak256([PFX_API_KEY_IDS, user.as_slice()].concat())
}

// ── Oracle price feed ─────────────────────────────────────────────────────────

/// Authorized oracle address (Address; zero = not set).
///
/// Precomputed `keccak256(b"orcl")`, pinned in `const_key_tests`.
pub const ORACLE_KEY: B256 =
    b256!("0xd411ab2cb54ccbef75296e12fdcf4fa6caf9f2b8da09908875765e8ae2c02c24");

/// See [`ORACLE_KEY`].
#[inline]
pub fn oracle_key() -> B256 {
    ORACLE_KEY
}

/// Authorized market manager address (Address; zero = not set).
///
/// Precomputed `keccak256(b"mkgr")`, pinned in `const_key_tests`.
pub const MARKET_MANAGER_KEY: B256 =
    b256!("0xe2693bd7dc3c7bbb81d1b8591a1f844a3a1f4be6bde409c947fffc86c87163bd");

/// See [`MARKET_MANAGER_KEY`].
#[inline]
pub fn market_manager_key() -> B256 {
    MARKET_MANAGER_KEY
}

/// Per-market IndexPriceState (index_price + timestamp).
pub fn index_price_state_key(market_id: u64) -> B256 {
    keccak256([PFX_INDEX_PRICE, &market_id.to_be_bytes()].concat())
}

/// Per-market recent index price checkpoints.
pub fn index_price_history_key(market_id: u64) -> B256 {
    keccak256([PFX_INDEX_HISTORY, &market_id.to_be_bytes()].concat())
}

/// Per-market PriceBasisWindow (30-second mid-price ring buffer).
pub fn price_basis_window_key(market_id: u64) -> B256 {
    keccak256([PFX_BASIS_WINDOW, &market_id.to_be_bytes()].concat())
}

/// Per-market last traded price (the "contract price" input to mark price median).
pub fn last_traded_price_key(market_id: u64) -> B256 {
    keccak256([PFX_LAST_TRADED, &market_id.to_be_bytes()].concat())
}

/// Per-market FundingState (last rate, interval, next timestamp).
pub fn funding_state_key(market_id: u64) -> B256 {
    keccak256([PFX_FUNDING_STATE, &market_id.to_be_bytes()].concat())
}

/// Per-market PremiumIndexAccumulator (linearly-weighted premium index for funding).
pub fn premium_accumulator_key(market_id: u64) -> B256 {
    keccak256([PFX_PREMIUM_ACCUMULATOR, &market_id.to_be_bytes()].concat())
}

// ── Insurance Fund ────────────────────────────────────────────────────────────

/// Global insurance fund balance (u64, USDC micro-units).
///
/// Precomputed `keccak256(b"infd")`, pinned in `const_key_tests`.
pub const INSURANCE_FUND_KEY: B256 =
    b256!("0x292dee8007df30a0d76dd66c314b3df92655b9311e95dcbf55734d3b9f3ea8e7");

/// See [`INSURANCE_FUND_KEY`].
#[inline]
pub fn insurance_fund_key() -> B256 {
    INSURANCE_FUND_KEY
}

#[cfg(test)]
mod const_key_tests {
    use super::*;

    /// Pins every precomputed key constant against its live keccak derivation.
    /// A mistyped constant here would silently move a storage key (and, for
    /// `COMMITMENT_SLOT`, the consensus-visible anchor slot under 0x1003) —
    /// the golden commitment test guards the same thing end-to-end.
    #[test]
    fn precomputed_constants_match_derivation() {
        assert_eq!(COMMITMENT_SLOT, keccak256(PFX_COMMITMENT), "cmit");
        assert_eq!(ADMIN_KEY, keccak256(PFX_ADMIN), "admn");
        assert_eq!(ORACLE_KEY, keccak256(PFX_ORACLE), "orcl");
        assert_eq!(MARKET_MANAGER_KEY, keccak256(PFX_MARKET_MANAGER), "mkgr");
        assert_eq!(INSURANCE_FUND_KEY, keccak256(PFX_INSURANCE_FUND), "infd");
    }
}
