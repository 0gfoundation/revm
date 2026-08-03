//! Storage key derivation for the PerpDEX precompile.
//!
//! Off-trie PerpState keys are **HashMap keys, not state-trie keys**, so they need no
//! cryptographic derivation — only a deterministic, collision-free encoding. Each key is
//! packed directly as `prefix(4) ++ fixed-layout big-endian fields ++ zero-pad` into 32 bytes
//! (catalog #12). This is **injective by construction**: distinct 4-byte prefixes separate
//! namespaces, and fixed-size fields separate entries within a namespace — strictly stronger
//! than keccak's probabilistic collision-resistance, and far cheaper (a `memcpy`, no hash, no
//! heap alloc).
//!
//! Two deliberate exceptions:
//! * [`order_key`] is the **raw 32-byte order id** (no room for a prefix). The order id is
//!   itself a preimage-resistant keccak output, so it cannot be steered onto any structured
//!   (prefixed) key — the same separation the EVM relies on between mapping and scalar slots.
//! * [`erc20_balance_slot`] stays `keccak256(...)` because it addresses a **real ERC-20
//!   contract's on-trie storage** (MockUSDC `_balances`), a different store entirely.
//!
//! Changing this layout moves every off-trie key and the block commitment → it is a CHAIN
//! change (requires a chain wipe + golden re-pin + `BLOCK_COMMITMENT_VERSION` bump).

use primitives::{keccak256, Address, B256};

// ── Key-family prefixes (4-byte, ASCII, must be pairwise distinct) ─────────────
const PFX_ADMIN: [u8; 4] = *b"admn";
const PFX_ACCOUNT: [u8; 4] = *b"acct"; // per-user account (balance + folded fee bps + order nonce)
const PFX_MARKET_FEE_TOTAL: [u8; 4] = *b"mfee"; // per-market collected trading fee total
const PFX_TRADE_COUNT: [u8; 4] = *b"tcnt"; // per-market sequential trade ID counter
const PFX_POSITION: [u8; 4] = *b"pos\x00";
const PFX_BUY_ORDERS: [u8; 4] = *b"bord"; // per-user buy order entries
const PFX_SELL_ORDERS: [u8; 4] = *b"sord"; // per-user sell order entries
const PFX_MARKET: [u8; 4] = *b"mkt\x00";
const PFX_MARKET_HOT: [u8; 4] = *b"mhot"; // per-market grouped hot scalars (MarketHot): mark/BBO/last/OI
const PFX_POSITION_REGISTRY: [u8; 4] = *b"preg"; // per-market set of addresses with an open position
const PFX_BID_PRICES: [u8; 4] = *b"bidp"; // sorted Vec<u64> of active bid prices
const PFX_ASK_PRICES: [u8; 4] = *b"askp"; // sorted Vec<u64> of active ask prices
const PFX_BID_LEVEL: [u8; 4] = *b"bidl"; // FIFO queue of order IDs at a bid price
const PFX_ASK_LEVEL: [u8; 4] = *b"askl"; // FIFO queue of order IDs at an ask price
const PFX_API_KEY: [u8; 4] = *b"apik"; // per-user per-slot ed25519 key
const PFX_API_KEY_IDS: [u8; 4] = *b"akid"; // per-user list of registered key_ids
const PFX_ORACLE: [u8; 4] = *b"orcl"; // authorized oracle address (updateIndexPrice role)
const PFX_MARKET_MANAGER: [u8; 4] = *b"mkgr"; // authorized market manager address (addMarket/updateMarket role)
const PFX_INDEX_PRICE: [u8; 4] = *b"idxp"; // per-market IndexPriceState
const PFX_INDEX_HISTORY: [u8; 4] = *b"idxh"; // per-market IndexPriceHistory
const PFX_BASIS_WINDOW: [u8; 4] = *b"bswn"; // per-market PriceBasisWindow (30s mid samples)
const PFX_FUNDING_STATE: [u8; 4] = *b"fund"; // per-market FundingState
const PFX_PREMIUM_ACCUMULATOR: [u8; 4] = *b"pacc"; // per-market PremiumIndexAccumulator
const PFX_INSURANCE_FUND: [u8; 4] = *b"infd"; // global insurance fund balance
const PFX_COMMITMENT: [u8; 4] = *b"cmit"; // global on-trie commitment anchor slot
const PFX_SEEN_SIG: [u8; 4] = *b"seen"; // signed-order replay guard: seen signature markers
const PFX_SEEN_BUCKET: [u8; 4] = *b"snbk"; // time-bucketed index of seen-sig keys (for GC)

