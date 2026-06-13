//! Storage helpers for the PerpDEX precompile.

pub mod keys;

use context::{ContextTr, JournalTr};
use primitives::{Address, HashMap, B256, U256};
use rmp_serde::{Deserializer as RMPDeserializer, Serializer as RMPSerializer};
use serde::{Deserialize, Serialize};

use crate::perp_dex::PERP_DEX_ADDRESS;
use crate::{
    perp_dex::{
        errors::perp_err,
        types::{
            ApiKey, FundingState, IndexPriceHistory, IndexPriceState, Market, Order, OrderEntry,
            PerpPosition, PremiumIndexAccumulator, PriceBasisWindow, UserAccount, UserFeeRates,
        },
    },
    stateful_precompiles::convert_db_err,
    PrecompileError,
};

use keys::{
    account_key, admin_key, api_key_ids_key, api_key_key, ask_level_key, ask_prices_key,
    best_ask_key, best_bid_key, bid_level_key, bid_prices_key, commitment_slot, erc20_balance_slot,
    funding_state_key, index_price_history_key, index_price_state_key, insurance_fund_key,
    last_traded_price_key, mark_price_key, market_fee_total_key, market_key, market_manager_key,
    open_interest_key, oracle_key, order_key, position_key, premium_accumulator_key,
    price_basis_window_key, trade_count_key, user_buy_orders_key, user_fee_rates_key,
    user_nonce_key, user_sell_orders_key,
};

// ── Generic msgpack helpers ───────────────────────────────────────────────────

fn encode<T: Serialize>(val: &T) -> Result<Vec<u8>, PrecompileError> {
    let mut buf = Vec::new();
    // Positional (array) msgpack: struct field NAMES are NOT serialized (P4/#20) — smaller blobs
    // and faster decode; the decoder reads fields by position via serde's `visit_seq`. Cross-
    // version blob compatibility is intentionally dropped (the chain is wiped on any encoding
    // change anyway, since blob bytes feed the on-trie commitment), so field ORDER in `types/` is
    // now layout-significant — only append fields, never reorder/insert.
    val.serialize(&mut RMPSerializer::new(&mut buf))
        .map_err(|_| perp_err("msgpack encode error"))?;
    Ok(buf)
}

fn decode<T: for<'de> Deserialize<'de>>(buf: &[u8]) -> Result<T, PrecompileError> {
    let mut de = RMPDeserializer::new(buf);
    Deserialize::deserialize(&mut de).map_err(|_| perp_err("msgpack decode error"))
}

/// Reads an off-trie PerpDEX blob ("PerpState").
///
/// Returns the in-block overlay value if the key was written during this block, otherwise the
/// committed off-trie store. This replaces the previous chunked `sstore`/`sload` blob, which
/// lived in the state trie under `PERP_DEX_ADDRESS`; perp data now rides the journal's perp
/// section instead, so it gets the same revert lifecycle but never enters the state root.
/// See `docs/perpstate-journal集成方案.md` §4.2.
fn load_blob<CTX: ContextTr>(context: &mut CTX, key: B256) -> Result<Vec<u8>, PrecompileError> {
    context
        .journal_mut()
        .perp_load(key)
        .map_err(convert_db_err::<CTX::Db>)
}

/// Writes an off-trie PerpDEX blob. An empty `buf` marks the key absent. The write is journaled
/// in the perp section (reverts in lock-step with the surrounding checkpoint / `discard_tx`) and
/// is never folded into the trie-bound `EvmState`.
fn store_blob<CTX: ContextTr>(
    context: &mut CTX,
    key: B256,
    buf: &[u8],
) -> Result<(), PrecompileError> {
    context.journal_mut().perp_store(key, buf.to_vec());

    // Global commitment over the off-trie perp write-stream, anchored ON-trie under 0x1003 so
    // divergence surfaces in the state root (consensus-detectable). Every write (incl. empty-buf
    // deletes) is appended to a per-call in-memory LOG, framed `key(32) ‖ len(u32 BE) ‖ value`,
    // and the whole log is hashed ONCE at call exit (`flush_commitment`):
    //   C_new = keccak256(C_prev ‖ COMMITMENT_VERSION ‖ log)
    // (P4/16b). This replaces the former per-store chained fold — one hash over a length-framed
    // log instead of N chained keccaks + N sstores. It CHANGES the 0x1003 commitment value vs the
    // chained scheme, so it requires a fresh chain (devnet wipe); the length prefix keeps the log
    // injective over variable-length blobs.
    let mut framed = Vec::with_capacity(32 + 4 + buf.len());
    framed.extend_from_slice(key.as_slice());
    framed.extend_from_slice(&(buf.len() as u32).to_be_bytes());
    framed.extend_from_slice(buf);
    context.journal_mut().perp_fold_append(&framed);
    Ok(())
}

/// Version byte mixed into the per-call commitment hash, so the framed construction can evolve
/// (e.g. a future block-level fold) while staying distinguishable.
/// v1 = 16b execution-order framed log; v2 = #18 coalesced net delta (last-value-per-key, sorted).
const COMMITMENT_VERSION: u8 = 2;

