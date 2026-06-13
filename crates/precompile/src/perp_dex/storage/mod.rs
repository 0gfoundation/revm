//! Storage helpers for the PerpDEX precompile.

pub mod keys;

use context::{ContextTr, JournalTr};
use primitives::{Address, B256, U256};
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
    val.serialize(&mut RMPSerializer::new(&mut buf).with_struct_map())
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

    // Global chained commitment over the off-trie perp write-stream, anchored ON-trie under
    // 0x1003 so divergence surfaces in the state root (consensus-detectable). Folds EVERY write
    // (incl. empty-buf deletes) in execution order: C = keccak256(C ‖ key ‖ buf).
    //
    // The fold is accumulated in a per-call in-memory accumulator and sstored ONCE at call exit
    // (`flush_commitment`), instead of sload+sstore on every write. The fold *sequence* is
    // byte-identical to the per-store chain, so the slot's final value (hence the state root) is
    // unchanged — only the number of intermediate (unobservable) sstores drops from N to 1.
    let c_old = match context.journal_mut().perp_fold_get() {
        Some(c) => c,
        None => {
            // First store of this call: seed from the committed on-trie running value. Warm
            // 0x1003 so the sload (and the later flush sstore) have an account to operate on —
            // 0x1003 is the precompile's own call target so it is normally pre-loaded, but warm it
            // explicitly to mirror the erc20 path and stay robust.
            context
                .journal_mut()
                .warm_account(PERP_DEX_ADDRESS)
                .map_err(convert_db_err::<CTX::Db>)?;
            context
                .journal_mut()
                .sload(PERP_DEX_ADDRESS, commitment_slot().into())
                .map_err(convert_db_err::<CTX::Db>)?
                .data
        }
    };
    // Streaming keccak over (C ‖ key ‖ buf) — digest identical to hashing the concatenation,
    // without allocating a 64+len preimage buffer per store.
    let mut hasher = alloy_primitives::Keccak256::new();
    hasher.update(c_old.to_be_bytes::<32>());
    hasher.update(key.as_slice());
    hasher.update(buf);
    let c_new = U256::from_be_bytes(hasher.finalize().0);
    context.journal_mut().perp_fold_set(c_new);
    Ok(())
}

/// Flushes the per-call commitment fold accumulator to the on-trie anchor slot under 0x1003.
///
/// Called once at the end of every successful `run_perp_dex_call`; a no-op if the call performed
/// no `store_blob`. The sstore is journaled, so it reverts with the surrounding frame exactly like
/// the former per-store sstore. The `touch_account` is required so the slot change is included in
/// the BundleState transition (a normal perp tx does not otherwise touch 0x1003's on-trie storage,
/// since its bulk writes are off-trie); mirrors `save_erc20_balance`.
///
/// Tests that drive `store_blob` / `run_*` directly (bypassing the dispatch) must call this to make
/// the folded commitment observable on the slot.
pub(crate) fn flush_commitment<CTX: ContextTr>(context: &mut CTX) -> Result<(), PrecompileError> {
    let Some(c_new) = context.journal_mut().perp_fold_take() else {
        return Ok(());
    };
    context
        .journal_mut()
        .warm_account(PERP_DEX_ADDRESS)
        .map_err(convert_db_err::<CTX::Db>)?;
    context
        .journal_mut()
        .sstore(PERP_DEX_ADDRESS, commitment_slot().into(), c_new)
        .map_err(convert_db_err::<CTX::Db>)?;
    context.journal_mut().touch_account(PERP_DEX_ADDRESS);
    Ok(())
}

