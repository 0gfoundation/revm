//! HL-workload bench fixtures for the perp_dex bottleneck sims (feature = "bench-util").
//!
//! Exposes, through the crate's PUBLIC api, the warm-DB perp entry points (`run_place_order` /
//! `run_cancel_order` / `match_order`-via-market-order) plus the storage readers a criterion bench
//! — a separate compilation unit that cannot see `#[cfg(test)]` items — needs to build the real
//! Hyperliquid order-flow shapes and measure the engine against them.
//!
//! The shapes reproduced here are the ones measured in
//! `perpdex-perf/docs/hl-workload-distribution-20260726.md` (reproducible via
//! `scripts/hl_book_dist.js`): an extreme per-account power law (p50 = 1 order, four MMs holding
//! 1 000–3 655), a WIDE book (20 878 distinct price levels, occupancy p50 = 1), and a few
//! pathologically deep levels (one bid rail of 7 037 orders). No `mod 500` wallet collapse.
#![allow(missing_docs)]

use alloy_sol_types::SolCall;
use context::{BlockEnv, CfgEnv, Context, ContextTr, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::perp_dex::{
    interface::IPerpDex::{
        batchCancelOrdersCall, batchPlaceOrdersCall, cancelOrderCall, placeOrderCall, PlaceItem,
    },
    storage,
    trading::{
        run_batch_cancel_orders, run_batch_place_orders, run_cancel_order, run_place_order,
    },
    types::{AccountUpdateReason, MarginTiers, Market, OrderEntry},
    PERP_DEX_ADDRESS, USDC_ADDRESS,
};

/// The full in-memory CTX the precompile runs against (no node / consensus / disk), identical in
/// shape to `perp_dex/trading/tests.rs::make_ctx()`.
pub type BenchCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

pub const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
pub const MARKET_ID: u64 = 1;

// Reuse the tests.rs market scale (base_decimals 8 / price_decimals 9 / tick 1e9): proven to run
// the full margin/reservation arithmetic without overflow. The absolute price scale is irrelevant
// to every sim here — what matters is the *count* of levels and the *length* of each account's
// order list, both of which we set independently of the scale.
pub const TICK: u64 = 1_000_000_000;
pub const QTY: u64 = 1_000_000;
/// Base price for the sim books (`= 100 * TICK`), well clear of 0 and of `max_price`.
pub const BASE: u64 = 100 * TICK;
/// Effectively-infinite collateral (§1e / §6: nothing is ever rejected for funds, so the full
/// reservation arithmetic runs at cost). Large enough for a 3 655-order MM, small enough that no
/// intermediate `u64`/`u128` product overflows.
pub const WALLET: u64 = 1_000_000_000_000_000;

// Order ABI encodings (interface.rs): side 0=Buy 1=Sell · orderType 0=Limit 1=Market ·
// tif 0=Gtc 1=Ioc 2=Fok 3=PostOnly.
pub const BUY: u8 = 0;
pub const SELL: u8 = 1;
pub const LIMIT: u8 = 0;
pub const MARKET: u8 = 1;
pub const IOC: u8 = 1;
pub const POST_ONLY: u8 = 3;

fn make_ctx() -> BenchCtx {
    let db = InMemoryDB::default();
    let mut ctx: BenchCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

fn the_market() -> Market {
    Market {
        market_id: MARKET_ID,
        base_decimals: 8,
        price_decimals: 9,
        tick_size: TICK,
        step_size: QTY,
        min_quantity: QTY,
        max_quantity: QTY * 1_000,
        // Room for tens of thousands of distinct levels above BASE (Sim B sweeps to 50k).
        max_price: BASE * 100_000,
        price_update_interval: 15,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 0,
        // Wide band so Sim C's marketable fills are never rejected by the fill-time off-mark guard.
        //
        // ⚠️ There IS a placement-time band now, and it is not fully disabled by this value. A quote
        // that BECOMES the best must be inside the band when it is too GOOD to be true, and
        // `mark_band_bounds` maps `bps >= 10_000` to `lower = 0` but only widens the UPPER edge to
        // `mark * (10_000 + bps)/10_000` = `101 * BASE` here. Asks are therefore unconstrained
        // (nothing is below 0), but a new best BID above `101 * BASE` is REJECTED — and `place`
        // panics on reject.
        //
        // Every builder below lays bids on an ascending grid from `BASE`, so each one is a new best
        // bid and the ceiling is a hard cap on scenario width: `BASE + i*TICK` grids break past
        // `i = 10_000`, and the `BASE + 2*i*TICK` grids (`scenario_wide`, `build_sparse_book`) past
        // `i = 5_000`. The module note about a 20 878-level book is ABOVE that cap. Raise
        // `price_band_bps` (it scales the upper edge linearly) or drop `mark_price` to 0 (which
        // returns `(u128::MAX, 0)` = no band at all, the choice the second market below makes) if a
        // wider book is needed.
        price_band_bps: 1_000_000,
        // Mark anchored at BASE so Sim C's taker (which fills near BASE) is well inside the band.
        mark_price: BASE,
        tiers: MarginTiers::default(),
    }
}

/// Warm ctx with the market registered + admin set. No accounts funded yet — the sims fund the
/// exact set of accounts their shape needs.
pub fn hl_ctx() -> BenchCtx {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(&mut ctx, &the_market()).unwrap();
    ctx
}

/// Deterministic distinct account address from an index (leading `0x20` byte avoids the
/// ADMIN/PERP/USDC reserved addresses).
pub fn user_addr(i: u64) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0x20;
    b[12..20].copy_from_slice(&i.to_be_bytes());
    Address::from(b)
}

/// Directly credit a user's perp wallet (bypasses the deposit/transfer flow) and register the
/// account with the journal.
pub fn fund(ctx: &mut BenchCtx, user: Address, amount: u64) {
    JournalTr::load_account(ctx.journal_mut(), user).unwrap();
    let mut acc = storage::load_account(ctx, user).unwrap();
    acc.credit_perp(amount).unwrap();
    storage::save_account(ctx, user, acc, AccountUpdateReason::Adjustment).unwrap();
}

fn place_input(side: u8, price: u64, qty: u64, order_type: u8, tif: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode()
}

fn cancel_input(order_id: [u8; 32]) -> Vec<u8> {
    cancelOrderCall {
        orderId: FixedBytes(order_id),
        marketId: MARKET_ID,
    }
    .abi_encode()
}

/// Place an order for `user` and return its 32-byte id. Panics on reject (the sims are built so
/// nothing legitimately rejects — a panic means the shape setup is wrong).
pub fn place(ctx: &mut BenchCtx, user: Address, side: u8, price: u64, qty: u64, ot: u8, tif: u8) -> [u8; 32] {
    let ret = run_place_order(&place_input(side, price, qty, ot, tif), user, ctx).unwrap();
    ret[..32].try_into().unwrap()
}

pub fn cancel(ctx: &mut BenchCtx, user: Address, order_id: [u8; 32]) {
    run_cancel_order(&cancel_input(order_id), user, ctx).unwrap();
}

/// A pre-encoded `placeOrder` calldata blob, so the timed loop pays only the engine cost (not ABI
/// encoding). Used by the hot place+cancel churn benches.
pub fn place_input_pub(side: u8, price: u64, qty: u64, ot: u8, tif: u8) -> Vec<u8> {
    place_input(side, price, qty, ot, tif)
}
pub fn cancel_input_pub(order_id: [u8; 32]) -> Vec<u8> {
    cancel_input(order_id)
}
pub fn run_place(ctx: &mut BenchCtx, user: Address, input: &[u8]) -> [u8; 32] {
    run_place_order(input, user, ctx).unwrap()[..32].try_into().unwrap()
}
pub fn run_cancel(ctx: &mut BenchCtx, user: Address, input: &[u8]) {
    run_cancel_order(input, user, ctx).unwrap();
}

// ── Readers (for attribution / assertions in the benches) ───────────────────────────────────

/// Length of `user`'s resting BUY order list — the `n` that Sim A sweeps (the O(account) axis).
pub fn buy_list_len(ctx: &mut BenchCtx, user: Address) -> usize {
    storage::load_buy_orders_ref(ctx, user, MARKET_ID).unwrap().len()
}

/// Number of distinct active BID price levels — the `L` that Sim B sweeps.
pub fn bid_prices_len(ctx: &mut BenchCtx) -> usize {
    storage::load_bid_prices_ref(ctx, MARKET_ID).unwrap().len()
}

/// Live-order count at an ask level (Sim C: distinguishes live from stale-but-queued ids).
pub fn ask_count(ctx: &mut BenchCtx, price: u64) -> u64 {
    storage::load_ask_count(ctx, MARKET_ID, price).unwrap()
}
/// Total ids (live + stale) queued at an ask level (Sim C: the FIFO length the walk traverses).
pub fn ask_queue_len(ctx: &mut BenchCtx, price: u64) -> usize {
    storage::load_ask_level_arc(ctx, MARKET_ID, price).unwrap().ids.len()
}

// ── Codec passthrough (Sim B cold path) ─────────────────────────────────────────────────────
// The price index is committed as an rmp-serde `Vec<u64>` blob; a block's first touch of the
// index pays a `decode` of the whole thing (§2B, ~167 KB at L = 20 878). These expose the REAL
// codec so the bench can measure that materialization in isolation, scaled by L.
pub fn encode_prices(prices: &[u64]) -> Vec<u8> {
    storage::encode(&prices.to_vec()).unwrap()
}
pub fn decode_prices(buf: &[u8]) -> Vec<u64> {
    storage::decode::<Vec<u64>>(buf).unwrap()
}

// ── D3 structure A/B (Sim E/F): Vec vs VecDeque for the per-user order list ─────────────────
// The per-user order list is serialized as an rmp-serde SEQ; `Vec<T>` and `VecDeque<T>` in the
// SAME logical order encode to byte-identical msgpack, so a Vec→VecDeque swap is golden-neutral —
// what we must verify is that VecDeque's encode/decode is not slower (the §5 "serialize
// dominates" concern), and that its front insert/remove is O(1) (the D3 win).
pub fn make_order_entries(n: u64) -> Vec<OrderEntry> {
    (0..n)
        .map(|i| OrderEntry {
            order_id: [0u8; 32],
            price: BASE + i * TICK,
            amount: QTY,
            maker_fee_bps: 0,
            assuming_price: BASE + i * TICK,
        })
        .collect()
}
pub fn encode_orders(list: &[OrderEntry]) -> Vec<u8> {
    storage::encode(&list.to_vec()).unwrap()
}
pub fn encode_orders_deque(list: &std::collections::VecDeque<OrderEntry>) -> Vec<u8> {
    storage::encode(list).unwrap()
}
pub fn decode_orders_vec(buf: &[u8]) -> Vec<OrderEntry> {
    storage::decode::<Vec<OrderEntry>>(buf).unwrap()
}
pub fn decode_orders_deque(buf: &[u8]) -> std::collections::VecDeque<OrderEntry> {
    storage::decode::<std::collections::VecDeque<OrderEntry>>(buf).unwrap()
}

// ── Shape builders ──────────────────────────────────────────────────────────────────────────

/// **Sim A actor.** Give `actor` a resting BUY list of length `n`, spread over `n` distinct
/// descending price levels (occupancy 1, matching real p50 = 1). No asks exist, so every order
/// rests (never crosses). Returns the price to churn at: one tick ABOVE the whole list, so the
/// churn place/cancel inserts/removes at the END of the bid price index (O(1) memmove) and touches
/// a fresh single-order level — isolating the O(account-list) reservation fold from the price
/// index (Sim B's concern). The actor's list length is the ONLY thing that varies with `n`.
pub fn build_actor_buys(ctx: &mut BenchCtx, actor: Address, n: u64) -> u64 {
    fund(ctx, actor, WALLET);
    for i in 0..n {
        // Ascending distinct prices: BASE + i*TICK. All below the churn price.
        place(ctx, actor, BUY, BASE + i * TICK, QTY, LIMIT, POST_ONLY);
    }
    BASE + n * TICK // churn price = new best bid, insert-at-end
}

/// **Sim B book.** Build `levels` distinct BID price levels, ONE order each, each from a DISTINCT
/// account (so no account carries a long list — the O(account) reservation stays O(1) and does not
/// confound the price-index cost). Prices are on an EVEN tick grid (`BASE + 2*i*TICK`) so an ODD
/// churn price lands strictly BETWEEN two existing levels → its insert/remove memmoves ~L/2
/// elements of the sorted index (the cost Sim B measures). Returns `(churn_user, churn_price)`.
pub fn build_sparse_book(ctx: &mut BenchCtx, levels: u64) -> (Address, u64) {
    for i in 0..levels {
        let u = user_addr(i);
        fund(ctx, u, WALLET);
        place(ctx, u, BUY, BASE + 2 * i * TICK, QTY, LIMIT, POST_ONLY);
    }
    let churn_user = user_addr(levels + 1);
    fund(ctx, churn_user, WALLET);
    // Odd offset in the middle of the range → not present, forces a mid-vector insert (memmove).
    let churn_price = BASE + (levels /* even index */) * TICK + TICK; // BASE + levels*TICK + TICK, odd
    (churn_user, churn_price)
}

/// **Sim C level.** Build ONE ask level at `price` whose FIFO holds `stale` cancelled-but-queued
/// tombstone ids FIRST, then `live` fillable makers — all via the REAL place/cancel path.
///
/// Build ORDER matters: we place the `stale` sells, then the `live` sells, THEN cancel the stale
/// ones. That keeps the level's live `count` at `live > 0` throughout the cancellations, so it
/// never hits 0 — the lazy-queue only clears the FIFO when `count` reaches 0, so this is exactly
/// how stale ids ACCUMULATE in front of live orders in reality (a persistently-quoted deep level
/// that is heavily churned). The taker then walks the FIFO front-to-back: it must SKIP all `stale`
/// tombstones (`load_order` → None → continue) before it reaches the `live` makers.
/// No bids exist, so the sells rest as makers. Setup is not timed (criterion `iter_batched`).
pub fn build_stale_ask_level(ctx: &mut BenchCtx, price: u64, stale: u64, live: u64) {
    // 1) Place the soon-to-be-stale sells (front of FIFO), keeping their ids + owners to cancel.
    let mut stale_ids: Vec<(Address, [u8; 32])> = Vec::with_capacity(stale as usize);
    for i in 0..stale {
        let u = user_addr(1_000_000 + i);
        fund(ctx, u, WALLET);
        let oid = place(ctx, u, SELL, price, QTY, LIMIT, POST_ONLY);
        stale_ids.push((u, oid));
    }
    // 2) Place the live makers AFTER them (so cancelling the stale ones can't drop count to 0).
    for i in 0..live {
        let u = user_addr(2_000_000 + i);
        fund(ctx, u, WALLET);
        place(ctx, u, SELL, price, QTY, LIMIT, POST_ONLY);
    }
    // 3) Cancel the stale sells: lazy O(1) — count-- (never below `live`), order deleted, id
    //    LINGERS in the FIFO as a tombstone the match walk will skip.
    for (u, oid) in stale_ids {
        cancel(ctx, u, oid);
    }
}

/// A funded taker for Sim C (buys through the ask level). Separate id space.
pub fn sim_c_taker(ctx: &mut BenchCtx) -> Address {
    let t = user_addr(3_000_000);
    fund(ctx, t, WALLET);
    t
}

// ── Batch drivers + scenario books (Sim G/H: real HL batch workload) ────────────────────────
// One HL batch = one atomic action = N same-type ops from one user (mean 6.94, 53% singletons,
// caps place 64 / cancel 256). These drive the REAL batch entry points (ABI decode + drive_batch
// shell + per-item place/cancel) so a batch's cost decomposes into the parts Sims A/B/E/F isolate.

/// `batchPlaceOrders(PlaceItem[])` calldata for `n` PostOnly buys at ascending fresh prices from
/// `base` (all rest, none cross — no asks). Pre-encoded so the timed loop pays only engine cost.
pub fn batch_place_input(base: u64, n: u64) -> Vec<u8> {
    let orders: Vec<PlaceItem> = (0..n)
        .map(|i| PlaceItem {
            marketId: MARKET_ID,
            side: BUY,
            price: base + i * TICK,
            quantity: QTY,
            orderType: LIMIT,
            tif: POST_ONLY,
            clientOrderId: FixedBytes::default(),
        })
        .collect();
    batchPlaceOrdersCall { orders }.abi_encode()
}

/// `batchCancelOrders(bytes32[])` calldata for the given ids.
pub fn batch_cancel_input(order_ids: &[[u8; 32]]) -> Vec<u8> {
    batchCancelOrdersCall {
        orderIds: order_ids.iter().map(|id| FixedBytes(*id)).collect(),
    }
    .abi_encode()
}

pub fn run_batch_place(ctx: &mut BenchCtx, user: Address, input: &[u8]) {
    run_batch_place_orders(input, user, ctx).unwrap();
}
/// Batch cancel; returns Ok even when most ids miss (abort-forward per-item reject). Ignores the
/// status blob — the point is the per-item cost (81.6% of real cancels miss = cheap not-found).
pub fn run_batch_cancel(ctx: &mut BenchCtx, user: Address, input: &[u8]) {
    let _ = run_batch_cancel_orders(input, user, ctx);
}

/// A funded actor at a fixed distinct id (for the batch scenarios).
pub fn batch_actor() -> Address {
    user_addr(7_000_000)
}

/// **Scenario: retail.** Empty book, one funded actor. Baseline per-item cost.
pub fn scenario_retail() -> BenchCtx {
    let mut ctx = hl_ctx();
    fund(&mut ctx, batch_actor(), WALLET);
    ctx
}

/// **Scenario: average.** A realistic mixed book — `accts` one-order accounts spread over `accts`
/// levels + the funded actor. (Keeps the market index at a real-ish moderate width.)
pub fn scenario_average(accts: u64) -> BenchCtx {
    let mut ctx = hl_ctx();
    for i in 0..accts {
        let u = user_addr(i);
        fund(&mut ctx, u, WALLET);
        // Spread over distinct levels well below the actor's band (no cross).
        place(&mut ctx, u, BUY, BASE + 2 * i * TICK, QTY, LIMIT, POST_ONLY);
    }
    fund(&mut ctx, batch_actor(), WALLET);
    ctx
}

/// **Scenario: wide book.** `levels` distinct price levels (Sim B width), one order each from
/// distinct accounts, + the funded actor.
pub fn scenario_wide(levels: u64) -> BenchCtx {
    let mut ctx = hl_ctx();
    for i in 0..levels {
        let u = user_addr(i);
        fund(&mut ctx, u, WALLET);
        place(&mut ctx, u, BUY, BASE + 2 * i * TICK, QTY, LIMIT, POST_ONLY);
    }
    fund(&mut ctx, batch_actor(), WALLET);
    ctx
}

/// **Scenario: big MM.** The actor already holds `n` resting buys (post-#A this is O(1) per place
/// — this scenario confirms the reservation fold is no longer the bottleneck).
pub fn scenario_big_mm(n: u64) -> BenchCtx {
    let mut ctx = hl_ctx();
    build_actor_buys(&mut ctx, batch_actor(), n);
    ctx
}

/// The price ABOVE the actor's own resting band to place a fresh batch at (new best bids, so the
/// index insert is at the end / a fresh level — isolates the per-item place cost from mid-index
/// memmove; matches the touch-churn hot pattern).
pub fn batch_actor_churn_base(n_existing: u64) -> u64 {
    BASE + (n_existing + 1) * TICK
}

/// Seed `n` real placed orders for the actor and return their ids (to build a hit-cancel batch).
pub fn seed_actor_orders(ctx: &mut BenchCtx, base: u64, n: u64) -> Vec<[u8; 32]> {
    (0..n)
        .map(|i| place(ctx, batch_actor(), BUY, base + i * TICK, QTY, LIMIT, POST_ONLY))
        .collect()
}

/// `n` never-placed ids for a miss-cancel batch (81.6% of real cancels miss).
pub fn miss_ids(n: u64) -> Vec<[u8; 32]> {
    (0..n)
        .map(|i| {
            let mut id = [0x55u8; 32];
            id[24..32].copy_from_slice(&i.to_be_bytes());
            id
        })
        .collect()
}

// ── FAITHFUL book0 + op-stream replay (validate the sim reproduces on-chain exec-time) ──────────
// Seeds the REAL book0 and replays the REAL HL op stream through the real precompile entry points,
// timing + classifying each op, to check the sim reproduces the on-chain per-op-type exec-time
// distribution (place_match / place_rest / cancel_hit / place_reject / cancel_miss). Mapping mirrors
// scripts/hl_replay_map.js at the FAITHFUL config: marketId, priceDecimals=2, baseDecimals=5,
// tick=10, step=1, minQty=1, maxQty=34_500_000, band disabled; book0 shifted -27450 (raw).
use std::collections::HashMap;
use std::time::Instant;

pub const HL_PD: u32 = 2;
pub const HL_BD: u32 = 5;
pub const HL_TICK: u64 = 10;
pub const HL_MAXQTY: u64 = 34_500_000;
pub const HL_BOOK0_SHIFT: i64 = -27450;
pub const FAITHFUL_WALLET: u64 = 1_000_000_000_000_000_000; // 1e18, plenty for bd5 notionals

fn faithful_market() -> Market {
    Market {
        market_id: MARKET_ID,
        base_decimals: HL_BD,
        price_decimals: HL_PD,
        tick_size: HL_TICK,
        step_size: 1,
        min_quantity: 1,
        max_quantity: HL_MAXQTY,
        max_price: 100_000_000, // $1,000,000 at pd2 → covers the $200k extent
        price_update_interval: 15,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 0,
        price_band_bps: 1_000_000, // disabled
        mark_price: 0,
        tiers: MarginTiers::default(),
    }
}
pub fn hl_ctx_faithful() -> BenchCtx {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(&mut ctx, &faithful_market()).unwrap();
    ctx
}

fn to_raw_price(px_usd: f64) -> i64 {
    let raw = (px_usd * 100.0).round() as i64; // ×10^pd
    (raw / 10) * 10 // snap down to tick 10
}
fn to_raw_qty(sz: f64) -> u64 {
    let raw = (sz * 100_000.0).round() as i64; // ×10^bd (bd5)
    if raw < 1 {
        0
    } else if raw as u64 > HL_MAXQTY {
        HL_MAXQTY
    } else {
        raw as u64
    }
}
fn map_tif(flags: u64) -> (u8, u8) {
    if flags & 1 != 0 {
        (LIMIT, POST_ONLY)
    } else if flags & (4 | 16) != 0 {
        (MARKET, IOC)
    } else {
        (LIMIT, 0 /*GTC*/)
    }
}

/// try_place: returns the order id, or None if the place reverted (reject).
fn try_place(ctx: &mut BenchCtx, user: Address, side: u8, price: u64, qty: u64, ot: u8, tif: u8) -> Option<[u8; 32]> {
    run_place_order(&place_input(side, price, qty, ot, tif), user, ctx)
        .ok()
        .map(|r| r[..32].try_into().unwrap())
}
fn try_cancel(ctx: &mut BenchCtx, user: Address, oid: [u8; 32]) -> bool {
    run_cancel_order(&cancel_input(oid), user, ctx).is_ok()
}
/// After an accepted place: true if it MATCHED (order gone = fully filled, or filled>0), false if it
/// purely rested (Open, filled 0).
fn order_matched(ctx: &mut BenchCtx, oid: &[u8; 32]) -> bool {
    match storage::load_order_ref(ctx, oid).unwrap() {
        None => true,                       // terminal / deleted → fully matched (or IOC expired)
        Some(o) => o.filled > 0,            // partial fill then rested
    }
}

/// Time a place either directly (engine fn) or through the full precompile envelope
/// (`run_perp_dex_call`: selector dispatch + gas + calldata + revert-encoding). Returns
/// (ns, Some(order_id) if accepted / None if rejected).
fn timed_place(ctx: &mut BenchCtx, u: Address, input: &[u8], via_envelope: bool) -> (u64, Option<[u8; 32]>) {
    if via_envelope {
        let t0 = Instant::now();
        let res = crate::perp_dex::run_perp_dex_call(input, u64::MAX, u, U256::ZERO, false, ctx);
        let dt = t0.elapsed().as_nanos() as u64;
        let oid = match res {
            Ok(o) if !o.reverted => o.bytes.get(..32).and_then(|s| s.try_into().ok()),
            _ => None,
        };
        (dt, oid)
    } else {
        let t0 = Instant::now();
        let res = run_place_order(input, u, ctx);
        let dt = t0.elapsed().as_nanos() as u64;
        (dt, res.ok().map(|r| r[..32].try_into().unwrap()))
    }
}
fn timed_cancel(ctx: &mut BenchCtx, u: Address, input: &[u8], via_envelope: bool) -> (u64, bool) {
    if via_envelope {
        let t0 = Instant::now();
        let res = crate::perp_dex::run_perp_dex_call(input, u64::MAX, u, U256::ZERO, false, ctx);
        let dt = t0.elapsed().as_nanos() as u64;
        (dt, matches!(res, Ok(ref o) if !o.reverted))
    } else {
        let t0 = Instant::now();
        let ok = run_cancel_order(input, u, ctx).is_ok();
        let dt = t0.elapsed().as_nanos() as u64;
        (dt, ok)
    }
}

fn pct(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() { return 0; }
    let i = ((sorted.len() as f64 * q) as usize).min(sorted.len() - 1);
    sorted[i]
}

/// Replay book0 + the first `max_ops` window ops; return a per-op-type exec-time report.
pub fn faithful_replay(book0_path: &str, ops_path: &str, max_ops: usize, via_envelope: bool) -> String {
    let mut ctx = hl_ctx_faithful();
    let mut funded: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let user = user_addr;
    macro_rules! ensure {
        ($ctx:expr, $u:expr) => {
            if funded.insert($u) {
                fund($ctx, user($u), FAITHFUL_WALLET);
            }
        };
    }

    // ---- seed book0 ----
    let mut by_oid: HashMap<String, [u8; 32]> = HashMap::new();
    let b = std::fs::read_to_string(book0_path).expect("read book0");
    let (mut b_ok, mut b_skip) = (0u64, 0u64);
    for line in b.lines() {
        if line.is_empty() { continue; }
        let c: Vec<&str> = line.split('\t').collect();
        if c.len() < 7 { continue; }
        // assetId isBuy px sz userIdx oid placeTs
        let is_buy: u64 = c[1].parse().unwrap_or(0);
        let px: f64 = c[2].parse().unwrap_or(0.0);
        let sz: f64 = c[3].parse().unwrap_or(0.0);
        let uidx: u64 = c[4].parse().unwrap_or(u64::MAX);
        let hl_oid = c[5].to_string();
        let qty = to_raw_qty(sz);
        let praw = to_raw_price(px) + HL_BOOK0_SHIFT;
        if qty == 0 || praw <= 0 || uidx == u64::MAX { b_skip += 1; continue; }
        ensure!(&mut ctx, uidx);
        let side = if is_buy == 1 { BUY } else { SELL };
        match try_place(&mut ctx, user(uidx), side, praw as u64, qty, LIMIT, POST_ONLY) {
            Some(oid) => { by_oid.insert(hl_oid, oid); b_ok += 1; }
            None => { b_skip += 1; }
        }
    }

    // ---- stage profiler (gated: PERP_PROF=1, DIRECT pass only) ----
    let prof_on = std::env::var("PERP_PROF").is_ok() && !via_envelope;
    crate::perp_dex::prof::set_on(prof_on);
    crate::perp_dex::prof::set_tree(std::env::var("PERP_NOTREE").is_err());
    crate::perp_dex::prof::set_merge(std::env::var("PERP_NOMERGE").is_err());
    let mut cancel_prof: Vec<[u64; crate::perp_dex::prof::N]> = Vec::new();
    let mut rest_prof: Vec<[u64; crate::perp_dex::prof::N]> = Vec::new();
    let mut match_prof: Vec<[u64; crate::perp_dex::prof::N]> = Vec::new();

    // ---- replay window ops ----
    let mut by_cloid: HashMap<String, [u8; 32]> = HashMap::new();
    let mut t: HashMap<&'static str, Vec<u64>> = HashMap::new();
    for k in ["place_match", "place_rest", "place_reject", "cancel_hit", "cancel_miss"] {
        t.insert(k, Vec::new());
    }
    let o = std::fs::read_to_string(ops_path).expect("read ops");
    let mut n = 0usize;
    for line in o.lines() {
        if line.is_empty() { continue; }
        if n >= max_ops { break; }
        n += 1;
        let c: Vec<&str> = line.split('\t').collect();
        if c.len() < 10 { continue; }
        // ts opType assetId isBuy px sz userIdx oid cloid flags
        let op = c[1];
        let is_buy: u64 = c[3].parse().unwrap_or(0);
        let uidx: u64 = c[6].parse().unwrap_or(u64::MAX);
        let hl_oid = c[7].to_string();
        let cloid = c[8].to_string();
        if uidx == u64::MAX { continue; }
        ensure!(&mut ctx, uidx);
        let u = user(uidx);
        match op {
            "order" => {
                let px: f64 = c[4].parse().unwrap_or(0.0);
                let sz: f64 = c[5].parse().unwrap_or(0.0);
                let flags: u64 = c[9].parse().unwrap_or(0);
                let qty = to_raw_qty(sz);
                let praw = to_raw_price(px);
                if qty == 0 || praw <= 0 { continue; }
                let (ot, tif) = map_tif(flags);
                let side = if is_buy == 1 { BUY } else { SELL };
                let input = place_input(side, praw as u64, qty, ot, tif);
                if prof_on {
                    crate::perp_dex::prof::clear();
                }
                let (dt, outcome) = timed_place(&mut ctx, u, &input, via_envelope);
                let snap = if prof_on { Some(crate::perp_dex::prof::snapshot()) } else { None };
                match outcome {
                    None => t.get_mut("place_reject").unwrap().push(dt),
                    Some(oid) => {
                        if !cloid.is_empty() && cloid != "0x" { by_cloid.insert(cloid, oid); }
                        if !hl_oid.is_empty() { by_oid.insert(hl_oid, oid); }
                        if order_matched(&mut ctx, &oid) {
                            t.get_mut("place_match").unwrap().push(dt);
                            if let Some(s) = snap { match_prof.push(s); }
                        } else {
                            t.get_mut("place_rest").unwrap().push(dt);
                            if let Some(s) = snap { rest_prof.push(s); }
                        }
                    }
                }
            }
            "cancel" | "cancelByCloid" => {
                let target = if op == "cancelByCloid" {
                    by_cloid.get(&cloid).copied()
                } else {
                    by_oid.get(&hl_oid).copied()
                };
                let oid = target.unwrap_or([0x55u8; 32]); // never-seen id → genuine miss (81.6%)
                let cin = cancel_input(oid);
                if prof_on {
                    crate::perp_dex::prof::clear();
                }
                let (dt, hit) = timed_cancel(&mut ctx, u, &cin, via_envelope);
                if hit {
                    t.get_mut("cancel_hit").unwrap().push(dt);
                    if prof_on {
                        cancel_prof.push(crate::perp_dex::prof::snapshot());
                    }
                } else {
                    t.get_mut("cancel_miss").unwrap().push(dt);
                }
            }
            _ => {}
        }
    }

    // ---- report ----
    let mut out = String::new();
    out.push_str(&format!(
        "book0: {} rested, {} skipped | window ops replayed: {}\n",
        b_ok, b_skip, n
    ));
    out.push_str("op_type        n       mean_us  p50    p90    p99\n");
    let _ = &try_cancel; // keep helper referenced
    for k in ["place_match", "place_rest", "cancel_hit", "place_reject", "cancel_miss"] {
        let v = t.get_mut(k).unwrap();
        v.sort_unstable();
        let n = v.len();
        let mean = if n > 0 { v.iter().sum::<u64>() as f64 / n as f64 / 1000.0 } else { 0.0 };
        out.push_str(&format!(
            "{:<14} {:<7} {:<8.2} {:<6.2} {:<6.2} {:<6.2}\n",
            k, n, mean,
            pct(v, 0.50) as f64 / 1000.0,
            pct(v, 0.90) as f64 / 1000.0,
            pct(v, 0.99) as f64 / 1000.0,
        ));
    }
    if prof_on {
        // Share of TOTAL engine time in the replayed window (Σ per-op wall time, all op types) —
        // shows which op type actually owns the block's execution budget, not just per-op cost.
        let mut grand = 0f64;
        let mut per_type: Vec<(&str, f64, usize)> = Vec::new();
        for k in ["place_match", "place_rest", "cancel_hit", "place_reject", "cancel_miss"] {
            let v = t.get(k).unwrap();
            let s: f64 = v.iter().map(|&x| x as f64).sum();
            grand += s;
            per_type.push((k, s, v.len()));
        }
        out.push_str("\n-- share of total engine time in the window (DIRECT) --\n");
        out.push_str("op_type        n        total_ms  %of_engine  mean_us\n");
        for (k, s, n) in &per_type {
            out.push_str(&format!(
                "{:<14} {:<8} {:<9.1} {:<11.1} {:.2}\n",
                k, n, s / 1e6, 100.0 * s / grand.max(1.0), s / 1e3 / (*n as f64).max(1.0)
            ));
        }
        out.push_str(&format!("{:<14} {:<8} {:<9.1} {:<11}\n", "TOTAL", "", grand / 1e6, "100.0"));

        const CANCEL_STAGES: &[usize] = &[0, 1, 2, 3, 4, 5, 6];
        const PLACE_STAGES: &[usize] = &[7, 8, 9, 10, 11, 12, 13];
        if !cancel_prof.is_empty() {
            out.push_str(&prof_stage_report("cancel_hit", &cancel_prof, CANCEL_STAGES));
        }
        if !rest_prof.is_empty() {
            out.push_str(&prof_stage_report("place_rest", &rest_prof, PLACE_STAGES));
        }
        if !match_prof.is_empty() {
            out.push_str(&prof_stage_report("place_match", &match_prof, PLACE_STAGES));
        }
    }
    out
}

/// Per-stage breakdown of a set of profiled ops: mean ns/stage + % of mean total, plus the mean
/// stage split of the p50-band (middle 2%) vs the p99-band (slowest 1%) ops — i.e. what makes the
/// slow tail slow. `stages` selects which stage ids belong to this op path.
fn prof_stage_report(label: &str, rows: &[[u64; crate::perp_dex::prof::N]], stages: &[usize]) -> String {
    let n = rows.len();
    let total_of = |r: &[u64; crate::perp_dex::prof::N]| -> u64 { stages.iter().map(|&s| r[s]).sum() };
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by_key(|&i| total_of(&rows[i]));
    // band = mean per-stage over a slice of the sorted-by-total ops.
    let band_mean = |lo: f64, hi: f64, s: usize| -> f64 {
        let (a, b): (usize, usize) =
            (((n as f64 * lo) as usize).min(n - 1), ((n as f64 * hi) as usize).clamp(1, n));
        let sl = &idx[a..b.max(a + 1)];
        sl.iter().map(|&i| rows[i][s] as f64).sum::<f64>() / sl.len() as f64
    };
    let overall_mean = |s: usize| -> f64 { rows.iter().map(|r| r[s] as f64).sum::<f64>() / n as f64 };
    let mean_total: f64 = stages.iter().map(|&s| overall_mean(s)).sum();
    let mut out = String::new();
    out.push_str(&format!(
        "\n-- {label} stage breakdown (n={n}, DIRECT engine) — ns, % of mean, p50-band, p99-band --\n"
    ));
    out.push_str("stage           mean_ns  %mean   p50band  p99band\n");
    for &s in stages {
        let m = overall_mean(s);
        out.push_str(&format!(
            "{:<15} {:<8.0} {:<7.1} {:<8.0} {:<8.0}\n",
            crate::perp_dex::prof::STAGE_NAMES[s],
            m,
            100.0 * m / mean_total.max(1.0),
            band_mean(0.49, 0.51, s),
            band_mean(0.99, 1.0, s),
        ));
    }
    let p50t: f64 = stages.iter().map(|&s| band_mean(0.49, 0.51, s)).sum();
    let p99t: f64 = stages.iter().map(|&s| band_mean(0.99, 1.0, s)).sum();
    out.push_str(&format!(
        "{:<15} {:<8.0} {:<7} {:<8.0} {:<8.0}\n",
        "TOTAL", mean_total, "", p50t, p99t
    ));
    out
}

// ═══════════ HL-window replay against the on-chain engine (hl_window_replay) ═══════════
//
// Extends `faithful_replay` for the hlrej (HL-reject-faithful) datasets + block-boundary costs:
//   (a) hlrej force-price fidelity — `max_price` is raised so the forced $9,999,999 buy
//       passes the bounds check and rejects on the post-only CROSS check (the same path HL
//       used), and forced tiny sell prices ($0.001) clamp to one tick instead of being
//       skipped by the `praw <= 0` guard;
//   (b) warmup replay (untimed) instead of the book0 price shift;
//   (c) cloid maps keyed per (user, cloid) — HL cloids are only unique within a user;
//   (d) `modify` handled as cancel+place (no native modify), timed into its own buckets;
//   (e) optional block boundaries every N rows: the journal's perp delta is harvested
//       (`take_perp_delta`, which pays the dirty-set serialization), the chained block
//       commitment is folded and timed, the delta is merged into a canonical store
//       mirroring reth's `canonical_perp` (bytes + decoded-Arc fast path), and the live
//       store is re-initialized — emulating the fresh-journal-per-block production shape.

use context::journaled_state::PerpBlob;
use primitives::{StorageKey, StorageValue, B256};

/// In-memory stand-in for reth's committed off-trie store (`canonical_perp`): serves the
/// journal's cross-block cold reads once a simulated block boundary has drained the overlay.
#[derive(Debug, Default)]
pub struct CanonPerpDb {
    pub inner: InMemoryDB,
    pub perp: std::collections::HashMap<B256, (Vec<u8>, Option<std::sync::Arc<PerpBlob>>)>,
}

impl database::Database for CanonPerpDb {
    type Error = <InMemoryDB as database::Database>::Error;
    fn basic(&mut self, address: Address) -> Result<Option<state::AccountInfo>, Self::Error> {
        database::Database::basic(&mut self.inner, address)
    }
    fn code_by_hash(&mut self, code_hash: B256) -> Result<state::Bytecode, Self::Error> {
        database::Database::code_by_hash(&mut self.inner, code_hash)
    }
    fn storage(&mut self, address: Address, index: StorageKey) -> Result<StorageValue, Self::Error> {
        database::Database::storage(&mut self.inner, address, index)
    }
    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        database::Database::block_hash(&mut self.inner, number)
    }
    fn perp_storage(&mut self, key: B256) -> Result<Vec<u8>, Self::Error> {
        Ok(self.perp.get(&key).map(|(b, _)| b.clone()).unwrap_or_default())
    }
    fn perp_load_arc(
        &mut self,
        key: B256,
    ) -> Result<Option<std::sync::Arc<PerpBlob>>, Self::Error> {
        Ok(self.perp.get(&key).and_then(|(_, d)| d.clone()))
    }
}