/// Hashes the per-call commitment log into the on-trie anchor slot under 0x1003.
///
/// Called once at the end of every successful `run_perp_dex_call`; a no-op if the call performed no
/// `store_blob` (empty log). Reads the running commitment `C_prev` from the slot, computes
/// `C_new = blake3(C_prev ‖ COMMITMENT_VERSION ‖ Σ_sorted(key ‖ len ‖ value))`, and sstores it. The
/// sstore is journaled, so it reverts with the surrounding frame. `touch_account` is required so
/// the slot change is included in the BundleState transition (a normal perp tx does not otherwise
/// touch 0x1003's on-trie storage — its bulk writes are off-trie); mirrors `save_erc20_balance`.
///
/// The raw per-call log records every write in execution order (possibly with duplicate keys);
/// here it is COALESCED to the net delta — last value per key, emitted in ascending key order
/// (#18). This commits the call's net STATE CHANGE rather than its write *sequence*: two executions
/// reaching the same net delta produce the same commitment (which is the property that matters for
/// state-divergence detection), and a key written N times in a call is hashed once. Coalescing +
/// key-sorting is deterministic across nodes (keys are a total order; HashMap is only an
/// intermediate, never iterated for the hash).
///
/// BLAKE3 (P4/#24) rather than keccak: the slot is an internal consensus anchor (no EVM SHA3
/// opcode, no contract reads it — only the next call's `C_prev` seed), so the hash function is a
/// free choice; BLAKE3 is faster, especially over the longer log. 32-byte digest → U256.
///
/// Tests that drive `store_blob` / `run_*` directly (bypassing the dispatch) must call this to make
/// the commitment observable on the slot.
pub(crate) fn flush_commitment<CTX: ContextTr>(context: &mut CTX) -> Result<(), PrecompileError> {
    let log = context.journal_mut().perp_fold_take_log();
    if log.is_empty() {
        return Ok(());
    }
    context
        .journal_mut()
        .warm_account(PERP_DEX_ADDRESS)
        .map_err(convert_db_err::<CTX::Db>)?;
    let c_prev = context
        .journal_mut()
        .sload(PERP_DEX_ADDRESS, commitment_slot().into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data;

    // Coalesce the framed log (key(32) ‖ len(u32 BE) ‖ value, per write) to the net delta:
    // last value wins per key. The log is internally produced, so the framing is exact.
    let mut net: HashMap<B256, &[u8]> = HashMap::default();
    let mut i = 0usize;
    while i < log.len() {
        let key = B256::from_slice(&log[i..i + 32]);
        i += 32;
        let len = u32::from_be_bytes(log[i..i + 4].try_into().unwrap()) as usize;
        i += 4;
        let value = &log[i..i + len];
        i += len;
        net.insert(key, value);
    }
    // Emit in ascending key order for cross-node determinism (HashMap iteration is not ordered).
    let mut net_keys: Vec<B256> = net.keys().copied().collect();
    net_keys.sort_unstable();

    let mut hasher = blake3::Hasher::new();
    hasher.update(&c_prev.to_be_bytes::<32>());
    hasher.update(&[COMMITMENT_VERSION]);
    for key in &net_keys {
        let value = net[key];
        hasher.update(key.as_slice());
        hasher.update(&(value.len() as u32).to_be_bytes());
        hasher.update(value);
    }
    let c_new = U256::from_be_bytes(*hasher.finalize().as_bytes());
    context
        .journal_mut()
        .sstore(PERP_DEX_ADDRESS, commitment_slot().into(), c_new)
        .map_err(convert_db_err::<CTX::Db>)?;
    context.journal_mut().touch_account(PERP_DEX_ADDRESS);
    Ok(())
}

/// Discards the per-call commitment log without hashing it (revert / fatal path).
///
/// The surrounding frame's `checkpoint_revert` undoes the perp overlay writes, and the anchor slot
/// was never written this call, so it stays at its pre-call value. Clearing the log here also
/// prevents it leaking into the next call in the same transaction.
pub(crate) fn discard_commitment_fold<CTX: ContextTr>(context: &mut CTX) {
    let _ = context.journal_mut().perp_fold_take_log();
}

// ── Admin ─────────────────────────────────────────────────────────────────────

/// Returns `Address::ZERO` when no admin has been initialised yet.
pub fn load_admin<CTX: ContextTr>(context: &mut CTX) -> Result<Address, PrecompileError> {
    let buf = load_blob(context, admin_key())?;
    if buf.is_empty() {
        return Ok(Address::ZERO);
    }
    decode(&buf)
}

pub fn save_admin<CTX: ContextTr>(
    context: &mut CTX,
    admin: Address,
) -> Result<(), PrecompileError> {
    let buf = encode(&admin)?;
    store_blob(context, admin_key(), &buf)
}

// ── UserAccount ───────────────────────────────────────────────────────────────

pub fn load_account<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
) -> Result<UserAccount, PrecompileError> {
    let buf = load_blob(context, account_key(user))?;
    if buf.is_empty() {
        return Ok(UserAccount::default());
    }
    decode(&buf)
}

pub fn save_account<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    account: UserAccount,
) -> Result<(), PrecompileError> {
    let buf = encode(&account)?;
    store_blob(context, account_key(user), &buf)
}

pub fn load_user_fee_rates<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
) -> Result<UserFeeRates, PrecompileError> {
    let buf = load_blob(context, user_fee_rates_key(user))?;
    if buf.is_empty() {
        return Ok(UserFeeRates::default());
    }
    decode(&buf)
}

pub fn save_user_fee_rates<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    rates: UserFeeRates,
) -> Result<(), PrecompileError> {
    let buf = encode(&rates)?;
    store_blob(context, user_fee_rates_key(user), &buf)
}

