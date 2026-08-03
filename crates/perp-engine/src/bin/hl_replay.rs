//! HL mainnet window replay against the engine on [`InMemoryHost`] — the engine-only
//! benchmark harness. No revm context, no journal, no EVM crates: this binary builds
//! from `perp-engine` + `perp-core` + the interface seam alone.
//!
//! Replays a `hl-window-*` dataset export (plain TSV; `zstd -d` the archive first):
//! seeds `book0.tsv`, replays `warmup-ops.tsv` untimed, then times every op of
//! `ops.tsv` through the real engine entry points.
//!
//! Usage:
//!   hl_replay --book0 <book0.tsv> --ops <ops.tsv> [--warmup <warmup-ops.tsv>]
//!             [--max-ops N] [--envelope] [--block-rows N] [--now UNIX_SECONDS]
//!
//! Modes:
//!   DIRECT (default)  times `run_place_order` / `run_cancel_order` — the engine itself.
//!   --envelope        times `run_perp_dex_call` — adds the call shell (selector dispatch,
//!                     gas table, depth gate, revert encoding).
//!   --block-rows N    simulates a block boundary every N rows: `InMemoryHost::end_block`
//!                     (delta harvest + canonical merge + live-store reset) plus the
//!                     chained commitment fold, both timed. NOTE: unlike the journal-host
//!                     harness, the harvest timing here INCLUDES the canonical-map merge.
//!
//! The dataset column layout and the op → engine-call mapping mirror the journal-host
//! harness (`revm-precompile`'s `bench_util`), so results are directly comparable; the
//! only intentional difference is the missing journal bookkeeping layer.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use alloy_sol_types::SolCall;
use perp_core::compute_block_commitment;
use perp_engine::{
    interface::IPerpDex::{cancelOrderCall, placeOrderCall},
    run_perp_dex_call, storage,
    trading::{run_cancel_order, run_place_order},
    types::Market,
    InMemoryHost, PerpHost,
};
use primitives::{address, Address, FixedBytes, U256};

// ── Faithful-config constants (identical to the journal-host harness) ────────

const MARKET_ID: u64 = 1;
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
const HL_MAXQTY: u64 = 34_500_000;
const HL_TICK: u64 = 10;
const HL_BOOK0_SHIFT: i64 = -27450;
const WALLET: u64 = 1_000_000_000_000_000_000;

const BUY: u8 = 0;
const SELL: u8 = 1;
const LIMIT: u8 = 0;
const MARKET: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;
const POST_ONLY: u8 = 3;

/// bd5 / pd2 / tick10 faithful market; `max_price` sits above the hlrej forced-buy
/// ($9,999,999 → raw 999,999,900) so forced rejects take the post-only CROSS path.
fn market() -> Market {
    Market {
        market_id: MARKET_ID,
        base_decimals: 5,
        price_decimals: 2,
        tick_size: HL_TICK,
        step_size: 1,
        min_quantity: 1,
        max_quantity: HL_MAXQTY,
        max_price: 1_000_000_000,
        price_update_interval: 15,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 0,
        price_band_bps: 1_000_000, // disabled
        mark_price: 0,
    }
}

fn user_addr(i: u64) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0x20;
    b[12..20].copy_from_slice(&i.to_be_bytes());
    Address::from(b)
}

fn to_raw_price(px_usd: f64) -> i64 {
    let raw = (px_usd * 100.0).round() as i64;
    let snapped = (raw / 10) * 10;
    // hlrej forced sells are $0.001 → 0 after snap; clamp to one tick so they reach the
    // engine and reject on the cross check instead of being silently skipped.
    if px_usd > 0.0 && snapped == 0 { HL_TICK as i64 } else { snapped }
}

fn to_raw_qty(sz: f64) -> u64 {
    let raw = (sz * 100_000.0).round() as i64;
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
        (LIMIT, GTC)
    }
}

fn place_input(side: u8, price: u64, qty: u64, ot: u8, tif: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: ot,
        tif,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode()
}

fn cancel_input(order_id: [u8; 32]) -> Vec<u8> {
    cancelOrderCall { orderId: FixedBytes(order_id), marketId: MARKET_ID }.abi_encode()
}

// ── Timed engine calls (DIRECT vs ENVELOPE) ──────────────────────────────────