pub type CanonCtx = Context<BlockEnv, TxEnv, CfgEnv, CanonPerpDb, Journal<CanonPerpDb>, ()>;

fn make_ctx_canon() -> CanonCtx {
    let mut ctx: CanonCtx = Context::new(CanonPerpDb::default(), SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

/// `faithful_market` with `max_price` raised above the hlrej forced-buy price
/// ($9,999,999 → raw 999,999,900 at pd2) so forced rejects take the post-only CROSS path.
fn hlrej_market() -> Market {
    let mut m = faithful_market();
    m.max_price = 1_000_000_000;
    m
}

fn to_raw_price_hlrej(px_usd: f64) -> i64 {
    let raw = (px_usd * 100.0).round() as i64;
    let snapped = (raw / 10) * 10;
    // hlrej forced sells are $0.001 → 0 after snap; clamp to one tick so they reach the
    // engine and reject on the cross check instead of being silently skipped.
    if px_usd > 0.0 && snapped == 0 { HL_TICK as i64 } else { snapped }
}

// Generic (any-ContextTr) clones of the small BenchCtx-typed helpers above.
fn fund_g<CTX: ContextTr>(ctx: &mut CTX, user: Address, amount: u64) {
    JournalTr::load_account(ctx.journal_mut(), user).unwrap();
    let mut acc = storage::load_account(ctx, user).unwrap();
    acc.credit_perp(amount).unwrap();
    storage::save_account(ctx, user, acc, AccountUpdateReason::Adjustment).unwrap();
}
fn try_place_g<CTX: ContextTr>(
    ctx: &mut CTX, user: Address, side: u8, price: u64, qty: u64, ot: u8, tif: u8,
) -> Option<[u8; 32]> {
    run_place_order(&place_input(side, price, qty, ot, tif), user, ctx)
        .ok()
        .map(|r| r[..32].try_into().unwrap())
}
fn order_matched_g<CTX: ContextTr>(ctx: &mut CTX, oid: &[u8; 32]) -> bool {
    match storage::load_order_ref(ctx, oid).unwrap() {
        None => true,
        Some(o) => o.filled > 0,
    }
}
std::thread_local! {
    /// Reject-reason histogram for DIRECT-mode places (diagnostic; printed in the report).
    static REJECT_HIST: std::cell::RefCell<std::collections::HashMap<String, u64>> =
        std::cell::RefCell::new(std::collections::HashMap::default());
}

fn timed_place_g<CTX: ContextTr>(
    ctx: &mut CTX, u: Address, input: &[u8], via_envelope: bool,
) -> (u64, Option<[u8; 32]>) {
    if via_envelope {
        let t0 = Instant::now();
        let res = crate::perp_dex::run_perp_dex_call(input, u64::MAX, u, U256::ZERO, false, ctx);
        let dt = t0.elapsed().as_nanos() as u64;
        let oid = match res {
            Ok(o) if !o.reverted => o.bytes.get(..32).and_then(|s| s.try_into().ok()),
            _ => None,
        };
        (dt, oid)
    } else {
        let t0 = Instant::now();
        let res = run_place_order(input, u, ctx);
        let dt = t0.elapsed().as_nanos() as u64;
        match res {
            Ok(r) => (dt, Some(r[..32].try_into().unwrap())),
            Err(e) => {
                REJECT_HIST.with(|h| {
                    *h.borrow_mut().entry(e.to_string()).or_insert(0) += 1;
                });
                (dt, None)
            }
        }
    }
}
fn timed_cancel_g<CTX: ContextTr>(
    ctx: &mut CTX, u: Address, input: &[u8], via_envelope: bool,
) -> (u64, bool) {
    if via_envelope {
        let t0 = Instant::now();
        let res = crate::perp_dex::run_perp_dex_call(input, u64::MAX, u, U256::ZERO, false, ctx);
        let dt = t0.elapsed().as_nanos() as u64;
        (dt, matches!(res, Ok(ref o) if !o.reverted))
    } else {
        let t0 = Instant::now();
        let ok = run_cancel_order(input, u, ctx).is_ok();
        let dt = t0.elapsed().as_nanos() as u64;
        (dt, ok)
    }
}

/// One pre-parsed window op (compact: the timed loop pays engine + map cost, not TSV parsing).
enum WindowOp {
    Place { uidx: u64, side: u8, praw: u64, qty: u64, ot: u8, tif: u8, oid: Box<str>, cloid: Box<str> },
    Cancel { uidx: u64, oid: Box<str> },
    CancelCloid { uidx: u64, cloid: Box<str> },
    Modify { uidx: u64, side: u8, praw: u64, qty: u64, oid: Box<str>, cloid: Box<str> },
}

fn parse_window_op(line: &str) -> Option<WindowOp> {
    let c: Vec<&str> = line.split('\t').collect();
    if c.len() < 10 {
        return None;
    }
    // ts opType assetId isBuy px sz userIdx oid cloid flags
    let uidx: u64 = c[6].parse().ok()?;
    let oid: Box<str> = c[7].into();
    let cloid: Box<str> = c[8].into();
    match c[1] {
        "order" | "modify" => {
            let is_buy: u64 = c[3].parse().unwrap_or(0);
            let px: f64 = c[4].parse().unwrap_or(0.0);
            let sz: f64 = c[5].parse().unwrap_or(0.0);
            let qty = to_raw_qty(sz);
            let praw = to_raw_price_hlrej(px);
            if qty == 0 || praw <= 0 {
                return None;
            }
            let side = if is_buy == 1 { BUY } else { SELL };
            if c[1] == "order" {
                let flags: u64 = c[9].parse().unwrap_or(0);
                let (ot, tif) = map_tif(flags);
                Some(WindowOp::Place { uidx, side, praw: praw as u64, qty, ot, tif, oid, cloid })
            } else {
                Some(WindowOp::Modify { uidx, side, praw: praw as u64, qty, oid, cloid })
            }
        }
        "cancel" => Some(WindowOp::Cancel { uidx, oid }),
        "cancelByCloid" => Some(WindowOp::CancelCloid { uidx, cloid }),
        _ => None,
    }
}

/// Book shape: (bid_levels, ask_levels, live_bid_orders, live_ask_orders).
fn book_stats(ctx: &mut CanonCtx) -> (usize, usize, u64, u64) {
    let bids: Vec<u64> = storage::load_bid_prices_ref(ctx, MARKET_ID).unwrap().to_vec();
    let asks: Vec<u64> = storage::load_ask_prices_ref(ctx, MARKET_ID).unwrap().to_vec();
    let mut nb = 0u64;
    for p in &bids {
        nb += storage::load_bid_count(ctx, MARKET_ID, *p).unwrap();
    }
    let mut na = 0u64;
    for p in &asks {
        na += storage::load_ask_count(ctx, MARKET_ID, *p).unwrap();
    }
    (bids.len(), asks.len(), nb, na)
}

fn drain_reject_hist() -> Vec<(String, u64)> {
    REJECT_HIST.with(|h| {
        let mut v: Vec<(String, u64)> = h.borrow_mut().drain().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    })
}

pub struct HlReplayOpts {
    pub max_ops: usize,
    pub via_envelope: bool,
    /// Simulated block size in rows; 0 = no block boundaries (one giant block, warm store).
    pub block_rows: usize,
}

#[derive(Default)]
struct IdMaps {
    by_oid: HashMap<Box<str>, [u8; 32]>,
    by_cloid: HashMap<(u64, Box<str>), [u8; 32]>,
}

struct BlockStat {
    keys: usize,
    take_ns: u64,
    fold_ns: u64,
}

fn simulate_block_boundary(ctx: &mut CanonCtx, prev_c: &mut U256) -> BlockStat {
    let t0 = Instant::now();
    let delta = ctx.journal_mut().take_perp_delta();
    let take_ns = t0.elapsed().as_nanos() as u64;
    let t1 = Instant::now();
    let c = crate::perp_dex::storage::compute_block_commitment(*prev_c, &delta);
    let fold_ns = t1.elapsed().as_nanos() as u64;
    *prev_c = c;
    let keys = delta.len();
    // Merge into the canonical store (production does this off the execution hot path, so
    // it is deliberately NOT part of take_ns/fold_ns) and hand the journal a fresh live
    // store — the next simulated block cold-reads exactly like a fresh production journal.
    for (k, e) in delta {
        if e.bytes.is_empty() {
            ctx.db_mut().perp.remove(&k);
        } else {
            ctx.db_mut().perp.insert(k, (e.bytes, e.decoded));
        }
    }
    // Fresh live store for the next simulated block (production journals are per-block).
    // In-place reset via downcast: `perp_live_init` asserts single-init, so the installed
    // store is overwritten rather than replaced.
    if let Some(store) = ctx.journal_mut().perp_live_get_mut() {
        if let Some(typed) = store
            .as_any_mut()
            .downcast_mut::<crate::perp_dex::typed_store::TypedPerpStore>()
        {
            *typed = crate::perp_dex::typed_store::TypedPerpStore::default();
        }
    }
    BlockStat { keys, take_ns, fold_ns }
}

/// Executes one op; pushes its engine time into the right bucket. Returns engine calls made.
#[allow(clippy::too_many_arguments)]
fn exec_window_op(
    ctx: &mut CanonCtx,
    op: &WindowOp,
    maps: &mut IdMaps,
    funded: &mut std::collections::HashSet<u64>,
    t: &mut HashMap<&'static str, Vec<u64>>,
    via_envelope: bool,
) -> u32 {
    macro_rules! ensure {
        ($u:expr) => {
            if funded.insert($u) {
                fund_g(ctx, user_addr($u), FAITHFUL_WALLET);
            }
        };
    }
    let mut place = |ctx: &mut CanonCtx,
                     maps: &mut IdMaps,
                     t: &mut HashMap<&'static str, Vec<u64>>,
                     uidx: u64, side: u8, praw: u64, qty: u64, ot: u8, tif: u8,
                     oid: &str, cloid: &str,
                     k_match: &'static str, k_rest: &'static str, k_rej: &'static str| {
        let input = place_input(side, praw, qty, ot, tif);
        let (dt, outcome) = timed_place_g(ctx, user_addr(uidx), &input, via_envelope);
        match outcome {
            None => t.get_mut(k_rej).unwrap().push(dt),
            Some(engine_oid) => {
                if !cloid.is_empty() && cloid != "0x" {
                    maps.by_cloid.insert((uidx, cloid.into()), engine_oid);
                }
                if !oid.is_empty() {
                    maps.by_oid.insert(oid.into(), engine_oid);
                }
                if order_matched_g(ctx, &engine_oid) {
                    t.get_mut(k_match).unwrap().push(dt);
                } else {
                    t.get_mut(k_rest).unwrap().push(dt);
                }
            }
        }
    };
    // Three-way outcome, aligned with the offchain harness's map-based split PLUS the
    // engine-level truth: `hit` = map found AND the order was still live (engine Ok);
    // `stale` = map found but the order already terminated (delete-on-terminal → same
    // not-found path as miss, kept separate so the map-based rate stays comparable);
    // `miss` = oid/cloid never accepted → sentinel id → genuine not-found path.
    let cancel = |ctx: &mut CanonCtx,
                  maps: &mut IdMaps,
                  t: &mut HashMap<&'static str, Vec<u64>>,
                  uidx: u64, target: Option<[u8; 32]>,
                  k_hit: &'static str, k_stale: &'static str, k_miss: &'static str|
     -> bool {
        let _ = maps;
        let mapped = target.is_some();
        let oid = target.unwrap_or([0x55u8; 32]); // never-seen id → genuine miss path
        let cin = cancel_input(oid);
        let (dt, ok) = timed_cancel_g(ctx, user_addr(uidx), &cin, via_envelope);
        let k = if ok { k_hit } else if mapped { k_stale } else { k_miss };
        t.get_mut(k).unwrap().push(dt);
        ok
    };
    match op {
        WindowOp::Place { uidx, side, praw, qty, ot, tif, oid, cloid } => {
            ensure!(*uidx);
            place(ctx, maps, t, *uidx, *side, *praw, *qty, *ot, *tif, oid, cloid,
                  "place_match", "place_rest", "place_reject");
            1
        }
        WindowOp::Cancel { uidx, oid } => {
            ensure!(*uidx);
            let target = maps.by_oid.get(oid.as_ref()).copied();
            cancel(ctx, maps, t, *uidx, target, "cancel_hit", "cancel_stale", "cancel_miss");
            1
        }
        WindowOp::CancelCloid { uidx, cloid } => {
            ensure!(*uidx);
            let target = maps.by_cloid.get(&(*uidx, cloid.clone())).copied();
            cancel(ctx, maps, t, *uidx, target, "cancel_hit", "cancel_stale", "cancel_miss");
            1
        }
        WindowOp::Modify { uidx, side, praw, qty, oid, cloid } => {
            ensure!(*uidx);
            let target = maps
                .by_oid
                .get(oid.as_ref())
                .copied()
                .or_else(|| maps.by_cloid.get(&(*uidx, cloid.clone())).copied());
            cancel(ctx, maps, t, *uidx, target, "mod_cancel_hit", "mod_cancel_stale", "mod_cancel_miss");
            place(ctx, maps, t, *uidx, *side, *praw, *qty, LIMIT, 0 /*GTC*/, oid, cloid,
                  "mod_place_match", "mod_place_rest", "mod_place_reject");
            2
        }
    }
}

const REPLAY_BUCKETS: [&str; 12] = [
    "place_match", "place_rest", "place_reject",
    "cancel_hit", "cancel_stale", "cancel_miss",
    "mod_cancel_hit", "mod_cancel_stale", "mod_cancel_miss",
    "mod_place_match", "mod_place_rest", "mod_place_reject",
];

/// Seed book0 → replay warmup (untimed) → replay the timed window, with optional
/// simulated block boundaries. Returns a text report.
pub fn hl_window_replay(book0_path: &str, warmup_path: Option<&str>, ops_path: &str, o: HlReplayOpts) -> String {
    let mut ctx = make_ctx_canon();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    storage::save_market(&mut ctx, &hlrej_market()).unwrap();
    let mut funded: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut maps = IdMaps::default();
    let mut out = String::new();

    // ---- seed book0 (no price shift: warmup replay carries the gap when provided) ----
    let shift: i64 = if warmup_path.is_some() { 0 } else { HL_BOOK0_SHIFT };
    let b = std::fs::read_to_string(book0_path).expect("read book0");
    let (mut b_ok, mut b_skip) = (0u64, 0u64);
    for line in b.lines() {
        if line.is_empty() { continue; }
        let c: Vec<&str> = line.split('\t').collect();
        if c.len() < 7 { continue; }
        // assetId isBuy px sz userIdx oid placeTs
        let is_buy: u64 = c[1].parse().unwrap_or(0);
        let px: f64 = c[2].parse().unwrap_or(0.0);
        let sz: f64 = c[3].parse().unwrap_or(0.0);
        let uidx: u64 = c[4].parse().unwrap_or(u64::MAX);
        let qty = to_raw_qty(sz);
        let praw = to_raw_price_hlrej(px) + shift;
        if qty == 0 || praw <= 0 || uidx == u64::MAX { b_skip += 1; continue; }
        if funded.insert(uidx) {
            fund_g(&mut ctx, user_addr(uidx), FAITHFUL_WALLET);
        }
        let side = if is_buy == 1 { BUY } else { SELL };
        match try_place_g(&mut ctx, user_addr(uidx), side, praw as u64, qty, LIMIT, POST_ONLY) {
            Some(engine_oid) => {
                maps.by_oid.insert(c[5].into(), engine_oid);
                b_ok += 1;
            }
            None => b_skip += 1,
        }
    }
    out.push_str(&format!("book0: {b_ok} rested, {b_skip} skipped (shift {shift})\n"));

    // ---- untimed buckets for warmup (thrown away; the calls still mutate state) ----
    let mut t: HashMap<&'static str, Vec<u64>> = HashMap::default();
    for k in REPLAY_BUCKETS { t.insert(k, Vec::new()); }

    if let Some(wp) = warmup_path {
        let w = std::fs::read_to_string(wp).expect("read warmup");
        let mut wn = 0u64;
        for line in w.lines() {
            if line.is_empty() { continue; }
            if let Some(op) = parse_window_op(line) {
                exec_window_op(&mut ctx, &op, &mut maps, &mut funded, &mut t, o.via_envelope);
                wn += 1;
            }
        }
        out.push_str(&format!("warmup: {wn} ops replayed (untimed)\n"));
        for k in REPLAY_BUCKETS { t.get_mut(k).unwrap().clear(); }
        let (bl, al, nb, na) = book_stats(&mut ctx);
        out.push_str(&format!(
            "post-warmup book: {bl} bid levels ({nb} live) / {al} ask levels ({na} live)\n"
        ));
        for (reason, n) in drain_reject_hist() {
            out.push_str(&format!("  warmup reject {n:>9}  {reason}\n"));
        }
    }

    // Drain the giant seed+warmup delta so the first timed block starts clean.
    let mut prev_c = U256::ZERO;
    if o.block_rows > 0 {
        let bs = simulate_block_boundary(&mut ctx, &mut prev_c);
        out.push_str(&format!(
            "warmup-end boundary: {} keys, take {:.1} ms, fold {:.1} ms\n",
            bs.keys, bs.take_ns as f64 / 1e6, bs.fold_ns as f64 / 1e6
        ));
    }

    // ---- pre-parse the timed window ----
    let raw = std::fs::read_to_string(ops_path).expect("read ops");
    let mut ops: Vec<WindowOp> = Vec::new();
    for line in raw.lines() {
        if ops.len() >= o.max_ops { break; }
        if line.is_empty() { continue; }
        if let Some(op) = parse_window_op(line) {
            ops.push(op);
        }
    }
    drop(raw);

    // ---- timed replay ----
    let mut blocks: Vec<BlockStat> = Vec::new();
    let mut engine_calls: u64 = 0;
    let wall0 = Instant::now();
    for (i, op) in ops.iter().enumerate() {
        engine_calls += exec_window_op(&mut ctx, op, &mut maps, &mut funded, &mut t, o.via_envelope) as u64;
        if o.block_rows > 0 && (i + 1) % o.block_rows == 0 {
            blocks.push(simulate_block_boundary(&mut ctx, &mut prev_c));
        }
    }
    if o.block_rows > 0 && ops.len() % o.block_rows != 0 {
        blocks.push(simulate_block_boundary(&mut ctx, &mut prev_c));
    }
    let wall_ns = wall0.elapsed().as_nanos() as u64;
    {
        let (bl, al, nb, na) = book_stats(&mut ctx);
        out.push_str(&format!(
            "post-window book: {bl} bid levels ({nb} live) / {al} ask levels ({na} live)\n"
        ));
        for (reason, n) in drain_reject_hist() {
            out.push_str(&format!("  window reject {n:>9}  {reason}\n"));
        }
    }

    // ---- report ----
    out.push_str(&format!(
        "window: {} rows replayed, {} engine calls, mode {}\n\n",
        ops.len(),
        engine_calls,
        if o.via_envelope { "ENVELOPE" } else { "DIRECT" }
    ));
    out.push_str("op_type          n         mean_us  p50    p90    p99\n");
    let mut engine_ns: u64 = 0;
    for k in REPLAY_BUCKETS {
        let v = t.get_mut(k).unwrap();
        v.sort_unstable();
        let n = v.len();
        engine_ns += v.iter().sum::<u64>();
        let mean = if n > 0 { v.iter().sum::<u64>() as f64 / n as f64 / 1000.0 } else { 0.0 };
        out.push_str(&format!(
            "{:<16} {:<9} {:<8.2} {:<6.2} {:<6.2} {:<6.2}\n",
            k, n, mean,
            pct(v, 0.50) as f64 / 1000.0,
            pct(v, 0.90) as f64 / 1000.0,
            pct(v, 0.99) as f64 / 1000.0,
        ));
    }
    let blk_take: u64 = blocks.iter().map(|b| b.take_ns).sum();
    let blk_fold: u64 = blocks.iter().map(|b| b.fold_ns).sum();
    out.push_str(&format!(
        "\nengine time: {:.3} s | engine calls/s {:.0} | rows/s (engine-only) {:.0}\n",
        engine_ns as f64 / 1e9,
        engine_calls as f64 * 1e9 / engine_ns.max(1) as f64,
        ops.len() as f64 * 1e9 / engine_ns.max(1) as f64,
    ));
    out.push_str(&format!(
        "loop wall:   {:.3} s | rows/s (wall, incl. maps+boundaries) {:.0}\n",
        wall_ns as f64 / 1e9,
        ops.len() as f64 * 1e9 / wall_ns.max(1) as f64,
    ));
    if !blocks.is_empty() {
        let n = blocks.len() as f64;
        let mut keys: Vec<usize> = blocks.iter().map(|b| b.keys).collect();
        keys.sort_unstable();
        let mut takes: Vec<u64> = blocks.iter().map(|b| b.take_ns).collect();
        takes.sort_unstable();
        let mut folds: Vec<u64> = blocks.iter().map(|b| b.fold_ns).collect();
        folds.sort_unstable();
        out.push_str(&format!(
            "\nblocks: {} × {} rows | keys/block p50 {} max {} | take_delta us mean {:.1} p50 {:.1} p99 {:.1} | commit-fold us mean {:.1} p50 {:.1} p99 {:.1}\n",
            blocks.len(), o.block_rows,
            keys[keys.len() / 2], keys[keys.len() - 1],
            blk_take as f64 / n / 1e3,
            takes[takes.len() / 2] as f64 / 1e3,
            pct(&takes, 0.99) as f64 / 1e3,
            blk_fold as f64 / n / 1e3,
            folds[folds.len() / 2] as f64 / 1e3,
            pct(&folds, 0.99) as f64 / 1e3,
        ));
        out.push_str(&format!(
            "block-end overhead: {:.3} s total = {:.1}% of engine time = {:.0} ns/row amortized | final C = {:#x}\n",
            (blk_take + blk_fold) as f64 / 1e9,
            100.0 * (blk_take + blk_fold) as f64 / engine_ns.max(1) as f64,
            (blk_take + blk_fold) as f64 / ops.len().max(1) as f64,
            prev_c,
        ));
    }
    out
}