pub fn load_market_fee_total<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, market_fee_total_key(market_id))?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn add_market_fee_total<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    amount: u64,
) -> Result<(), PrecompileError> {
    if amount == 0 {
        return Ok(());
    }
    let total = load_market_fee_total(context, market_id)?
        .checked_add(amount)
        .ok_or_else(|| perp_err("fee total overflow"))?;
    let buf = encode(&total)?;
    store_blob(context, market_fee_total_key(market_id), &buf)
}

// ── ERC-20 balance helpers ─────────────────────────────────────────────────────

pub fn load_erc20_balance<CTX: ContextTr>(
    context: &mut CTX,
    token: Address,
    account: Address,
) -> Result<U256, PrecompileError> {
    let slot = erc20_balance_slot(account);
    // Ensure the token address is loaded into journal state before sload.
    // sload panics if the account is absent from the journal.
    context
        .journal_mut()
        .warm_account(token)
        .map_err(convert_db_err::<CTX::Db>)?;
    let value = context
        .journal_mut()
        .sload(token, slot.into())
        .map_err(convert_db_err::<CTX::Db>)?
        .data;
    Ok(value)
}

pub fn save_erc20_balance<CTX: ContextTr>(
    context: &mut CTX,
    token: Address,
    account: Address,
    balance: U256,
) -> Result<(), PrecompileError> {
    let slot = erc20_balance_slot(account);
    // Ensure the token address is loaded into journal state before sstore.
    context
        .journal_mut()
        .warm_account(token)
        .map_err(convert_db_err::<CTX::Db>)?;
    context
        .journal_mut()
        .sstore(token, slot.into(), balance)
        .map_err(convert_db_err::<CTX::Db>)?;
    // Mark the token account as touched so its storage changes are included in
    // the BundleState transition.  Without this, apply_account_state() skips
    // untouched accounts and the sstore above is silently dropped from the DB commit.
    context.journal_mut().touch_account(token);
    Ok(())
}

// ── PerpPosition ──────────────────────────────────────────────────────────────

pub fn load_position<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
) -> Result<PerpPosition, PrecompileError> {
    let buf = load_blob(context, position_key(user, market_id))?;
    if buf.is_empty() {
        return Ok(PerpPosition::default());
    }
    decode(&buf)
}

pub fn save_position<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    pos: &PerpPosition,
) -> Result<(), PrecompileError> {
    let buf = encode(pos)?;
    store_blob(context, position_key(user, market_id), &buf)
}

// ── Order entry lists (per-user per-market) ───────────────────────────────────

pub fn load_buy_orders<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
) -> Result<Vec<OrderEntry>, PrecompileError> {
    let buf = load_blob(context, user_buy_orders_key(user, market_id))?;
    if buf.is_empty() {
        return Ok(vec![]);
    }
    decode(&buf)
}

pub fn save_buy_orders<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    entries: &[OrderEntry],
) -> Result<(), PrecompileError> {
    let buf = encode(&entries)?;
    store_blob(context, user_buy_orders_key(user, market_id), &buf)
}

pub fn load_sell_orders<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
) -> Result<Vec<OrderEntry>, PrecompileError> {
    let buf = load_blob(context, user_sell_orders_key(user, market_id))?;
    if buf.is_empty() {
        return Ok(vec![]);
    }
    decode(&buf)
}

pub fn save_sell_orders<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    market_id: u64,
    entries: &[OrderEntry],
) -> Result<(), PrecompileError> {
    let buf = encode(&entries)?;
    store_blob(context, user_sell_orders_key(user, market_id), &buf)
}

// ── Full Order struct ─────────────────────────────────────────────────────────

pub fn load_order<CTX: ContextTr>(
    context: &mut CTX,
    order_id: &[u8; 32],
) -> Result<Option<Order>, PrecompileError> {
    let buf = load_blob(context, order_key(order_id))?;
    if buf.is_empty() {
        return Ok(None);
    }
    Ok(Some(decode(&buf)?))
}

pub fn save_order<CTX: ContextTr>(
    context: &mut CTX,
    order_id: &[u8; 32],
    order: &Order,
) -> Result<(), PrecompileError> {
    let buf = encode(order)?;
    store_blob(context, order_key(order_id), &buf)
}

// ── Global trade counter ──────────────────────────────────────────────────────

/// Atomically increment and return the *current* trade ID for a market, then store the
/// incremented value.  Returns 0 for the first trade in that market, 1 for the second, etc.
/// Trade IDs are per-market so that indexers can use them directly as `fromId` cursors.
pub fn next_trade_id<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, trade_count_key(market_id))?;
    let current: u64 = if buf.is_empty() { 0 } else { decode(&buf)? };
    let next_buf = encode(&(current + 1))?;
    store_blob(context, trade_count_key(market_id), &next_buf)?;
    Ok(current)
}

// ── User nonce ────────────────────────────────────────────────────────────────

pub fn load_user_nonce<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, user_nonce_key(user))?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn save_user_nonce<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    nonce: u64,
) -> Result<(), PrecompileError> {
    let buf = encode(&nonce)?;
    store_blob(context, user_nonce_key(user), &buf)
}

// ── Market ────────────────────────────────────────────────────────────────────

pub fn load_market<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<Option<Market>, PrecompileError> {
    let buf = load_blob(context, market_key(market_id))?;
    if buf.is_empty() {
        return Ok(None);
    }
    Ok(Some(decode(&buf)?))
}