fn timed_place(
    host: &mut InMemoryHost,
    u: Address,
    input: &[u8],
    envelope: bool,
    rejects: &mut HashMap<String, u64>,
) -> (u64, Option<[u8; 32]>) {
    if envelope {
        let t0 = Instant::now();
        let res = run_perp_dex_call(input, u64::MAX, u, U256::ZERO, false, host);
        let dt = t0.elapsed().as_nanos() as u64;
        let oid = match res {
            Ok(o) if !o.reverted => o.bytes.get(..32).and_then(|s| s.try_into().ok()),
            _ => None,
        };
        (dt, oid)
    } else {
        let t0 = Instant::now();
        let res = run_place_order(input, u, host);
        let dt = t0.elapsed().as_nanos() as u64;
        match res {
            Ok(r) => (dt, Some(r[..32].try_into().unwrap())),
            Err(e) => {
                *rejects.entry(e.to_string()).or_insert(0) += 1;
                (dt, None)
            }
        }
    }
}

fn timed_cancel(host: &mut InMemoryHost, u: Address, input: &[u8], envelope: bool) -> (u64, bool) {
    if envelope {
        let t0 = Instant::now();
        let res = run_perp_dex_call(input, u64::MAX, u, U256::ZERO, false, host);
        let dt = t0.elapsed().as_nanos() as u64;
        (dt, matches!(res, Ok(ref o) if !o.reverted))
    } else {
        let t0 = Instant::now();
        let ok = run_cancel_order(input, u, host).is_ok();
        let dt = t0.elapsed().as_nanos() as u64;
        (dt, ok)
    }
}

// ── Ops ───────────────────────────────────────────────────────────────────────

enum Op {
    Place { uidx: u64, side: u8, praw: u64, qty: u64, ot: u8, tif: u8, oid: Box<str>, cloid: Box<str> },
    Cancel { uidx: u64, oid: Box<str> },
    CancelCloid { uidx: u64, cloid: Box<str> },
    Modify { uidx: u64, side: u8, praw: u64, qty: u64, oid: Box<str>, cloid: Box<str> },
}

fn parse_op(line: &str) -> Option<Op> {
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
            let praw = to_raw_price(px);
            if qty == 0 || praw <= 0 {
                return None;
            }
            let side = if is_buy == 1 { BUY } else { SELL };
            if c[1] == "order" {
                let flags: u64 = c[9].parse().unwrap_or(0);
                let (ot, tif) = map_tif(flags);
                Some(Op::Place { uidx, side, praw: praw as u64, qty, ot, tif, oid, cloid })
            } else {
                Some(Op::Modify { uidx, side, praw: praw as u64, qty, oid, cloid })
            }
        }
        "cancel" => Some(Op::Cancel { uidx, oid }),
        "cancelByCloid" => Some(Op::CancelCloid { uidx, cloid }),
        _ => None,
    }
}

const BUCKETS: [&str; 12] = [
    "place_match", "place_rest", "place_reject",
    "cancel_hit", "cancel_stale", "cancel_miss",
    "mod_cancel_hit", "mod_cancel_stale", "mod_cancel_miss",
    "mod_place_match", "mod_place_rest", "mod_place_reject",
];

#[derive(Default)]
struct Maps {
    by_oid: HashMap<Box<str>, [u8; 32]>,
    by_cloid: HashMap<(u64, Box<str>), [u8; 32]>,
}

struct Replayer {
    host: InMemoryHost,
    maps: Maps,
    funded: HashSet<u64>,
    t: HashMap<&'static str, Vec<u64>>,
    rejects: HashMap<String, u64>,
    envelope: bool,
    engine_calls: u64,
}

impl Replayer {
    fn ensure(&mut self, uidx: u64) {
        if self.funded.insert(uidx) {
            let u = user_addr(uidx);
            let mut acc = storage::load_account(&mut self.host, u).unwrap();
            acc.credit_perp(WALLET).unwrap();
            storage::save_account(&mut self.host, u, acc).unwrap();
        }
    }

    fn order_matched(&mut self, oid: &[u8; 32]) -> bool {
        match storage::load_order_ref(&mut self.host, oid).unwrap() {
            None => true, // terminal / deleted → fully matched (or IOC expired)
            Some(o) => o.filled > 0,
        }
    }