// ── Packing helpers (catalog #12: pack, don't hash) ───────────────────────────
// All produce `prefix ++ fields ++ zero-pad` in a stack `[u8; 32]` — no hash, no heap alloc.
// The field layouts are fixed-size + big-endian, so the packed bytes are deterministic across
// validators and enter the block commitment unchanged in structure.

/// `prefix(4) ++ 28 zero` — a parameterless global slot.
const fn packed_const(prefix: [u8; 4]) -> B256 {
    let mut buf = [0u8; 32];
    buf[0] = prefix[0];
    buf[1] = prefix[1];
    buf[2] = prefix[2];
    buf[3] = prefix[3];
    B256::new(buf)
}

/// `prefix(4) ++ market_id(8 BE) ++ 20 zero`.
#[inline]
fn pack_market(prefix: [u8; 4], market_id: u64) -> B256 {
    let mut buf = [0u8; 32];
    buf[..4].copy_from_slice(&prefix);
    buf[4..12].copy_from_slice(&market_id.to_be_bytes());
    B256::new(buf)
}

/// `prefix(4) ++ address(20) ++ 8 zero`.
#[inline]
fn pack_addr(prefix: [u8; 4], user: Address) -> B256 {
    let mut buf = [0u8; 32];
    buf[..4].copy_from_slice(&prefix);
    buf[4..24].copy_from_slice(user.as_slice());
    B256::new(buf)
}

/// `prefix(4) ++ address(20) ++ market_id(8 BE)` — fills all 32 bytes (the tightest key).
#[inline]
fn pack_addr_market(prefix: [u8; 4], user: Address, market_id: u64) -> B256 {
    let mut buf = [0u8; 32];
    buf[..4].copy_from_slice(&prefix);
    buf[4..24].copy_from_slice(user.as_slice());
    buf[24..32].copy_from_slice(&market_id.to_be_bytes());
    B256::new(buf)
}

/// `prefix(4) ++ market_id(8 BE) ++ price(8 BE) ++ 12 zero`.
#[inline]
fn pack_market_price(prefix: [u8; 4], market_id: u64, price: u64) -> B256 {
    let mut buf = [0u8; 32];
    buf[..4].copy_from_slice(&prefix);
    buf[4..12].copy_from_slice(&market_id.to_be_bytes());
    buf[12..20].copy_from_slice(&price.to_be_bytes());
    B256::new(buf)
}

/// `prefix(4) ++ address(20) ++ key_id(1) ++ 7 zero`.
#[inline]
fn pack_addr_u8(prefix: [u8; 4], user: Address, key_id: u8) -> B256 {
    let mut buf = [0u8; 32];
    buf[..4].copy_from_slice(&prefix);
    buf[4..24].copy_from_slice(user.as_slice());
    buf[24] = key_id;
    B256::new(buf)
}

// ── ERC-20 helper (on-trie, external contract — stays keccak) ──────────────────

/// Standard OpenZeppelin ERC-20 `_balances[account]` storage slot.
///
/// Both OZ v4 and OZ v5 non-upgradeable ERC20 store `_balances` as the first
/// state variable (slot 0).  EIP-7201 namespaced storage is only used by
/// `ERC20Upgradeable.sol`, not by the standard `ERC20.sol`.
///
/// Slot = `keccak256(abi.encode(account, uint256(0)))`. This addresses a REAL ERC-20
/// contract's on-trie storage (not the off-trie perp store), so it MUST stay keccak to match
/// the Solidity mapping layout — it is a different store and never collides with packed
/// off-trie keys.
#[inline]
pub fn erc20_balance_slot(account: Address) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(account.as_slice()); // left-pad address to 32 bytes
                                                     // buf[32..64] stays zero → mapping at slot 0
    keccak256(buf)
}

// ── Commitment anchor (on-trie slot under 0x1003) ──────────────────────────────

/// Global on-trie storage slot under 0x1003 holding the chained commitment over the off-trie
/// PerpState write-stream. Anchored ON the state trie so any perp-write divergence surfaces in
/// the state root and is caught by consensus. 0x1003's on-trie storage holds ONLY this slot, so
/// packing it (`"cmit" ++ zero`) cannot collide with anything.
///
/// Invariant: written ONLY by `storage::flush_commitment` (the per-call fold flush); the
/// accumulator seeds from a single `sload` of this slot.
pub const COMMITMENT_SLOT: B256 = packed_const(PFX_COMMITMENT);