pub fn save_market<CTX: ContextTr>(
    context: &mut CTX,
    market: &Market,
) -> Result<(), PrecompileError> {
    let buf = encode(market)?;
    store_blob(context, market_key(market.market_id), &buf)
}

// ── Mark price ────────────────────────────────────────────────────────────────

pub fn load_mark_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, mark_price_key(market_id))?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn save_mark_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    let buf = encode(&price)?;
    store_blob(context, mark_price_key(market_id), &buf)
}

// ── Open interest ─────────────────────────────────────────────────────────────

pub fn load_open_interest<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, open_interest_key(market_id))?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn save_open_interest<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    oi: u64,
) -> Result<(), PrecompileError> {
    let buf = encode(&oi)?;
    store_blob(context, open_interest_key(market_id), &buf)
}

// ── Order book: price level lists ─────────────────────────────────────────────

/// Sorted bid prices DESC.
pub fn load_bid_prices<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<Vec<u64>, PrecompileError> {
    let buf = load_blob(context, bid_prices_key(market_id))?;
    if buf.is_empty() {
        return Ok(vec![]);
    }
    decode(&buf)
}

pub fn save_bid_prices<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    prices: &[u64],
) -> Result<(), PrecompileError> {
    let buf = encode(&prices)?;
    store_blob(context, bid_prices_key(market_id), &buf)
}

/// Sorted ask prices ASC.
pub fn load_ask_prices<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<Vec<u64>, PrecompileError> {
    let buf = load_blob(context, ask_prices_key(market_id))?;
    if buf.is_empty() {
        return Ok(vec![]);
    }
    decode(&buf)
}

pub fn save_ask_prices<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    prices: &[u64],
) -> Result<(), PrecompileError> {
    let buf = encode(&prices)?;
    store_blob(context, ask_prices_key(market_id), &buf)
}

// ── Order book: FIFO queue at a price level ───────────────────────────────────

pub fn load_bid_level<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<Vec<[u8; 32]>, PrecompileError> {
    let buf = load_blob(context, bid_level_key(market_id, price))?;
    if buf.is_empty() {
        return Ok(vec![]);
    }
    decode(&buf)
}

pub fn save_bid_level<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
    queue: &[[u8; 32]],
) -> Result<(), PrecompileError> {
    let buf = encode(&queue)?;
    store_blob(context, bid_level_key(market_id, price), &buf)
}

pub fn load_ask_level<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<Vec<[u8; 32]>, PrecompileError> {
    let buf = load_blob(context, ask_level_key(market_id, price))?;
    if buf.is_empty() {
        return Ok(vec![]);
    }
    decode(&buf)
}

pub fn save_ask_level<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
    queue: &[[u8; 32]],
) -> Result<(), PrecompileError> {
    let buf = encode(&queue)?;
    store_blob(context, ask_level_key(market_id, price), &buf)
}

// ── Order book helpers ────────────────────────────────────────────────────────

/// Insert `price` into the bid price list (kept sorted DESC) if not already present.
pub fn insert_bid_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    let mut prices = load_bid_prices(context, market_id)?;
    if !prices.contains(&price) {
        let idx = prices.partition_point(|&p| p > price);
        prices.insert(idx, price);
        save_bid_prices(context, market_id, &prices)?;
    }
    Ok(())
}

/// Insert `price` into the ask price list (kept sorted ASC) if not already present.
pub fn insert_ask_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    let mut prices = load_ask_prices(context, market_id)?;
    if !prices.contains(&price) {
        let idx = prices.partition_point(|&p| p < price);
        prices.insert(idx, price);
        save_ask_prices(context, market_id, &prices)?;
    }
    Ok(())
}

/// Remove `price` from the bid price list (call when level becomes empty).
pub fn remove_bid_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    let mut prices = load_bid_prices(context, market_id)?;
    prices.retain(|&p| p != price);
    save_bid_prices(context, market_id, &prices)
}

/// Remove `price` from the ask price list (call when level becomes empty).
pub fn remove_ask_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    let mut prices = load_ask_prices(context, market_id)?;
    prices.retain(|&p| p != price);
    save_ask_prices(context, market_id, &prices)
}

/// Append `order_id` to the FIFO queue at the given bid price level.
pub fn push_bid_order<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
    order_id: [u8; 32],
) -> Result<(), PrecompileError> {
    let mut queue = load_bid_level(context, market_id, price)?;
    queue.push(order_id);
    save_bid_level(context, market_id, price, &queue)
}

/// Append `order_id` to the FIFO queue at the given ask price level.
pub fn push_ask_order<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
    order_id: [u8; 32],
) -> Result<(), PrecompileError> {
    let mut queue = load_ask_level(context, market_id, price)?;
    queue.push(order_id);
    save_ask_level(context, market_id, price, &queue)
}

// ── Best bid / ask cache ──────────────────────────────────────────────────────
// Stored as a single u64 per market.  0 means "no orders on that side".
// Kept in sync with the sorted price lists so callers can avoid loading the
// full list just for a PostOnly check or a quick spread query.

pub fn load_best_bid<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, best_bid_key(market_id))?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn save_best_bid<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    let buf = encode(&price)?;
    store_blob(context, best_bid_key(market_id), &buf)
}

pub fn load_best_ask<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, best_ask_key(market_id))?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn save_best_ask<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    let buf = encode(&price)?;
    store_blob(context, best_ask_key(market_id), &buf)
}