    fn place(
        &mut self, uidx: u64, side: u8, praw: u64, qty: u64, ot: u8, tif: u8,
        oid: &str, cloid: &str, k_match: &'static str, k_rest: &'static str, k_rej: &'static str,
    ) {
        let input = place_input(side, praw, qty, ot, tif);
        let (dt, outcome) =
            timed_place(&mut self.host, user_addr(uidx), &input, self.envelope, &mut self.rejects);
        match outcome {
            None => self.t.get_mut(k_rej).unwrap().push(dt),
            Some(engine_oid) => {
                if !cloid.is_empty() && cloid != "0x" {
                    self.maps.by_cloid.insert((uidx, cloid.into()), engine_oid);
                }
                if !oid.is_empty() {
                    self.maps.by_oid.insert(oid.into(), engine_oid);
                }
                let k = if self.order_matched(&engine_oid) { k_match } else { k_rest };
                self.t.get_mut(k).unwrap().push(dt);
            }
        }
    }

    /// Three-way cancel outcome: `hit` = still-live order cancelled; `stale` = id was
    /// accepted once but the order already terminated (same not-found path as miss,
    /// kept separate so the map-based rate stays comparable); `miss` = never accepted.
    fn cancel(
        &mut self, uidx: u64, target: Option<[u8; 32]>,
        k_hit: &'static str, k_stale: &'static str, k_miss: &'static str,
    ) {
        let mapped = target.is_some();
        let oid = target.unwrap_or([0x55u8; 32]);
        let cin = cancel_input(oid);
        let (dt, ok) = timed_cancel(&mut self.host, user_addr(uidx), &cin, self.envelope);
        let k = if ok { k_hit } else if mapped { k_stale } else { k_miss };
        self.t.get_mut(k).unwrap().push(dt);
    }

    fn exec(&mut self, op: &Op) {
        match op {
            Op::Place { uidx, side, praw, qty, ot, tif, oid, cloid } => {
                self.ensure(*uidx);
                self.place(*uidx, *side, *praw, *qty, *ot, *tif, oid, cloid,
                           "place_match", "place_rest", "place_reject");
                self.engine_calls += 1;
            }
            Op::Cancel { uidx, oid } => {
                self.ensure(*uidx);
                let target = self.maps.by_oid.get(oid.as_ref()).copied();
                self.cancel(*uidx, target, "cancel_hit", "cancel_stale", "cancel_miss");
                self.engine_calls += 1;
            }
            Op::CancelCloid { uidx, cloid } => {
                self.ensure(*uidx);
                let target = self.maps.by_cloid.get(&(*uidx, cloid.clone())).copied();
                self.cancel(*uidx, target, "cancel_hit", "cancel_stale", "cancel_miss");
                self.engine_calls += 1;
            }
            Op::Modify { uidx, side, praw, qty, oid, cloid } => {
                self.ensure(*uidx);
                let target = self
                    .maps
                    .by_oid
                    .get(oid.as_ref())
                    .copied()
                    .or_else(|| self.maps.by_cloid.get(&(*uidx, cloid.clone())).copied());
                self.cancel(*uidx, target, "mod_cancel_hit", "mod_cancel_stale", "mod_cancel_miss");
                self.place(*uidx, *side, *praw, *qty, LIMIT, GTC, oid, cloid,
                           "mod_place_match", "mod_place_rest", "mod_place_reject");
                self.engine_calls += 2;
            }
        }
    }

    fn book_stats(&mut self) -> (usize, usize, u64, u64) {
        let bids: Vec<u64> = storage::load_bid_prices_ref(&mut self.host, MARKET_ID).unwrap().to_vec();
        let asks: Vec<u64> = storage::load_ask_prices_ref(&mut self.host, MARKET_ID).unwrap().to_vec();
        let mut nb = 0u64;
        for p in &bids {
            nb += storage::load_bid_count(&mut self.host, MARKET_ID, *p).unwrap();
        }
        let mut na = 0u64;
        for p in &asks {
            na += storage::load_ask_count(&mut self.host, MARKET_ID, *p).unwrap();
        }
        (bids.len(), asks.len(), nb, na)
    }
}

