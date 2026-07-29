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
    types::{Market, OrderEntry},
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
        // Wide band so Sim C's marketable fills are never rejected by the fill-time off-mark guard
        // (there is NO placement-time band on this tip — deep passive orders always rest).
        price_band_bps: 1_000_000,
        // Mark anchored at BASE so Sim C's taker (which fills near BASE) is well inside the band.
        mark_price: BASE,
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
    storage::save_account(ctx, user, acc).unwrap();
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
        let (a, b) = (((n as f64 * lo) as usize).min(n - 1), ((n as f64 * hi) as usize).clamp(1, n));
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