/// Re-derive best_bid from the current bid price list (already in journal cache after matching).
/// Call this after any operation that may have removed the top bid level.
pub fn refresh_best_bid<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let prices = load_bid_prices(context, market_id)?;
    let best = prices.first().copied().unwrap_or(0);
    save_best_bid(context, market_id, best)?;
    Ok(best)
}

/// Re-derive best_ask from the current ask price list (already in journal cache after matching).
/// Call this after any operation that may have removed the top ask level.
pub fn refresh_best_ask<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let prices = load_ask_prices(context, market_id)?;
    let best = prices.first().copied().unwrap_or(0);
    save_best_ask(context, market_id, best)?;
    Ok(best)
}
// ── API key (ed25519 signed orders) ──────────────────────────────────────────

pub fn load_api_key<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    key_id: u8,
) -> Result<Option<ApiKey>, PrecompileError> {
    let buf = load_blob(context, api_key_key(user, key_id))?;
    if buf.is_empty() {
        return Ok(None);
    }
    Ok(Some(decode(&buf)?))
}

pub fn save_api_key<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    key_id: u8,
    key: ApiKey,
) -> Result<(), PrecompileError> {
    let buf = encode(&key)?;
    store_blob(context, api_key_key(user, key_id), &buf)?;
    // Track this key_id in the user's id list (deduplicated).
    let mut ids = load_api_key_ids(context, user)?;
    if !ids.contains(&key_id) {
        ids.push(key_id);
        ids.sort_unstable();
        save_api_key_ids(context, user, &ids)?;
    }
    Ok(())
}

pub fn delete_api_key<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    key_id: u8,
) -> Result<(), PrecompileError> {
    store_blob(context, api_key_key(user, key_id), &[])?;
    let mut ids = load_api_key_ids(context, user)?;
    ids.retain(|&id| id != key_id);
    save_api_key_ids(context, user, &ids)
}

pub fn load_api_key_ids<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
) -> Result<Vec<u8>, PrecompileError> {
    let buf = load_blob(context, api_key_ids_key(user))?;
    if buf.is_empty() {
        return Ok(vec![]);
    }
    decode(&buf)
}

fn save_api_key_ids<CTX: ContextTr>(
    context: &mut CTX,
    user: Address,
    ids: &[u8],
) -> Result<(), PrecompileError> {
    let buf = encode(&ids)?;
    store_blob(context, api_key_ids_key(user), &buf)
}

// ── Role addresses ────────────────────────────────────────────────────────────

pub fn load_oracle<CTX: ContextTr>(context: &mut CTX) -> Result<Address, PrecompileError> {
    let buf = load_blob(context, oracle_key())?;
    if buf.is_empty() {
        return Ok(Address::ZERO);
    }
    decode(&buf)
}

pub fn save_oracle<CTX: ContextTr>(
    context: &mut CTX,
    oracle: Address,
) -> Result<(), PrecompileError> {
    let buf = encode(&oracle)?;
    store_blob(context, oracle_key(), &buf)
}

pub fn load_market_manager<CTX: ContextTr>(context: &mut CTX) -> Result<Address, PrecompileError> {
    let buf = load_blob(context, market_manager_key())?;
    if buf.is_empty() {
        return Ok(Address::ZERO);
    }
    decode(&buf)
}

pub fn save_market_manager<CTX: ContextTr>(
    context: &mut CTX,
    manager: Address,
) -> Result<(), PrecompileError> {
    let buf = encode(&manager)?;
    store_blob(context, market_manager_key(), &buf)
}

// ── Index price state ─────────────────────────────────────────────────────────

pub fn load_index_price_state<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<IndexPriceState, PrecompileError> {
    let buf = load_blob(context, index_price_state_key(market_id))?;
    if buf.is_empty() {
        return Ok(IndexPriceState::default());
    }
    decode(&buf)
}

pub fn save_index_price_state<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    state: &IndexPriceState,
) -> Result<(), PrecompileError> {
    let buf = encode(state)?;
    store_blob(context, index_price_state_key(market_id), &buf)
}

// ── Price mid window (30s MA basis input) ─────────────────────────────────────

pub fn load_index_price_history<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<IndexPriceHistory, PrecompileError> {
    let buf = load_blob(context, index_price_history_key(market_id))?;
    if buf.is_empty() {
        return Ok(IndexPriceHistory::default());
    }
    decode(&buf)
}

pub fn save_index_price_history<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    history: &IndexPriceHistory,
) -> Result<(), PrecompileError> {
    let buf = encode(history)?;
    store_blob(context, index_price_history_key(market_id), &buf)
}

pub fn load_price_basis_window<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<PriceBasisWindow, PrecompileError> {
    let buf = load_blob(context, price_basis_window_key(market_id))?;
    if buf.is_empty() {
        return Ok(PriceBasisWindow::default());
    }
    decode(&buf)
}

pub fn save_price_basis_window<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    window: &PriceBasisWindow,
) -> Result<(), PrecompileError> {
    let buf = encode(window)?;
    store_blob(context, price_basis_window_key(market_id), &buf)
}

// ── Last traded price (contract price) ───────────────────────────────────────

pub fn load_last_traded_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, last_traded_price_key(market_id))?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn save_last_traded_price<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    price: u64,
) -> Result<(), PrecompileError> {
    let buf = encode(&price)?;
    store_blob(context, last_traded_price_key(market_id), &buf)
}

// ── Funding state ─────────────────────────────────────────────────────────────