/// See [`COMMITMENT_SLOT`].
#[inline]
pub fn commitment_slot() -> B256 {
    COMMITMENT_SLOT
}

// ── Admin / roles (parameterless global slots) ─────────────────────────────────

/// Single slot storing the admin address (20 bytes, zero = uninitialized).
pub const ADMIN_KEY: B256 = packed_const(PFX_ADMIN);

/// See [`ADMIN_KEY`].
#[inline]
pub fn admin_key() -> B256 {
    ADMIN_KEY
}

// ── Global counters ───────────────────────────────────────────────────────────

/// Per-market sequential trade ID counter.
#[inline]
pub fn trade_count_key(market_id: u64) -> B256 {
    pack_market(PFX_TRADE_COUNT, market_id)
}

// ── Account ─────────────────────────────────────────────────────────────────

#[inline]
pub fn account_key(user: Address) -> B256 {
    pack_addr(PFX_ACCOUNT, user)
}

// Fee rates + order nonce are folded into the account blob (see [`account_key`]) — no own keys.

#[inline]
pub fn market_fee_total_key(market_id: u64) -> B256 {
    pack_market(PFX_MARKET_FEE_TOTAL, market_id)
}

// ── Perp position ─────────────────────────────────────────────────────────────

#[inline]
pub fn position_key(user: Address, market_id: u64) -> B256 {
    pack_addr_market(PFX_POSITION, user, market_id)
}

/// Buy-order entries for a user in a market (Vec<OrderEntry>, sorted price DESC).
#[inline]
pub fn user_buy_orders_key(user: Address, market_id: u64) -> B256 {
    pack_addr_market(PFX_BUY_ORDERS, user, market_id)
}

/// Sell-order entries for a user in a market (Vec<OrderEntry>, sorted price ASC).
#[inline]
pub fn user_sell_orders_key(user: Address, market_id: u64) -> B256 {
    pack_addr_market(PFX_SELL_ORDERS, user, market_id)
}

// ── Orders ──────────────────────────────────────────────────────────────────

/// Full `Order` struct keyed by 32-byte order ID.
///
/// UNLIKE every other off-trie key this is the **raw `order_id`** — no prefix, no hash. The
/// order id is itself a preimage-resistant keccak output (`keccak256(account ‖ nonce)` for
/// `placeOrder`, `keccak256(signature)` for `placeOrderSigned`), so it is a uniformly
/// distributed 32-byte value that cannot be steered onto any structured (prefixed) key —
/// exactly how the EVM separates mapping slots from scalar slots.
///
/// INVARIANT: order-id generation MUST remain a preimage-resistant hash. If it ever becomes
/// low-entropy (e.g. a raw counter), this key needs a namespace tag or its own hash again,
/// otherwise a crafted order id could collide with a structured key.
#[inline]
pub fn order_key(order_id: &[u8; 32]) -> B256 {
    B256::new(*order_id)
}

// ── Market ────────────────────────────────────────────────────────────────────

#[inline]
pub fn market_key(market_id: u64) -> B256 {
    pack_market(PFX_MARKET, market_id)
}

/// Per-market grouped hot scalars (`MarketHot`): mark price, best bid/ask, last traded, open
/// interest. Replaces the five former single-scalar keys with one, so a co-access is one probe.
#[inline]
pub fn market_hot_key(market_id: u64) -> B256 {
    pack_market(PFX_MARKET_HOT, market_id)
}

/// Per-market set of addresses holding an open position (packed 20-byte
/// addresses). Maintained by the `save_position` zero-crossing hook; enumerated
/// by the liquidation sweep.
#[inline]
pub fn position_registry_key(market_id: u64) -> B256 {
    pack_market(PFX_POSITION_REGISTRY, market_id)
}

// ── Order book ────────────────────────────────────────────────────────────────

/// Sorted list of all active **bid** prices for a market (Vec<u64>, price DESC).
#[inline]
pub fn bid_prices_key(market_id: u64) -> B256 {
    pack_market(PFX_BID_PRICES, market_id)
}

/// Sorted list of all active **ask** prices for a market (Vec<u64>, price ASC).
#[inline]
pub fn ask_prices_key(market_id: u64) -> B256 {
    pack_market(PFX_ASK_PRICES, market_id)
}

/// FIFO queue of order IDs at a specific bid price level.
#[inline]
pub fn bid_level_key(market_id: u64, price: u64) -> B256 {
    pack_market_price(PFX_BID_LEVEL, market_id, price)
}

