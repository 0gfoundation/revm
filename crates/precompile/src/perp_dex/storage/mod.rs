//! Storage helpers for the PerpDEX precompile.

pub mod keys;

use context::{ContextTr, JournalTr};
use primitives::{Address, B256, U256};
use rmp_serde::{Deserializer as RMPDeserializer, Serializer as RMPSerializer};
use serde::{Deserialize, Serialize};

use crate::{
    journal::{load_bytes, store_bytes},
    perp_dex::{
        errors::perp_err,
        types::{
            ApiKey, FundingState, IndexPriceHistory, IndexPriceState, Market, Order, OrderEntry,
            PerpPosition, PremiumIndexAccumulator, PriceBasisWindow, UserAccount, UserFeeRates,
        },
        PERP_DEX_ADDRESS,
    },
    stateful_precompiles::convert_db_err,
    PrecompileError,
};

use keys::{
    account_key, admin_key, api_key_ids_key, api_key_key, ask_level_key, ask_prices_key,
    best_ask_key, best_bid_key, bid_level_key, bid_prices_key, erc20_balance_slot,
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

fn load_blob<CTX: ContextTr>(context: &mut CTX, key: B256) -> Result<Vec<u8>, PrecompileError> {
    load_bytes(context, PERP_DEX_ADDRESS, key)
}

fn store_blob<CTX: ContextTr>(
    context: &mut CTX,
    key: B256,
    buf: &[u8],
) -> Result<(), PrecompileError> {
    store_bytes(context, PERP_DEX_ADDRESS, key, buf)
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