pub fn load_funding_state<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<FundingState, PrecompileError> {
    let buf = load_blob(context, funding_state_key(market_id))?;
    if buf.is_empty() {
        return Ok(FundingState::default());
    }
    decode(&buf)
}

pub fn save_funding_state<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    state: &FundingState,
) -> Result<(), PrecompileError> {
    let buf = encode(state)?;
    store_blob(context, funding_state_key(market_id), &buf)
}

// ── Insurance Fund ────────────────────────────────────────────────────────────

pub fn load_insurance_fund<CTX: ContextTr>(context: &mut CTX) -> Result<u64, PrecompileError> {
    let buf = load_blob(context, insurance_fund_key())?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn save_insurance_fund<CTX: ContextTr>(
    context: &mut CTX,
    balance: u64,
) -> Result<(), PrecompileError> {
    let buf = encode(&balance)?;
    store_blob(context, insurance_fund_key(), &buf)
}

/// Absorb up to `deficit` from the insurance fund.
/// Returns `(absorbed, remaining_deficit)`.
/// If the fund covers everything, `remaining_deficit` is 0.
/// If the fund is insufficient, it is drained to zero and `remaining_deficit` > 0.
pub fn absorb_from_insurance_fund<CTX: ContextTr>(
    context: &mut CTX,
    deficit: u64,
) -> Result<(u64, u64), PrecompileError> {
    let balance = load_insurance_fund(context)?;
    let absorbed = deficit.min(balance);
    let remaining = deficit - absorbed;
    save_insurance_fund(context, balance - absorbed)?;
    Ok((absorbed, remaining))
}

// ── Premium index accumulator ─────────────────────────────────────────────────

pub fn load_premium_accumulator<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
) -> Result<PremiumIndexAccumulator, PrecompileError> {
    let buf = load_blob(context, premium_accumulator_key(market_id))?;
    if buf.is_empty() {
        return Ok(PremiumIndexAccumulator::default());
    }
    decode(&buf)
}

pub fn save_premium_accumulator<CTX: ContextTr>(
    context: &mut CTX,
    market_id: u64,
    acc: &PremiumIndexAccumulator,
) -> Result<(), PrecompileError> {
    let buf = encode(acc)?;
    store_blob(context, premium_accumulator_key(market_id), &buf)
}

#[cfg(test)]
mod commitment_tests {
    use super::*;
    use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
    use database::InMemoryDB;
    use primitives::hardfork::SpecId;

    type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

    /// Mirror of `perp_dex::trading::tests::make_ctx`: build a journal-backed context and warm the
    /// PERP_DEX account so the on-trie commitment sload/sstore have an account to operate on.
    fn new_test_ctx() -> TestCtx {
        let db = InMemoryDB::default();
        let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
        JournalTr::load_account(ctx.journal_mut(), PERP_DEX_ADDRESS).unwrap();
        ctx
    }

    fn read_commitment<CTX: ContextTr>(ctx: &mut CTX) -> U256 {
        ctx.journal_mut()
            .sload(PERP_DEX_ADDRESS, commitment_slot().into())
            .unwrap()
            .data
    }

    /// Independent reference model of one call's commitment hash (P4/16b + #24 + #18):
    /// `C_new = blake3(C_prev ‖ COMMITMENT_VERSION ‖ Σ_sorted(key(32) ‖ len(u32 BE) ‖ value))`,
    /// the writes COALESCED to the net delta (last value per key) and emitted in ascending key
    /// order. Uses a BTreeMap (sorted, last-insert-wins) — a different implementation than the
    /// production HashMap+sort, so agreement is a genuine cross-check, not a tautology.
    fn expect_framed(prev: U256, writes: &[(B256, &[u8])]) -> U256 {
        let mut net: std::collections::BTreeMap<B256, &[u8]> = std::collections::BTreeMap::new();
        for (key, blob) in writes {
            net.insert(*key, blob);
        }
        let mut p = Vec::new();
        p.extend_from_slice(&prev.to_be_bytes::<32>());
        p.push(COMMITMENT_VERSION);
        for (key, blob) in &net {
            p.extend_from_slice(key.as_slice());
            p.extend_from_slice(&(blob.len() as u32).to_be_bytes());
            p.extend_from_slice(blob);
        }
        U256::from_be_bytes(*blake3::hash(&p).as_bytes())
    }

    #[test]
    fn commitment_framed_log_over_writes() {
        let mut ctx = new_test_ctx();
        assert_eq!(read_commitment(&mut ctx), U256::ZERO); // genesis init = 0

        // Each store+flush is one call hashing a 1-write log against the running C_prev.
        let (k1, b1) = (B256::with_last_byte(1), vec![0xAAu8]);
        let (k2, b2) = (B256::with_last_byte(2), vec![0xBBu8, 0xCC]);
        store_blob(&mut ctx, k1, &b1).unwrap();
        flush_commitment(&mut ctx).unwrap();
        let c1 = expect_framed(U256::ZERO, &[(k1, &b1)]);
        assert_eq!(read_commitment(&mut ctx), c1);
        store_blob(&mut ctx, k2, &b2).unwrap();
        flush_commitment(&mut ctx).unwrap();
        let c2 = expect_framed(c1, &[(k2, &b2)]);
        assert_eq!(read_commitment(&mut ctx), c2);
        assert_ne!(c2, c1);
    }