/// Discards the per-call commitment fold accumulator without writing it (revert / fatal path).
///
/// The surrounding frame's `checkpoint_revert` undoes the perp overlay writes, and the anchor slot
/// was never written this call, so it stays at its pre-call value — net-identical to reverting the
/// former N per-store sstores. Taking the accumulator here also prevents it leaking into the next
/// call in the same transaction.
pub(crate) fn discard_commitment_fold<CTX: ContextTr>(context: &mut CTX) {
    let _ = context.journal_mut().perp_fold_take();
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
    use primitives::{hardfork::SpecId, keccak256};

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

    fn expect_chain(prev: U256, key: B256, blob: &[u8]) -> U256 {
        let mut p = Vec::new();
        p.extend_from_slice(&prev.to_be_bytes::<32>());
        p.extend_from_slice(key.as_slice());
        p.extend_from_slice(blob);
        U256::from_be_bytes(keccak256(&p).0)
    }

    #[test]
    fn commitment_chains_over_writes() {
        let mut ctx = new_test_ctx();
        assert_eq!(read_commitment(&mut ctx), U256::ZERO); // genesis init = 0

        // P2: the fold is accumulated in-memory and only sstored at `flush_commitment` (call
        // exit). Flush after each store so the slot is observable at per-store granularity.
        let (k1, b1) = (B256::with_last_byte(1), vec![0xAAu8]);
        let (k2, b2) = (B256::with_last_byte(2), vec![0xBBu8, 0xCC]);
        store_blob(&mut ctx, k1, &b1).unwrap();
        flush_commitment(&mut ctx).unwrap();
        let c1 = expect_chain(U256::ZERO, k1, &b1);
        assert_eq!(read_commitment(&mut ctx), c1);
        store_blob(&mut ctx, k2, &b2).unwrap();
        flush_commitment(&mut ctx).unwrap();
        let c2 = expect_chain(c1, k2, &b2);
        assert_eq!(read_commitment(&mut ctx), c2);
        assert_ne!(c2, c1);
    }

    /// The per-call accumulator must fold to the SAME value whether flushed after every store
    /// (N sstores) or once after all stores (1 sstore) — this is the byte-identity that makes P2
    /// SAFE. Also checks the slot only advances on flush, not on store.
    #[test]
    fn commitment_accumulates_per_call_then_flushes_once() {
        let writes: [(B256, Vec<u8>); 3] = [
            (B256::with_last_byte(1), vec![0xAA]),
            (B256::with_last_byte(2), vec![0xBB, 0xCC]),
            (B256::with_last_byte(1), vec![0xDD]), // same key written twice in one call
        ];

        // Reference: flush after each store.
        let mut ref_ctx = new_test_ctx();
        for (k, b) in &writes {
            store_blob(&mut ref_ctx, *k, b).unwrap();
            flush_commitment(&mut ref_ctx).unwrap();
        }
        let expected = read_commitment(&mut ref_ctx);

        // Per-call: all stores, then a single flush. The slot must NOT move until the flush.
        let mut ctx = new_test_ctx();
        for (k, b) in &writes {
            store_blob(&mut ctx, *k, b).unwrap();
            assert_eq!(read_commitment(&mut ctx), U256::ZERO, "slot must not move before flush");
        }
        flush_commitment(&mut ctx).unwrap();
        assert_eq!(read_commitment(&mut ctx), expected, "single flush == per-store flushes");
    }

    /// `flush_commitment` is a no-op when the call performed no `store_blob`.
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

        // The flush sstore is journaled, so a checkpoint_revert after it rolls the slot back —
        // exactly as the former per-store sstore did. The checkpoint also snapshots the (here
        // empty) fold accumulator and restores it on revert.
        let cp = ctx.journal_mut().checkpoint();
        store_blob(&mut ctx, B256::with_last_byte(2), &[0xBB]).unwrap();
        flush_commitment(&mut ctx).unwrap();
        assert_ne!(read_commitment(&mut ctx), before);
        ctx.journal_mut().checkpoint_revert(cp);
        assert_eq!(read_commitment(&mut ctx), before); // reverted write's commitment update rolled back
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

/// Forward-compatibility: blobs written under an older schema (missing a field
/// that was later appended) must still decode, defaulting the absent field —
/// not hard-fail with "msgpack decode error". This pins the `#[serde(default)]`
/// on later-added fields in `types/` so it cannot be silently dropped.
#[cfg(test)]
mod forward_compat_tests {
    use super::*;
    use crate::perp_dex::types::{Market, OrderEntry, PerpPosition};
    use serde::Serialize;

    #[test]
    fn legacy_position_blob_without_fee_reserved_decodes() {
        // PerpPosition as it existed before the "fr" (fee_reserved) field.
        #[derive(Serialize)]
        struct LegacyPerpPosition {
            #[serde(rename = "a")]
            amount: i64,
            #[serde(rename = "v")]
            v_quote_balance: i64,
            #[serde(rename = "m")]
            margin: i64,
            #[serde(rename = "mr")]
            margin_reserved: u64,
            #[serde(rename = "mrn")]
            margin_reserved_notional: u64,
            #[serde(rename = "br")]
            buy_side_margin_reserved: u64,
            #[serde(rename = "brn")]
            buy_side_reserved_notional: u64,
            #[serde(rename = "sr")]
            sell_side_margin_reserved: u64,
            #[serde(rename = "srn")]
            sell_side_reserved_notional: u64,
            // no "fr" / fee_reserved
            #[serde(rename = "lv")]
            leverage: u64,
        }

        let buf = encode(&LegacyPerpPosition {
            amount: 5,
            v_quote_balance: -100,
            margin: 20,
            margin_reserved: 1,
            margin_reserved_notional: 2,
            buy_side_margin_reserved: 3,
            buy_side_reserved_notional: 4,
            sell_side_margin_reserved: 5,
            sell_side_reserved_notional: 6,
            leverage: 7,
        })
        .unwrap();

        let pos: PerpPosition = decode(&buf).unwrap();
        assert_eq!(pos.amount, 5);
        assert_eq!(pos.leverage, 7);
        assert_eq!(pos.fee_reserved, 0, "missing fee_reserved must default to 0");
    }

    #[test]
    fn legacy_order_entry_without_maker_fee_bps_decodes() {
        // OrderEntry as it existed before the "MFB" (maker_fee_bps) field.
        #[derive(Serialize)]
        struct LegacyOrderEntry {
            order_id: [u8; 32],
            price: u64,
            amount: u64,
            // no "MFB" / maker_fee_bps
        }

        let buf = encode(&LegacyOrderEntry {
            order_id: [1u8; 32],
            price: 100,
            amount: 9,
        })
        .unwrap();

        let entry: OrderEntry = decode(&buf).unwrap();
        assert_eq!(entry.price, 100);
        assert_eq!(entry.amount, 9);
        assert_eq!(
            entry.maker_fee_bps, 0,
            "missing maker_fee_bps must default to 0"
        );
    }

    #[test]
    fn legacy_market_blob_without_added_fields_decodes() {
        // Market with only its original (no-default) fields; everything added
        // later (price_decimals, max_*, price_update_interval, funding, …) absent.
        #[derive(Serialize)]
        struct LegacyMarket {
            market_id: u64,
            base_decimals: u32,
            tick_size: u64,
            step_size: u64,
            min_quantity: u64,
            active: bool,
        }

        let buf = encode(&LegacyMarket {
            market_id: 1,
            base_decimals: 8,
            tick_size: 1,
            step_size: 1,
            min_quantity: 1,
            active: true,
        })
        .unwrap();

        let market: Market = decode(&buf).unwrap();
        assert_eq!(market.market_id, 1);
        assert_eq!(market.base_decimals, 8);
        assert_eq!(market.price_decimals, 0);
        assert_eq!(market.max_price, 0);
        assert_eq!(market.price_update_interval, 0);
    }
}
