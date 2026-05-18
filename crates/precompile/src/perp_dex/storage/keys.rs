//! Storage key derivation for the PerpDEX precompile.
//!
//! Every storage key is `B256 = keccak256(prefix ++ domain-fields)`.
//! Fixed 4-byte ASCII prefixes prevent cross-domain collisions inside
//! the single `PERP_DEX_ADDRESS` storage space.

use primitives::{keccak256, Address, B256};

// ── Key-family prefixes ───────────────────────────────────────────────────
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
const PFX_ORACLE: &[u8] = b"orcl"; // authorized oracle address (updateIndexPrice role)
const PFX_MARKET_MANAGER: &[u8] = b"mkgr"; // authorized market manager address (addMarket/updateMarket role)
const PFX_INDEX_PRICE: &[u8] = b"idxp"; // per-market IndexPriceState
const PFX_INDEX_HISTORY: &[u8] = b"idxh"; // per-market IndexPriceHistory
const PFX_BASIS_WINDOW: &[u8] = b"bswn"; // per-market PriceBasisWindow (30s mid samples)
const PFX_LAST_TRADED: &[u8] = b"ltrd"; // per-market last traded price (contract price)
const PFX_FUNDING_STATE: &[u8] = b"fund"; // per-market FundingState
const PFX_PREMIUM_ACCUMULATOR: &[u8] = b"pacc"; // per-market PremiumIndexAccumulator
const PFX_INSURANCE_FUND: &[u8] = b"infd"; // global insurance fund balance

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
pub fn oracle_key() -> B256 {
    keccak256(PFX_ORACLE)
}

/// Authorized market manager address (Address; zero = not set).
pub fn market_manager_key() -> B256 {
    keccak256(PFX_MARKET_MANAGER)
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
pub fn insurance_fund_key() -> B256 {
    keccak256(PFX_INSURANCE_FUND)
}