    /// A whole call's writes are accumulated into one log and hashed ONCE at flush; the slot must
    /// not move until the flush. With #18 the log is coalesced to the net delta — a key written
    /// twice in the call contributes only its FINAL value, and the order is by key, not execution.
    #[test]
    fn commitment_single_call_coalesces_and_hashes_once() {
        let writes: [(B256, Vec<u8>); 3] = [
            (B256::with_last_byte(1), vec![0xAA]),
            (B256::with_last_byte(2), vec![0xBB, 0xCC]),
            (B256::with_last_byte(1), vec![0xDD]), // same key twice → only 0xDD survives in the net
        ];
        let mut ctx = new_test_ctx();
        for (k, b) in &writes {
            store_blob(&mut ctx, *k, b).unwrap();
            assert_eq!(read_commitment(&mut ctx), U256::ZERO, "slot must not move before flush");
        }
        flush_commitment(&mut ctx).unwrap();
        let refs: Vec<(B256, &[u8])> = writes.iter().map(|(k, b)| (*k, b.as_slice())).collect();
        assert_eq!(read_commitment(&mut ctx), expect_framed(U256::ZERO, &refs));

        // Coalescing is real: writing only the FINAL value of the duplicated key (a different
        // execution that reaches the same net delta) yields the SAME commitment.
        let net_only: [(B256, Vec<u8>); 2] = [
            (B256::with_last_byte(2), vec![0xBB, 0xCC]),
            (B256::with_last_byte(1), vec![0xDD]),
        ];
        let mut ctx2 = new_test_ctx();
        for (k, b) in &net_only {
            store_blob(&mut ctx2, *k, b).unwrap();
        }
        flush_commitment(&mut ctx2).unwrap();
        assert_eq!(
            read_commitment(&mut ctx2),
            read_commitment(&mut ctx),
            "same net delta (regardless of write count/order) => same commitment"
        );
    }

    /// `flush_commitment` is a no-op when the call performed no `store_blob` (empty log).
    #[test]
    fn flush_is_noop_without_stores() {
        let mut ctx = new_test_ctx();
        flush_commitment(&mut ctx).unwrap();
        assert_eq!(read_commitment(&mut ctx), U256::ZERO);
    }

    #[test]
    fn commitment_rolls_back_on_revert() {
        let mut ctx = new_test_ctx();
        store_blob(&mut ctx, B256::with_last_byte(1), &[0xAA]).unwrap();
        flush_commitment(&mut ctx).unwrap();
        let before = read_commitment(&mut ctx);

        // The flush sstore is journaled, so a checkpoint_revert after it rolls the slot back. The
        // checkpoint also snapshots the (here empty) commitment-log length and truncates on revert.
        let cp = ctx.journal_mut().checkpoint();
        store_blob(&mut ctx, B256::with_last_byte(2), &[0xBB]).unwrap();
        flush_commitment(&mut ctx).unwrap();
        assert_ne!(read_commitment(&mut ctx), before);
        ctx.journal_mut().checkpoint_revert(cp);
        assert_eq!(read_commitment(&mut ctx), before); // reverted write's commitment update rolled back
    }

    /// Exercises the `JournalCheckpoint` commitment-log truncation with a NON-empty log at the
    /// checkpoint — the path the inner fuzz test (which drives `perp_store` directly, never
    /// appending to the log) never reaches. A checkpoint is taken mid-call after one store, a
    /// second store is made and reverted, then a third store proceeds; the flushed commitment must
    /// equal the framed log over only the surviving writes (w0, w2) in order.
    #[test]
    fn log_truncates_under_mid_call_checkpoint_revert() {
        let (k0, b0) = (B256::with_last_byte(0xA0), vec![0x01u8, 0x02]);
        let (k1, b1) = (B256::with_last_byte(0xA1), vec![0x03u8]); // reverted
        let (k2, b2) = (B256::with_last_byte(0xA2), vec![0x04u8, 0x05, 0x06]);

        let mut ctx = new_test_ctx();
        store_blob(&mut ctx, k0, &b0).unwrap();
        // Checkpoint with a NON-empty log (k0 appended but not yet hashed).
        let cp = ctx.journal_mut().checkpoint();
        store_blob(&mut ctx, k1, &b1).unwrap();
        // Revert: drops the k1 overlay write and truncates the log back to its post-k0 length.
        ctx.journal_mut().checkpoint_revert(cp);
        // The next store appends to the TRUNCATED log, so k1 is absent from the final hash.
        store_blob(&mut ctx, k2, &b2).unwrap();
        flush_commitment(&mut ctx).unwrap();

        let expected = expect_framed(U256::ZERO, &[(k0, &b0), (k2, &b2)]);
        assert_eq!(read_commitment(&mut ctx), expected);
        // The reverted blob must be gone from the overlay, in lock-step with the log truncation.
        assert!(load_blob(&mut ctx, k1).unwrap().is_empty(), "reverted write must leave no overlay residue");
    }
}

#[cfg(test)]
mod size_probe_tests {
    use super::*;
    use crate::perp_dex::types::{
        OrderStatus, OrderType, Side, TimeInForce, PRICE_BASIS_WINDOW_SIZE,
    };

