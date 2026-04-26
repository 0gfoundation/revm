//! Storage key derivation for the PerpDEX precompile.
//!
//! Every storage key is `B256 = keccak256(prefix ++ domain-fields)`.
//! Fixed 4-byte ASCII prefixes prevent cross-domain collisions inside
//! the single `PERP_DEX_ADDRESS` storage space.

use primitives::{keccak256, Address, B256};

// ── Key-family prefixes ───────────────────────────────────────────────────
const PFX_ADMIN:        &[u8] = b"admn";
const PFX_ACCOUNT:      &[u8] = b"acct";
const PFX_TRADE_COUNT:  &[u8] = b"tcnt"; // per-market sequential trade ID counter
const PFX_POSITION:     &[u8] = b"pos\x00";
const PFX_BUY_ORDERS:   &[u8] = b"bord";   // per-user buy order entries
const PFX_SELL_ORDERS:  &[u8] = b"sord";   // per-user sell order entries
const PFX_ORDER:        &[u8] = b"ord\x00"; // full Order struct by order_id
const PFX_USER_NONCE:   &[u8] = b"nonc";   // per-user nonce for order-id generation
const PFX_MARKET:       &[u8] = b"mkt\x00";
const PFX_MARK_PRICE:   &[u8] = b"mktp";
const PFX_OPEN_INT:     &[u8] = b"oint";
const PFX_BID_PRICES:   &[u8] = b"bidp";   // sorted Vec<u64> of active bid prices
const PFX_ASK_PRICES:   &[u8] = b"askp";   // sorted Vec<u64> of active ask prices
const PFX_BID_LEVEL:    &[u8] = b"bidl";   // FIFO queue of order IDs at a bid price
const PFX_ASK_LEVEL:    &[u8] = b"askl";   // FIFO queue of order IDs at an ask price
const PFX_BEST_BID:     &[u8] = b"bbd\x00"; // cached best bid price (0 = empty)
const PFX_BEST_ASK:     &[u8] = b"bak\x00"; // cached best ask price (0 = empty)
const PFX_API_KEY:      &[u8] = b"apik";    // per-user ed25519 public key (32 bytes)

// ── ERC-20 helper (shared with deposit/withdraw) ──────────────────────────

/// OpenZeppelin ERC-20 v5 `_balances[account]` storage slot.
///
/// OZ v5 uses EIP-7201 namespaced storage; `_balances` lives inside
/// `ERC20Storage` whose root slot is:
///   keccak256("openzeppelin.storage.ERC20") − 1, rounded down to 256-boundary
///   = 0x52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00
///
/// `_balances` is the first field (offset 0), so it IS at that root slot.
/// For a mapping at slot S, key K resolves to `keccak256(abi.encode(K, S))`.
pub fn erc20_balance_slot(account: Address) -> B256 {
    // ERC20StorageLocation from OZ v5 ERC20.sol
    const OZ_V5_ERC20_STORAGE: [u8; 32] = [
        0x52, 0xc6, 0x32, 0x47, 0xe1, 0xf4, 0x7d, 0xb1,
        0x9d, 0x5c, 0xe0, 0x46, 0x00, 0x30, 0xc4, 0x97,
        0xf0, 0x67, 0xca, 0x4c, 0xeb, 0xf7, 0x1b, 0xa9,
        0x8e, 0xea, 0xda, 0xbe, 0x20, 0xba, 0xce, 0x00,
    ];
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(account.as_slice()); // left-pad address to 32 bytes
    buf[32..64].copy_from_slice(&OZ_V5_ERC20_STORAGE); // mapping slot
    keccak256(buf)
}

// ── Admin ─────────────────────────────────────────────────────────────────

/// Single slot storing the admin address (20 bytes, zero = uninitialized).
pub fn admin_key() -> B256 {
    keccak256(PFX_ADMIN)
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
    keccak256([PFX_BID_LEVEL, &market_id.to_be_bytes(), &price.to_be_bytes()].concat())
}

/// FIFO queue of order IDs at a specific ask price level.
pub fn ask_level_key(market_id: u64, price: u64) -> B256 {
    keccak256([PFX_ASK_LEVEL, &market_id.to_be_bytes(), &price.to_be_bytes()].concat())
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

/// ed25519 public key registered by a user for signed order submission.
pub fn api_key_key(user: Address) -> B256 {
    keccak256([PFX_API_KEY, user.as_slice()].concat())
}