/// FIFO queue of order IDs at a specific ask price level.
#[inline]
pub fn ask_level_key(market_id: u64, price: u64) -> B256 {
    pack_market_price(PFX_ASK_LEVEL, market_id, price)
}

// The per-level live-order count now lives INSIDE the level blob (see storage `LevelBlob`) — no
// separate count key.

// Best bid/ask, last-traded, open-interest, and mark price now live in the grouped
// [`market_hot_key`] blob (`MarketHot`) — no per-scalar keys.

// ── API key (ed25519 signed orders) ────────────────────────────────────────────

/// ed25519 key for a specific (user, key_id) slot.
#[inline]
pub fn api_key_key(user: Address, key_id: u8) -> B256 {
    pack_addr_u8(PFX_API_KEY, user, key_id)
}

/// List of registered key_ids for a user (Vec<u8>).
#[inline]
pub fn api_key_ids_key(user: Address) -> B256 {
    pack_addr(PFX_API_KEY_IDS, user)
}

// ── Oracle / market-manager roles ──────────────────────────────────────────────

/// Authorized oracle address (Address; zero = not set).
pub const ORACLE_KEY: B256 = packed_const(PFX_ORACLE);

/// See [`ORACLE_KEY`].
#[inline]
pub fn oracle_key() -> B256 {
    ORACLE_KEY
}

/// Authorized market manager address (Address; zero = not set).
pub const MARKET_MANAGER_KEY: B256 = packed_const(PFX_MARKET_MANAGER);

/// See [`MARKET_MANAGER_KEY`].
#[inline]
pub fn market_manager_key() -> B256 {
    MARKET_MANAGER_KEY
}

/// Per-market IndexPriceState (index_price + timestamp).
#[inline]
pub fn index_price_state_key(market_id: u64) -> B256 {
    pack_market(PFX_INDEX_PRICE, market_id)
}

/// Per-market recent index price checkpoints.
#[inline]
pub fn index_price_history_key(market_id: u64) -> B256 {
    pack_market(PFX_INDEX_HISTORY, market_id)
}

/// Per-market PriceBasisWindow (30-second mid-price ring buffer).
#[inline]
pub fn price_basis_window_key(market_id: u64) -> B256 {
    pack_market(PFX_BASIS_WINDOW, market_id)
}

/// Per-market FundingState (last rate, interval, next timestamp).
#[inline]
pub fn funding_state_key(market_id: u64) -> B256 {
    pack_market(PFX_FUNDING_STATE, market_id)
}

/// Per-market PremiumIndexAccumulator (linearly-weighted premium index for funding).
#[inline]
pub fn premium_accumulator_key(market_id: u64) -> B256 {
    pack_market(PFX_PREMIUM_ACCUMULATOR, market_id)
}

// ── Insurance Fund ──────────────────────────────────────────────────────────────

/// Global insurance fund balance (u64, USDC micro-units).
pub const INSURANCE_FUND_KEY: B256 = packed_const(PFX_INSURANCE_FUND);

/// See [`INSURANCE_FUND_KEY`].
#[inline]
pub fn insurance_fund_key() -> B256 {
    INSURANCE_FUND_KEY
}

// ── Signed-order replay guard (seen-signature set) ─────────────────────────────
// Decoupled from the order map: with delete-on-terminal the order-id is no longer a durable
// replay witness (a filled/cancelled signed order is deleted), so a signature's replay guard lives
// in its own namespace. `keccak256(signature)` is already computed for the order id, so the seen
// key REUSES it (no extra hash) — a `prefix(4) ++ hash[..28]` direct-pack. 28-byte truncation of a
// preimage-resistant hash keeps ~2^112 collision resistance, ample for a replay marker; the prefix
// separates it from the raw-keccak order-id namespace so the two can never alias.

/// Replay-guard key for a signed order, derived from `keccak256(signature)` (`sig_hash`).
#[inline]
pub fn seen_sig_key(sig_hash: &[u8; 32]) -> B256 {
    let mut buf = [0u8; 32];
    buf[..4].copy_from_slice(&PFX_SEEN_SIG);
    buf[4..32].copy_from_slice(&sig_hash[..28]);
    B256::new(buf)
}

/// Time bucket (`timestamp / bucket_width`) holding the seen-sig keys recorded in that window, as a
/// raw-packed `Vec<[u8;32]>` (like a level FIFO). Enumerated ONLY by the lazy GC, which drops whole
/// expired buckets; never consulted on the replay-check hot path.
#[inline]
pub fn seen_bucket_key(bucket_id: u64) -> B256 {
    pack_market(PFX_SEEN_BUCKET, bucket_id)
}