    #[test]
    fn probe_encoded_sizes() {
        // Order — realistic BTC-ish values
        let order = Order {
            owner: [0xAB; 20],
            market_id: 1,
            side: Side::Buy,
            price: 65_432_10,        // 7 digits
            quantity: 150_000_000,   // 1.5 BTC @ 8 decimals
            filled: 50_000_000,
            order_type: OrderType::Limit,
            tif: TimeInForce::Gtc,
            status: OrderStatus::PartiallyFilled,
        };
        let buf = encode(&order).unwrap();
        println!("Order: {} bytes; hex={}", buf.len(), primitives::hex::encode(&buf));

        let entry = OrderEntry {
            order_id: [0xCD; 32],
            price: 65_432_10,
            amount: 150_000_000,
            maker_fee_bps: 2,
        };
        println!("OrderEntry x1 (in vec): {} bytes", encode(&vec![entry]).unwrap().len());
        println!("OrderEntry x5: {} bytes", encode(&vec![entry; 5]).unwrap().len());
        println!("OrderEntry x20: {} bytes", encode(&vec![entry; 20]).unwrap().len());
        println!("OrderEntry single hex={}", primitives::hex::encode(encode(&entry).unwrap()));

        let prices: Vec<u64> = (0..1u64).map(|i| 65_000_00 + i * 10).collect();
        println!("bid_prices x1: {} bytes", encode(&prices).unwrap().len());
        let prices: Vec<u64> = (0..10u64).map(|i| 65_000_00 + i * 10).collect();
        println!("bid_prices x10: {} bytes", encode(&prices).unwrap().len());
        let prices: Vec<u64> = (0..100u64).map(|i| 65_000_00 + i * 10).collect();
        println!("bid_prices x100: {} bytes", encode(&prices).unwrap().len());

        let q: Vec<[u8; 32]> = vec![[0xEF; 32]; 1];
        println!("level queue x1: {} bytes", encode(&q).unwrap().len());
        let q: Vec<[u8; 32]> = vec![[0xEF; 32]; 5];
        println!("level queue x5: {} bytes", encode(&q).unwrap().len());
        println!("level queue single elem hex={}", primitives::hex::encode(encode(&[0xEFu8; 32]).unwrap()));

        let pos = PerpPosition {
            amount: 150_000_000,
            v_quote_balance: -98_148_315,
            margin: 9_814_831,
            margin_reserved: 5_000_000,
            margin_reserved_notional: 50_000_000,
            buy_side_margin_reserved: 5_000_000,
            buy_side_reserved_notional: 50_000_000,
            sell_side_margin_reserved: 1_000_000,
            sell_side_reserved_notional: 10_000_000,
            fee_reserved: 10_000,
            leverage: 10,
            last_funding_index: 123_456_789_012_345i128,
        };
        let buf = encode(&pos).unwrap();
        println!("PerpPosition: {} bytes; hex={}", buf.len(), primitives::hex::encode(&buf));
        println!("PerpPosition default: {} bytes", encode(&PerpPosition::default()).unwrap().len());

        let acct = UserAccount {
            usdc_balance: "123456789000000000000".into(), // 21-digit decimal string
            perp_wallet_balance: 1_234_567_890,
        };
        let buf = encode(&acct).unwrap();
        println!("UserAccount: {} bytes; hex={}", buf.len(), primitives::hex::encode(&buf));

        let market = Market {
            market_id: 1,
            base_decimals: 8,
            price_decimals: 2,
            tick_size: 10,
            step_size: 1000,
            min_quantity: 1000,
            max_quantity: 10_000_000_000,
            max_price: 100_000_000,
            price_update_interval: 1,
            active: true,
            funding_interval: 28_800,
            interest_rate: 100,
            liquidation_fee_rate_bps: 50,
        };
        let buf = encode(&market).unwrap();
        println!("Market: {} bytes", buf.len());

        let fs = FundingState {
            last_funding_rate: 125,
            next_funding_ts: 1_750_000_000,
            cumulative_funding_index: 9_876_543_210_123i128,
        };
        println!("FundingState: {} bytes", encode(&fs).unwrap().len());

        let acc = PremiumIndexAccumulator {
            weighted_sum: 123_456_789_012i128,
            sample_count: 28_000,
            epoch_start_ts: 1_750_000_000,
            last_pi: -1234,
            last_sample_ts: 1_750_000_123,
        };
        println!("PremiumIndexAccumulator: {} bytes", encode(&acc).unwrap().len());

        let mut window = PriceBasisWindow::default();
        for i in 0..PRICE_BASIS_WINDOW_SIZE as u64 {
            window.record_observation(1_750_000_000 + i, 65_000_00 + i);
        }
        println!("PriceBasisWindow full: {} bytes", encode(&window).unwrap().len());
        println!("PriceBasisWindow empty: {} bytes", encode(&PriceBasisWindow::default()).unwrap().len());

        let mut hist = IndexPriceHistory::default();
        for i in 0..32u64 {
            hist.push(
                IndexPriceState { index_price: 65_000_00 + i, timestamp: 1_750_000_000 + i },
                32,
            );
        }
        println!("IndexPriceHistory x32: {} bytes", encode(&hist).unwrap().len());

        println!("UserFeeRates: {} bytes", encode(&UserFeeRates { maker_fee_bps: 2, taker_fee_bps: 5 }).unwrap().len());
        println!("u64 scalar (mark price 6_543_210): {} bytes", encode(&6_543_210u64).unwrap().len());
        println!("u64 scalar small (nonce 7): {} bytes", encode(&7u64).unwrap().len());
        println!("Address: {} bytes", encode(&Address::ZERO).unwrap().len());
        println!(
            "ApiKey: {} bytes",
            encode(&ApiKey { pubkey: [9; 32], expiry: 1_750_000_000 }).unwrap().len()
        );
    }
}