fn pct(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 * q) as usize).min(sorted.len() - 1);
    sorted[i]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| -> Option<String> {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
    };
    let book0 = get("--book0").expect("--book0 <path> required");
    let ops_path = get("--ops").expect("--ops <path> required");
    let warmup = get("--warmup");
    let max_ops: usize = get("--max-ops").map(|v| v.parse().unwrap()).unwrap_or(usize::MAX);
    let block_rows: usize = get("--block-rows").map(|v| v.parse().unwrap()).unwrap_or(0);
    let now: u64 = get("--now").map(|v| v.parse().unwrap()).unwrap_or(1);
    let envelope = args.iter().any(|a| a == "--envelope");

    eprintln!(
        "hl_replay (InMemoryHost) | book0={book0} warmup={warmup:?} ops={ops_path} \
         max_ops={max_ops} mode={} block_rows={block_rows}",
        if envelope { "ENVELOPE" } else { "DIRECT" }
    );

    let mut r = Replayer {
        host: InMemoryHost::new(now),
        maps: Maps::default(),
        funded: HashSet::new(),
        t: BUCKETS.iter().map(|k| (*k, Vec::new())).collect(),
        rejects: HashMap::new(),
        envelope,
        engine_calls: 0,
    };
    storage::save_admin(&mut r.host, ADMIN).unwrap();
    storage::save_market(&mut r.host, &market()).unwrap();

    // ── seed book0 (no price shift when warmup replays the gap) ──
    let shift: i64 = if warmup.is_some() { 0 } else { HL_BOOK0_SHIFT };
    let b = std::fs::read_to_string(&book0).expect("read book0");
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
        let praw = to_raw_price(px) + shift;
        if qty == 0 || praw <= 0 || uidx == u64::MAX {
            b_skip += 1;
            continue;
        }
        r.ensure(uidx);
        let side = if is_buy == 1 { BUY } else { SELL };
        let input = place_input(side, praw as u64, qty, LIMIT, POST_ONLY);
        match run_place_order(&input, user_addr(uidx), &mut r.host) {
            Ok(ret) => {
                r.maps.by_oid.insert(c[5].into(), ret[..32].try_into().unwrap());
                b_ok += 1;
            }
            Err(_) => b_skip += 1,
        }
    }
    println!("book0: {b_ok} rested, {b_skip} skipped (shift {shift})");

    // ── warmup (untimed; buckets discarded, state kept) ──
    if let Some(wp) = &warmup {
        let w = std::fs::read_to_string(wp).expect("read warmup");
        let mut wn = 0u64;
        for line in w.lines() {
            if line.is_empty() { continue; }
            if let Some(op) = parse_op(line) {
                r.exec(&op);
                wn += 1;
            }
        }
        println!("warmup: {wn} ops replayed (untimed)");
        for k in BUCKETS {
            r.t.get_mut(k).unwrap().clear();
        }
        let (bl, al, nb, na) = r.book_stats();
        println!("post-warmup book: {bl} bid levels ({nb} live) / {al} ask levels ({na} live)");
        for (reason, n) in {
            let mut v: Vec<_> = r.rejects.drain().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1));
            v
        } {
            println!("  warmup reject {n:>9}  {reason}");
        }
        r.engine_calls = 0;
    }

    // Drain the giant seed+warmup delta so the first timed block starts clean.
    let mut prev_c = U256::ZERO;
    if block_rows > 0 {
        let t0 = Instant::now();
        let delta = r.host.end_block();
        let end_ms = t0.elapsed().as_millis();
        let t1 = Instant::now();
        prev_c = compute_block_commitment(prev_c, &delta);
        println!(
            "warmup-end boundary: {} keys, end_block {} ms, fold {:.1} ms",
            delta.len(), end_ms, t1.elapsed().as_nanos() as f64 / 1e6
        );
    }

    // ── pre-parse the timed window ──
    let raw = std::fs::read_to_string(&ops_path).expect("read ops");
    let mut ops: Vec<Op> = Vec::new();
    for line in raw.lines() {
        if ops.len() >= max_ops { break; }
        if line.is_empty() { continue; }
        if let Some(op) = parse_op(line) {
            ops.push(op);
        }
    }
    drop(raw);

    // ── timed replay ──
    struct BlockStat { keys: usize, end_ns: u64, fold_ns: u64 }
    let mut blocks: Vec<BlockStat> = Vec::new();
    let mut boundary = |r: &mut Replayer, prev_c: &mut U256, blocks: &mut Vec<BlockStat>| {
        let t0 = Instant::now();
        let delta = r.host.end_block();
        let end_ns = t0.elapsed().as_nanos() as u64;
        let t1 = Instant::now();
        *prev_c = compute_block_commitment(*prev_c, &delta);
        let fold_ns = t1.elapsed().as_nanos() as u64;
        blocks.push(BlockStat { keys: delta.len(), end_ns, fold_ns });
    };
    let wall0 = Instant::now();
    for (i, op) in ops.iter().enumerate() {
        r.exec(op);
        if block_rows > 0 && (i + 1) % block_rows == 0 {
            boundary(&mut r, &mut prev_c, &mut blocks);
        }
    }
    if block_rows > 0 && ops.len() % block_rows != 0 {
        boundary(&mut r, &mut prev_c, &mut blocks);
    }
    let wall_ns = wall0.elapsed().as_nanos() as u64;

    // ── report ──
    {
        let (bl, al, nb, na) = r.book_stats();
        println!("post-window book: {bl} bid levels ({nb} live) / {al} ask levels ({na} live)");
        let mut v: Vec<_> = r.rejects.drain().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        for (reason, n) in v {
            println!("  window reject {n:>9}  {reason}");
        }
    }
    println!(
        "window: {} rows replayed, {} engine calls, mode {}\n",
        ops.len(), r.engine_calls,
        if envelope { "ENVELOPE" } else { "DIRECT" }
    );
    println!("op_type          n         mean_us  p50    p90    p99");
    let mut engine_ns: u64 = 0;
    for k in BUCKETS {
        let v = r.t.get_mut(k).unwrap();
        v.sort_unstable();
        let n = v.len();
        engine_ns += v.iter().sum::<u64>();
        let mean = if n > 0 { v.iter().sum::<u64>() as f64 / n as f64 / 1000.0 } else { 0.0 };
        println!(
            "{:<16} {:<9} {:<8.2} {:<6.2} {:<6.2} {:<6.2}",
            k, n, mean,
            pct(v, 0.50) as f64 / 1000.0,
            pct(v, 0.90) as f64 / 1000.0,
            pct(v, 0.99) as f64 / 1000.0,
        );
    }
    println!(
        "\nengine time: {:.3} s | engine calls/s {:.0} | rows/s (engine-only) {:.0}",
        engine_ns as f64 / 1e9,
        r.engine_calls as f64 * 1e9 / engine_ns.max(1) as f64,
        ops.len() as f64 * 1e9 / engine_ns.max(1) as f64,
    );
    println!(
        "loop wall:   {:.3} s | rows/s (wall, incl. maps+boundaries) {:.0}",
        wall_ns as f64 / 1e9,
        ops.len() as f64 * 1e9 / wall_ns.max(1) as f64,
    );
    if !blocks.is_empty() {
        let n = blocks.len() as f64;
        let mut keys: Vec<usize> = blocks.iter().map(|b| b.keys).collect();
        keys.sort_unstable();
        let mut ends: Vec<u64> = blocks.iter().map(|b| b.end_ns).collect();
        ends.sort_unstable();
        let mut folds: Vec<u64> = blocks.iter().map(|b| b.fold_ns).collect();
        folds.sort_unstable();
        let (blk_end, blk_fold): (u64, u64) =
            (blocks.iter().map(|b| b.end_ns).sum(), blocks.iter().map(|b| b.fold_ns).sum());
        println!(
            "\nblocks: {} × {} rows | keys/block p50 {} max {} | end_block us mean {:.1} p50 {:.1} p99 {:.1} | commit-fold us mean {:.1} p50 {:.1} p99 {:.1}",
            blocks.len(), block_rows,
            keys[keys.len() / 2], keys[keys.len() - 1],
            blk_end as f64 / n / 1e3, ends[ends.len() / 2] as f64 / 1e3, pct(&ends, 0.99) as f64 / 1e3,
            blk_fold as f64 / n / 1e3, folds[folds.len() / 2] as f64 / 1e3, pct(&folds, 0.99) as f64 / 1e3,
        );
        println!(
            "block-end overhead: {:.3} s total = {:.1}% of engine time = {:.0} ns/row amortized | final C = {prev_c:#x}",
            (blk_end + blk_fold) as f64 / 1e9,
            100.0 * (blk_end + blk_fold) as f64 / engine_ns.max(1) as f64,
            (blk_end + blk_fold) as f64 / ops.len().max(1) as f64,
        );
    }
}