#[cfg(test)]
mod const_key_tests {
    use super::*;

    /// Cross-namespace injectivity depends on prefixes being pairwise distinct.
    #[test]
    fn prefixes_distinct() {
        let all: &[[u8; 4]] = &[
            PFX_ADMIN,
            PFX_ACCOUNT,
            PFX_MARKET_FEE_TOTAL,
            PFX_TRADE_COUNT,
            PFX_POSITION,
            PFX_BUY_ORDERS,
            PFX_SELL_ORDERS,
            PFX_MARKET,
            PFX_MARKET_HOT,
            PFX_POSITION_REGISTRY,
            PFX_BID_PRICES,
            PFX_ASK_PRICES,
            PFX_BID_LEVEL,
            PFX_ASK_LEVEL,
            PFX_API_KEY,
            PFX_API_KEY_IDS,
            PFX_ORACLE,
            PFX_MARKET_MANAGER,
            PFX_INDEX_PRICE,
            PFX_INDEX_HISTORY,
            PFX_BASIS_WINDOW,
            PFX_FUNDING_STATE,
            PFX_PREMIUM_ACCUMULATOR,
            PFX_INSURANCE_FUND,
            PFX_COMMITMENT,
            PFX_SEEN_SIG,
            PFX_SEEN_BUCKET,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "duplicate prefix at {i},{j}");
            }
        }
    }

    /// Known-answer pins. A layout/prefix change moves every key + the golden commitment (a
    /// CHAIN change); this catches an ACCIDENTAL one. Regenerate deliberately on a real change.
    #[test]
    fn known_answer_pins() {
        let a = Address::from([0xABu8; 20]);

        // account = "acct" ++ addr ++ 8 zero
        let mut want = [0u8; 32];
        want[..4].copy_from_slice(b"acct");
        want[4..24].copy_from_slice(a.as_slice());
        assert_eq!(account_key(a), B256::new(want));

        // position = "pos\0" ++ addr ++ market(8) — fills all 32 bytes (tightest key)
        let mut wp = [0u8; 32];
        wp[..4].copy_from_slice(b"pos\x00");
        wp[4..24].copy_from_slice(a.as_slice());
        wp[24..32].copy_from_slice(&7u64.to_be_bytes());
        assert_eq!(position_key(a, 7), B256::new(wp));

        // market = "mkt\0" ++ market(8) ++ 20 zero
        let mut wm = [0u8; 32];
        wm[..4].copy_from_slice(b"mkt\x00");
        wm[4..12].copy_from_slice(&7u64.to_be_bytes());
        assert_eq!(market_key(7), B256::new(wm));

        // bid_level = "bidl" ++ market(8) ++ price(8) ++ 12 zero
        let mut wl = [0u8; 32];
        wl[..4].copy_from_slice(b"bidl");
        wl[4..12].copy_from_slice(&7u64.to_be_bytes());
        wl[12..20].copy_from_slice(&100u64.to_be_bytes());
        assert_eq!(bid_level_key(7, 100), B256::new(wl));

        // order = raw id (no prefix, no hash)
        let id = [0x42u8; 32];
        assert_eq!(order_key(&id), B256::new(id));

        // parameterless globals = prefix ++ 28 zero
        let mut wadmin = [0u8; 32];
        wadmin[..4].copy_from_slice(b"admn");
        assert_eq!(admin_key(), B256::new(wadmin));
        let mut wcmit = [0u8; 32];
        wcmit[..4].copy_from_slice(b"cmit");
        assert_eq!(commitment_slot(), B256::new(wcmit));
    }

    /// Same fields under different families must differ; the raw-order namespace must not equal
    /// any structured key (spot-check — full injectivity holds by construction).
    #[test]
    fn no_cross_family_collision() {
        let a = Address::from([0x22u8; 20]);
        assert_ne!(position_key(a, 3), user_buy_orders_key(a, 3));
        assert_ne!(position_key(a, 3), user_sell_orders_key(a, 3));
        assert_ne!(user_buy_orders_key(a, 3), user_sell_orders_key(a, 3));
        assert_ne!(market_key(3), market_hot_key(3));
        assert_ne!(bid_prices_key(3), ask_prices_key(3));
        assert_ne!(bid_level_key(3, 100), ask_level_key(3, 100));
        // raw order id vs a structured key
        let id = [0x01u8; 32];
        assert_ne!(order_key(&id), position_key(a, 3));
    }
}
