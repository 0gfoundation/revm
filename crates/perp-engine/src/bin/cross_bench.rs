//! Synthetic cross-margin benches (cross margin P4) against the engine on [`InMemoryHost`] — the
//! before/after yardstick of the `cross_legs` account summary. Like `hl_replay`, no revm context:
//! every timed call goes through the real entry point (`run_perp_dex_call`, the shell's selector
//! dispatch and gas table included).
//!
//! State is built by real calls (orders, fills, `setMarginType`, `setLeverage`, oracle ticks) plus
//! a few explicit storage fixtures (wallet balances, the insurance fund, a mark set without a
//! tick), then harvested with `InMemoryHost::end_block`, so a timed call starts from a block-start
//! state: it cold-fills every blob it touches from the committed store (decoded `Arc`s, reth's
//! `canonical_perp` shape) exactly as the first touch in a block does on chain. Every repetition
//! restores the same committed snapshot, so repetitions are identical runs.
//!
//! Scenarios (deterministic: `--seed`, default 42):
//!
//!   s1    SWEEP. N accounts (`--mode cross|isolated`), each long `Q` in k markets against an
//!         ISOLATED house, so all N sit in market 1's registry. Times ONE `updateIndexPrice` of
//!         market 1: `noliq` (−5%: nobody below maintenance — the pure detection cost) and `liq`
//!         (−22%, after ~`--liq-pct`% of the accounts were made thin: cross by a wallet fixture at
//!         `Σ MM + 10%` of one leg, isolated by 3x leverage in market 1), the closes filling a
//!         resting bid at the new mark. `--analysis` counts one `noliq` tick's cold loads by
//!         namespace and (cross) splits one candidate's cost into its loads and the fold's compute.
//!   s2    ADL. One cross account long market 1 only, bankrupt at the −22% mark (wallet 20% of
//!         its notional), whose whole position M cross winners hold the other side of (each short
//!         `Q` in market 1 and long `Q` in markets 2..=k, wallets random). Times `tick` (the
//!         `updateIndexPrice` that finds it: detection over all M + 1, the episode, the ADL) and
//!         `manual` (`liquidate()` at a mark set without a tick: the episode and its ADL only).
//!   s3    ORDER PATH. One account (`--mode`) long `Q` in k markets: `place`+`cancel` a resting
//!         GTC buy `--ops` times, then a taker loop of `--ops` IOC fills alternating an OPENING buy
//!         and a REDUCING sell against a deep isolated maker. `--block-ops B` ends a block every B
//!         calls (default 1: every call is its own block — the worst case for a per-account blob,
//!         whose clone, encode and hash then land on every call); the boundary is timed separately.
//!   blob  the stored size of the account / position / user-market-index blobs of a cross account
//!         holding k = 1, 4, 16 positions.
//!   all   the whole baseline matrix (`--quick` for a smoke-sized one).
//!
//! Usage:
//!   cross_bench s1 --mode cross|isolated --n N --k K [--reps R] [--liq-pct X] [--analysis]
//!   cross_bench s2 --m M --k K [--reps R]
//!   cross_bench s3 --mode cross|isolated --k K [--ops N] [--reps R] [--block-ops B]
//!   cross_bench blob
//!   cross_bench all [--quick] [--reps R] [--s3-reps R]
//!
//! Every command also takes `--seed S`. Times are µs of wall clock around the one call (release
//! profile); "block-end" is `InMemoryHost::end_block` (delta harvest, committed-map merge, the
//! resident-cache drop) plus the commitment fold, timed separately.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

use alloy_sol_types::{SolCall, SolError, SolEvent};
use context_interface::journaled_state::PerpBlob;
use perp_core::{compute_block_commitment, keys, TypedPerpStore};
use perp_engine::{
    interface::IPerpDex::{
        cancelOrderCall, liquidateCall, placeOrderCall, setLeverageCall, setMarginTypeCall,
        updateIndexPriceCall, Adl, CrossLiquidationEnded, CrossLiquidationStarted, Liquidation,
        MarkPriceUpdated, Trade,
    },
    margin_view, run_perp_dex_call, storage,
    types::{MarginTiers, Market},
    InMemoryHost, PerpError, PerpHost, PerpOutput,
};
use primitives::{address, Address, FixedBytes, Log, B256, U256};

/// `writeln!` into a report buffer (sections printed after a table, so they never split it).
macro_rules! out {
    ($buf:expr, $($arg:tt)*) => {{
        use std::fmt::Write as _;
        writeln!($buf, $($arg)*).expect("writing to a String");
    }};
}

// ── Fixture constants ─────────────────────────────────────────────────────────

const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
/// The ISOLATED counterparty of every population position.
const HOUSE: Address = address!("b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0");
/// The ISOLATED liquidity provider: the liquidation closes' bid (s1), the taker loop's book (s3).
const LP: Address = address!("c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0");
/// The s3 account.
const TRADER: Address = address!("d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0");

const GAS: u64 = 30_000_000;
const BUY: u8 = 0;
const SELL: u8 = 1;
const GTC: u8 = 0;
const IOC: u8 = 1;
const ISOLATED: u8 = 1;

/// $100.00 at 2 price decimals; base 0 decimals, so one contract at `P` is `P · 10^4` quote units.
const P: u64 = 10_000;
/// Contracts per population position.
const Q: u64 = 10;
/// One population leg's notional at `P`: `Q · P · 10^4 = 10^9` quote units ($1,000).
const LEG_NOTIONAL: i64 = (Q * P * 10_000) as i64;
/// The `noliq` tick (−5%) and the `liq` / ADL tick (−22%).
const NOLIQ_PX: u64 = P * 95 / 100;
const LIQ_PX: u64 = P * 78 / 100;
/// The setup's oracle tick (a multiple of the 15 s interval); the timed tick is one interval later,
/// and the block clock sits there, so every setup mark is fresh for the stale guard (Q10).
const T0: u64 = 1_000_005;
const T1: u64 = T0 + 15;
/// Wallets: a population account ($10M), the house and the LP ($100B).
const POP_WALLET: i64 = 10_000_000_000_000;
const DEEP_WALLET: i64 = 100_000_000_000_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Cross,
    Isolated,
}

impl Mode {
    fn parse(s: &str) -> Self {
        match s {
            "cross" => Self::Cross,
            "isolated" => Self::Isolated,
            _ => panic!("--mode must be cross or isolated, got {s}"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Cross => "cross",
            Self::Isolated => "isolated",
        }
    }
}

/// The market every scenario trades: the cross tests' grid, the band off (≥ 100%), funding off,
/// Binance's 125 bps clearance fee on the notional closed (Q3 / R7), the default 3x tier table
/// (`MM = N / 6`).
fn market(id: u64) -> Market {
    Market {
        market_id: id,
        base_decimals: 0,
        price_decimals: 2,
        tick_size: 1,
        step_size: 1,
        min_quantity: 1,
        max_quantity: 10_000_000,
        max_price: 100_000_000,
        price_update_interval: 15,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 125,
        price_band_bps: 10_000,
        mark_price: P,
        tiers: MarginTiers::default(),
        basis_mode: 0,
        deferred_work: 0,
        cumulative_funding_index: 0,
    }
}

fn user(i: usize) -> Address {
    let mut b = [0u8; 20];
    b[0] = 0x20;
    b[12..20].copy_from_slice(&(i as u64).to_be_bytes());
    Address::from(b)
}

/// splitmix64 — the deterministic stream behind every random choice.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ── Calls ─────────────────────────────────────────────────────────────────────

fn call<H: PerpHost>(h: &mut H, caller: Address, input: &[u8]) -> PerpOutput {
    run_perp_dex_call(input, GAS, caller, U256::ZERO, false, h)
        .expect("a perp call must not hard-fail")
}

fn reason(out: &PerpOutput) -> String {
    alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason).unwrap_or_default()
}

/// A setup call that must succeed; its logs are dropped.
fn ok(h: &mut InMemoryHost, caller: Address, input: &[u8]) -> PerpOutput {
    let out = call(h, caller, input);
    assert!(!out.reverted, "setup call by {caller} reverted: {}", reason(&out));
    h.logs.clear();
    out
}

fn order(m: u64, side: u8, price: u64, qty: u64, tif: u8) -> Vec<u8> {
    placeOrderCall {
        marketId: m,
        side,
        price,
        quantity: qty,
        orderType: 0,
        tif,
        clientOrderId: FixedBytes::default(),
        flags: 0,
    }
    .abi_encode()
}

fn cancel(order_id: [u8; 32], m: u64) -> Vec<u8> {
    cancelOrderCall { orderId: FixedBytes(order_id), marketId: m }.abi_encode()
}

fn tick(m: u64, index: u64, ts: u64) -> Vec<u8> {
    updateIndexPriceCall { marketId: m, indexPrice: index, timestamp: ts }.abi_encode()
}

fn set_type(h: &mut InMemoryHost, u: Address, m: u64, t: u8) {
    ok(h, u, &setMarginTypeCall { marketId: m, marginType: t }.abi_encode());
}

fn set_leverage(h: &mut InMemoryHost, u: Address, m: u64, leverage: u64) {
    ok(h, u, &setLeverageCall { marketId: m, leverage }.abi_encode());
}

/// Fixture: sets a wallet outright (no event, no custody bookkeeping — a bench, not a ledger).
fn set_wallet(h: &mut InMemoryHost, u: Address, w: i64) {
    storage::mutate_account(h, u, |a| a.perp_wallet_balance = w).unwrap();
}

fn count<E: SolEvent>(logs: &[Log]) -> usize {
    logs.iter().filter(|l| l.data.topics().first() == Some(&E::SIGNATURE_HASH)).count()
}

// ── World lifecycle ───────────────────────────────────────────────────────────

/// Admin, markets `1..=k`, a deep insurance fund, and the house and the LP — funded and ISOLATED
/// in every market.
fn base(k: u64) -> InMemoryHost {
    let mut h = InMemoryHost::new(T1);
    storage::save_admin(&mut h, ADMIN).unwrap();
    for m in 1..=k {
        storage::save_market(&mut h, &market(m)).unwrap();
    }
    storage::save_insurance_fund(&mut h, 1_000_000_000_000_000).unwrap();
    for a in [HOUSE, LP] {
        set_wallet(&mut h, a, DEEP_WALLET);
        for m in 1..=k {
            set_type(&mut h, a, m, ISOLATED);
        }
    }
    h
}

/// Ends a setup: every market's last traded price reset (a tick's mark is then exactly its index),
/// one oracle tick per market at `T0` (fresh marks for the stale guard), and the block harvested,
/// so the timed call starts cold.
fn seal(h: &mut InMemoryHost, k: u64) {
    for m in 1..=k {
        storage::save_last_traded_price(h, m, 0).unwrap();
        ok(h, ADMIN, &tick(m, P, T0));
    }
    h.end_block();
    h.logs.clear();
}

/// A fresh host over a sealed world's committed store (`end_block` left nothing else in it).
fn restore(snap: &InMemoryHost) -> InMemoryHost {
    let mut h = InMemoryHost::new(snap.now);
    h.committed = snap.committed.clone();
    h.balances = snap.balances.clone();
    h
}

/// One timed call from a block start, then its block end (`end_block` + the commitment fold),
/// timed separately.
struct Shot {
    us: f64,
    blk_us: f64,
    logs: Vec<Log>,
}

fn shot(h: &mut InMemoryHost, caller: Address, input: &[u8]) -> Shot {
    h.logs.clear();
    let t = Instant::now();
    let out = call(h, caller, input);
    let us = t.elapsed().as_nanos() as f64 / 1e3;
    assert!(!out.reverted, "timed call reverted: {}", reason(&out));
    let logs = std::mem::take(&mut h.logs);
    let t = Instant::now();
    let delta = h.end_block();
    let c = compute_block_commitment(U256::ZERO, &delta);
    let blk_us = t.elapsed().as_nanos() as f64 / 1e3;
    std::hint::black_box(c);
    Shot { us, blk_us, logs }
}

// ── Stats ─────────────────────────────────────────────────────────────────────

fn sorted(v: &[f64]) -> Vec<f64> {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    s
}

fn median(v: &[f64]) -> f64 {
    let s = sorted(v);
    let n = s.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// Nearest-rank p90.
fn p90(v: &[f64]) -> f64 {
    let s = sorted(v);
    if s.is_empty() {
        return 0.0;
    }
    let rank = ((s.len() as f64) * 0.9).ceil() as usize;
    s[rank.clamp(1, s.len()) - 1]
}

/// Per-op percentile over pooled nanosecond samples, in µs.
fn pct_ns(v: &mut [u64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 * q) as usize).min(v.len() - 1);
    v[i] as f64 / 1e3
}

// ── Load counting ─────────────────────────────────────────────────────────────

/// Counts the host-level (COLD) loads of a call by namespace. A typed blob's first touch in a
/// block is one `perp_load_arc` (plus a `perp_load` when it is absent from the committed store); a
/// byte-tier blob is one `perp_load` per read. Resident reads never reach the host.
struct Counting<'a> {
    inner: &'a mut InMemoryHost,
    names: &'a HashMap<B256, &'static str>,
    /// namespace → (`perp_load_arc` calls, `perp_load` calls).
    loads: BTreeMap<&'static str, (u64, u64)>,
}

impl Counting<'_> {
    fn note(&mut self, key: B256, arc: bool) {
        let ns = self.names.get(&key).copied().unwrap_or("other");
        let e = self.loads.entry(ns).or_default();
        if arc {
            e.0 += 1;
        } else {
            e.1 += 1;
        }
    }
}

impl PerpHost for Counting<'_> {
    type Checkpoint = usize;

    fn live(&mut self) -> &mut TypedPerpStore {
        self.inner.live()
    }

    fn perp_load(&mut self, key: B256) -> Result<Vec<u8>, PerpError> {
        self.note(key, false);
        self.inner.perp_load(key)
    }

    fn perp_store(&mut self, key: B256, value: Vec<u8>) {
        self.inner.perp_store(key, value)
    }

    fn perp_load_arc(&mut self, key: B256) -> Result<Option<Arc<PerpBlob>>, PerpError> {
        self.note(key, true);
        self.inner.perp_load_arc(key)
    }

    fn log(&mut self, log: Log) {
        self.inner.log(log)
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn external_balance(&mut self, token: Address, owner: Address) -> Result<U256, PerpError> {
        self.inner.external_balance(token, owner)
    }

    fn set_external_balance(
        &mut self,
        token: Address,
        owner: Address,
        value: U256,
    ) -> Result<(), PerpError> {
        self.inner.set_external_balance(token, owner, value)
    }

    fn checkpoint(&mut self) -> usize {
        self.inner.checkpoint()
    }

    fn checkpoint_commit(&mut self) {
        self.inner.checkpoint_commit()
    }

    fn checkpoint_revert(&mut self, cp: usize) {
        self.inner.checkpoint_revert(cp)
    }

    fn perp_write_count(&self) -> u64 {
        self.inner.perp_write_count()
    }

    fn call_depth(&self) -> usize {
        self.inner.call_depth()
    }
}

/// Key → namespace for every key the scenarios can touch.
fn namespaces(users: &[Address], k: u64) -> HashMap<B256, &'static str> {
    let mut names = HashMap::new();
    for &u in users.iter().chain([HOUSE, LP, ADMIN, TRADER].iter()) {
        names.insert(keys::account_key(u), "account");
        names.insert(keys::user_markets_key(u), "umkt");
        names.insert(keys::cross_legs_key(u), "cross_legs");
        for m in 1..=k {
            names.insert(keys::position_key(u, m), "position");
            names.insert(keys::user_buy_orders_key(u, m), "user_orders");
            names.insert(keys::user_sell_orders_key(u, m), "user_orders");
            names.insert(keys::user_algo_orders_key(u, m), "user_algo");
        }
    }
    type KeyFn = fn(u64) -> B256;
    let per_market: [(KeyFn, &'static str); 14] = [
        (keys::funding_state_key, "funding_state"),
        (keys::index_price_state_key, "index_price_state"),
        (keys::index_price_history_key, "index_price_history"),
        (keys::index_mode_state_key, "index_mode_state"),
        (keys::price_basis_window_key, "price_basis_window"),
        (keys::price_basis_ewma_key, "price_basis_ewma"),
        (keys::premium_accumulator_key, "premium_accumulator"),
        (keys::bid_prices_key, "bid_prices"),
        (keys::ask_prices_key, "ask_prices"),
        (keys::algo_up_prices_key, "algo_prices"),
        (keys::algo_down_prices_key, "algo_prices"),
        (keys::trail_meta_key, "trail_meta"),
        (keys::trade_count_key, "trade_count"),
        (keys::market_fee_total_key, "market_fee_total"),
    ];
    for m in 1..=k {
        names.insert(keys::market_key(m), "market");
        names.insert(keys::market_hot_key(m), "market_hot");
        names.insert(keys::position_registry_key(m), "registry");
        for (f, name) in per_market {
            names.insert(f(m), name);
        }
    }
    for key in [
        keys::admin_key(),
        keys::oracle_key(),
        keys::market_manager_key(),
        keys::insurance_fund_key(),
        keys::max_maker_rebate_key(),
    ] {
        names.insert(key, "global");
    }
    names
}

fn print_counts(
    out: &mut String,
    label: &str, loads: &BTreeMap<&'static str, (u64, u64)>, per: usize) {
    out!(out, "\ncold loads of one {label} (per candidate = / {per}):");
    out!(out, "| namespace | perp_load_arc | perp_load | arc / cand | bytes / cand |");
    out!(out, "|---|---:|---:|---:|---:|");
    for (ns, (arc, bytes)) in loads {
        out!(out, 
            "| {ns} | {arc} | {bytes} | {:.2} | {:.2} |",
            *arc as f64 / per.max(1) as f64,
            *bytes as f64 / per.max(1) as f64
        );
    }
}

// ── s1: sweep ─────────────────────────────────────────────────────────────────

struct S1World {
    snap: InMemoryHost,
    users: Vec<Address>,
    thin: usize,
}

fn build_s1(mode: Mode, n: usize, k: u64, liq_pct: f64, seed: u64) -> S1World {
    let mut h = base(k);
    if n > 0 {
        for m in 1..=k {
            ok(&mut h, HOUSE, &order(m, SELL, P, n as u64 * Q, GTC));
        }
    }
    let n_thin = (((n as f64) * liq_pct / 100.0).round() as usize).min(n);
    let mut is_thin = vec![false; n];
    let mut rng = Rng(seed);
    let mut picked = 0;
    while picked < n_thin {
        let i = rng.below(n as u64) as usize;
        if !is_thin[i] {
            is_thin[i] = true;
            picked += 1;
        }
    }
    let users: Vec<Address> = (0..n).map(user).collect();
    for (i, &u) in users.iter().enumerate() {
        set_wallet(&mut h, u, POP_WALLET);
        if mode == Mode::Isolated {
            for m in 1..=k {
                set_type(&mut h, u, m, ISOLATED);
            }
            if is_thin[i] {
                // 3x in market 1: healthy at −5%, below maintenance at −22%.
                set_leverage(&mut h, u, 1, 3);
            }
        }
        for m in 1..=k {
            ok(&mut h, u, &order(m, BUY, P, Q, IOC));
        }
    }
    for &u in &users {
        for m in 1..=k {
            assert_eq!(storage::load_position_ref(&mut h, u, m).unwrap().amount, Q as i64);
        }
    }
    if mode == Mode::Cross {
        // Thin: `E = Σ MM + 10%` of one leg — healthy at −5%, below maintenance at −22%, and back
        // above it once market 1 alone is closed (one episode step each).
        for (i, &u) in users.iter().enumerate() {
            if is_thin[i] {
                let mm = margin_view::cross_fold(&mut h, u, None, None, None).unwrap().cross_mm;
                set_wallet(&mut h, u, (mm + LEG_NOTIONAL as i128 / 10) as i64);
            }
        }
    }
    if n_thin > 0 {
        // The closes' bid, at the `liq` tick's mark.
        ok(&mut h, LP, &order(1, BUY, LIQ_PX, n_thin as u64 * Q, GTC));
    }
    seal(&mut h, k);
    S1World { snap: h, users, thin: n_thin }
}

fn s1_header() {
    println!(
        "| scenario | mode | N | k | tick | reps | median µs | p90 µs | µs/cand (median) | \
         block-end µs (median) | Liquidation / CrossLiqStarted / Adl |"
    );
    println!("|---|---|---:|---:|---|---:|---:|---:|---:|---:|---|");
}

#[allow(clippy::too_many_arguments)]
fn run_s1(
    mode: Mode,
    n: usize,
    k: u64,
    reps: usize,
    liq_pct: f64,
    seed: u64,
    analysis: bool,
    notes: &mut String,
) {
    let t = Instant::now();
    let w = build_s1(mode, n, k, liq_pct, seed);
    eprintln!(
        "s1 {} N={n} k={k}: built in {:.1} s ({} committed keys, {} thin)",
        mode.name(),
        t.elapsed().as_secs_f64(),
        w.snap.committed.len(),
        w.thin
    );
    let mut noliq_per_cand = None;
    for (variant, px) in [("noliq", NOLIQ_PX), ("liq", LIQ_PX)] {
        if variant == "liq" && w.thin == 0 {
            continue;
        }
        let (mut us, mut blk) = (Vec::new(), Vec::new());
        let mut ev = (0, 0, 0);
        for _ in 0..reps {
            let mut h = restore(&w.snap);
            let s = shot(&mut h, ADMIN, &tick(1, px, T1));
            assert_eq!(count::<MarkPriceUpdated>(&s.logs), 1, "the tick did not run");
            us.push(s.us);
            blk.push(s.blk_us);
            ev = (
                count::<Liquidation>(&s.logs),
                count::<CrossLiquidationStarted>(&s.logs),
                count::<Adl>(&s.logs),
            );
        }
        if variant == "noliq" {
            assert_eq!(ev.0, 0, "the detection tick liquidated someone");
            noliq_per_cand = Some(median(&us) / (n.max(1) as f64));
        } else {
            assert!(ev.0 >= 1, "the liquidation tick liquidated nobody");
        }
        let per_cand =
            if n == 0 { "—".to_string() } else { format!("{:.3}", median(&us) / n as f64) };
        println!(
            "| s1 sweep | {} | {n} | {k} | {variant} | {reps} | {:.1} | {:.1} | {per_cand} | \
             {:.1} | {} / {} / {} |",
            mode.name(),
            median(&us),
            p90(&us),
            median(&blk),
            ev.0,
            ev.1,
            ev.2
        );
    }
    if analysis && n > 0 {
        let mut h = restore(&w.snap);
        let names = namespaces(&w.users, k);
        let mut c = Counting { inner: &mut h, names: &names, loads: BTreeMap::new() };
        let out = call(&mut c, ADMIN, &tick(1, NOLIQ_PX, T1));
        assert!(!out.reverted);
        let loads = std::mem::take(&mut c.loads);
        let label = format!("s1 {} N={n} k={k} noliq tick", mode.name());
        print_counts(notes, &label, &loads, n);
    }
    if analysis && n > 0 && mode == Mode::Cross {
        s1_decompose(notes, &w, k, reps, noliq_per_cand);
    }
}

/// One cross candidate's sweep cost, split. Each pass walks every population account on a FRESH
/// restore (cold, as the tick sees them) after warming the `k` market blobs, which the tick reads
/// once per block; the time is per account. `fold cold` is the whole `cross_fold` the detection
/// runs; `fold warm` the same fold with every blob resident (the compute plus resident lookups).
fn s1_decompose(
    out: &mut String,
    w: &S1World,
    k: u64,
    reps: usize,
    tick_per_cand: Option<f64>,
) {
    let n = w.users.len().max(1) as f64;
    type Pass<'a> = (&'a str, Box<dyn Fn(&mut InMemoryHost, Address)>);
    let passes: Vec<Pass<'_>> = vec![
        (
            "position(m1) — the sweep's own read",
            Box::new(|h, u| {
                std::hint::black_box(storage::load_position_ref(h, u, 1).unwrap());
            }),
        ),
        (
            "account",
            Box::new(|h, u| {
                std::hint::black_box(storage::load_account_ref(h, u).unwrap());
            }),
        ),
        (
            "user-market index (umkt)",
            Box::new(|h, u| {
                std::hint::black_box(storage::load_user_markets_ref(h, u).unwrap());
            }),
        ),
        (
            "positions m2..=k (k−1 cold loads)",
            Box::new(move |h, u| {
                for m in 2..=k {
                    std::hint::black_box(storage::load_position_ref(h, u, m).unwrap());
                }
            }),
        ),
        (
            "markets ×k (resident hits)",
            Box::new(move |h, _| {
                for m in 1..=k {
                    std::hint::black_box(storage::load_market_ref(h, m).unwrap());
                }
            }),
        ),
        (
            "cross_fold, cold (account + umkt + k positions + compute)",
            Box::new(|h, u| {
                std::hint::black_box(margin_view::cross_fold(h, u, None, None, None).unwrap());
            }),
        ),
        (
            "cross legs blob (P4, cold)",
            Box::new(|h, u| {
                std::hint::black_box(storage::load_cross_legs_ref(h, u).unwrap());
            }),
        ),
        (
            "cross_health, cold (P4: account + cross legs + compute)",
            Box::new(|h, u| {
                std::hint::black_box(margin_view::cross_health(h, u, None).unwrap());
            }),
        ),
    ];
    let warm_up = |h: &mut InMemoryHost| {
        for m in 1..=k {
            storage::load_market_ref(h, m).unwrap();
        }
    };
    out!(out, 
        "\ns1 cross N={} k={k} — per-candidate decomposition (µs, median of {reps}):",
        w.users.len()
    );
    out!(out, "| component | µs / candidate |");
    out!(out, "|---|---:|");
    let mut results = Vec::new();
    for (name, f) in &passes {
        let mut v = Vec::new();
        for _ in 0..reps {
            let mut h = restore(&w.snap);
            warm_up(&mut h);
            let t = Instant::now();
            for &u in &w.users {
                f(&mut h, u);
            }
            v.push(t.elapsed().as_nanos() as f64 / 1e3 / n);
        }
        results.push(median(&v));
        out!(out, "| {name} | {:.3} |", median(&v));
    }
    // The fold with everything resident: pure compute + resident lookups.
    let mut v = Vec::new();
    for _ in 0..reps {
        let mut h = restore(&w.snap);
        warm_up(&mut h);
        for &u in &w.users {
            margin_view::cross_fold(&mut h, u, None, None, None).unwrap();
        }
        let t = Instant::now();
        for &u in &w.users {
            std::hint::black_box(margin_view::cross_fold(&mut h, u, None, None, None).unwrap());
        }
        v.push(t.elapsed().as_nanos() as f64 / 1e3 / n);
    }
    let warm = median(&v);
    out!(out, "| cross_fold, warm (compute + resident lookups) | {warm:.3} |");
    // The sweep's candidate: its own position read, then the cold fold.
    let mut v = Vec::new();
    for _ in 0..reps {
        let mut h = restore(&w.snap);
        warm_up(&mut h);
        let t = Instant::now();
        for &u in &w.users {
            std::hint::black_box(storage::load_position_ref(&mut h, u, 1).unwrap());
            std::hint::black_box(margin_view::cross_fold(&mut h, u, None, None, None).unwrap());
            std::hint::black_box(storage::load_account_ref(&mut h, u).unwrap());
        }
        v.push(t.elapsed().as_nanos() as f64 / 1e3 / n);
    }
    let detection = median(&v);
    out!(
        out,
        "| position(m1) + cold fold + account hit (the detection's reads) | {detection:.3} |"
    );
    // P4: the detection the sweep runs since the switch — `cross_health`, no walk.
    let mut v = Vec::new();
    for _ in 0..reps {
        let mut h = restore(&w.snap);
        warm_up(&mut h);
        for &u in &w.users {
            margin_view::cross_health(&mut h, u, None).unwrap();
        }
        let t = Instant::now();
        for &u in &w.users {
            std::hint::black_box(margin_view::cross_health(&mut h, u, None).unwrap());
        }
        v.push(t.elapsed().as_nanos() as f64 / 1e3 / n);
    }
    let health_warm = median(&v);
    out!(out, "| cross_health, warm (compute + resident lookups) | {health_warm:.3} |");
    let mut v = Vec::new();
    for _ in 0..reps {
        let mut h = restore(&w.snap);
        warm_up(&mut h);
        let t = Instant::now();
        for &u in &w.users {
            std::hint::black_box(storage::load_position_ref(&mut h, u, 1).unwrap());
            std::hint::black_box(margin_view::cross_health(&mut h, u, None).unwrap());
            std::hint::black_box(storage::load_account_ref(&mut h, u).unwrap());
        }
        v.push(t.elapsed().as_nanos() as f64 / 1e3 / n);
    }
    let p4 = median(&v);
    out!(out, "| position(m1) + cold cross_health + account hit (P4 detection reads) | {p4:.3} |");
    let (pos1, acct, umkt, posk, mkts, fold) =
        (results[0], results[1], results[2], results[3], results[4], results[5]);
    let walk = umkt + posk;
    let rest = (detection - pos1 - acct - mkts - walk).max(0.0);
    let pc = |x: f64| 100.0 * x / detection;
    out!(out, 
        "\ndetection reads {detection:.3} µs/candidate = position(m1) {pos1:.3} ({:.0}%) + account \
         {acct:.3} ({:.0}%) + market hits {mkts:.3} ({:.0}%) + THE WALK [umkt + positions m2..k] \
         {walk:.3} ({:.0}%) + fold compute / bookkeeping {rest:.3} ({:.0}%); cold fold alone \
         {fold:.3}, warm fold {warm:.3}",
        pc(pos1),
        pc(acct),
        pc(mkts),
        pc(walk),
        pc(rest),
    );
    if let Some(t) = tick_per_cand {
        out!(out, 
            "noliq tick {t:.3} µs/candidate → the sweep's own overhead beyond these reads \
             (checkpoint, margin-type test, the episode's flag read, registry decode amortized) \
             {:.3} µs",
            t - detection
        );
    }
    let (legs, health) = (results[6], results[7]);
    out!(
        out,
        "P4 detection reads (position(m1) + cold cross_health + account hit) {p4:.3} µs/candidate \
         = position(m1) {pos1:.3} + cross legs {legs:.3} + the rest of cross_health (account, \
         market hits, compute) {:.3}; cold cross_health alone {health:.3}, warm {health_warm:.3}",
        (p4 - pos1 - legs).max(0.0)
    );
    if let Some(t) = tick_per_cand {
        out!(out, "noliq tick {t:.3} µs/candidate → beyond the P4 reads {:.3} µs", t - p4);
    }
}

// ── s2: ADL ───────────────────────────────────────────────────────────────────

fn build_s2(m_w: usize, k: u64, seed: u64) -> (InMemoryHost, Address) {
    let mut h = base(k);
    let loser = user(0);
    let winners: Vec<Address> = (1..=m_w).map(user).collect();
    let mut rng = Rng(seed);
    set_wallet(&mut h, loser, POP_WALLET);
    ok(&mut h, loser, &order(1, BUY, P, m_w as u64 * Q, GTC));
    for m in 2..=k {
        ok(&mut h, HOUSE, &order(m, SELL, P, m_w as u64 * Q, GTC));
    }
    for &w in &winners {
        // $20k + up to $200k: the ADL ranking (uPnL / E) is not one tie.
        set_wallet(&mut h, w, 20_000_000_000 + rng.below(200_000_000_000) as i64);
        ok(&mut h, w, &order(1, SELL, P, Q, IOC));
        for m in 2..=k {
            ok(&mut h, w, &order(m, BUY, P, Q, IOC));
        }
    }
    let loser_amount = storage::load_position_ref(&mut h, loser, 1).unwrap().amount;
    assert_eq!(loser_amount, (m_w as u64 * Q) as i64);
    // 20% of its notional: healthy at the setup mark (`MM = N / 6`), `E = −2%` of it at −22%.
    set_wallet(&mut h, loser, m_w as i64 * LEG_NOTIONAL / 5);
    seal(&mut h, k);
    (h, loser)
}

fn s2_header() {
    println!(
        "| scenario | M | k | call | reps | median µs | p90 µs | µs/winner (median) | \
         block-end µs (median) | Liquidation / Adl / CrossLiqEnded |"
    );
    println!("|---|---:|---:|---|---:|---:|---:|---:|---:|---|");
}

fn run_s2(m_w: usize, k: u64, reps: usize, seed: u64) {
    let t = Instant::now();
    let (snap, loser) = build_s2(m_w, k, seed);
    eprintln!(
        "s2 M={m_w} k={k}: built in {:.1} s ({} committed keys)",
        t.elapsed().as_secs_f64(),
        snap.committed.len()
    );
    for variant in ["tick", "manual"] {
        let (mut us, mut blk) = (Vec::new(), Vec::new());
        let mut ev = (0, 0, 0);
        for _ in 0..reps {
            let mut h = restore(&snap);
            let s = if variant == "tick" {
                shot(&mut h, ADMIN, &tick(1, LIQ_PX, T1))
            } else {
                // The mark moved without a tick (no sweep), then a keeper's `liquidate()`.
                storage::save_mark_price(&mut h, 1, LIQ_PX).unwrap();
                h.end_block();
                shot(&mut h, ADMIN, &liquidateCall { user: loser, marketId: 1 }.abi_encode())
            };
            us.push(s.us);
            blk.push(s.blk_us);
            ev = (
                count::<Liquidation>(&s.logs),
                count::<Adl>(&s.logs),
                count::<CrossLiquidationEnded>(&s.logs),
            );
        }
        assert!(ev.0 >= 1 && ev.1 >= 1, "s2 {variant}: no liquidation / ADL ({ev:?})");
        println!(
            "| s2 ADL | {m_w} | {k} | {variant} | {reps} | {:.1} | {:.1} | {:.3} | {:.1} | \
             {} / {} / {} |",
            median(&us),
            p90(&us),
            median(&us) / m_w.max(1) as f64,
            median(&blk),
            ev.0,
            ev.1,
            ev.2
        );
    }
}

// ── s3: order path ────────────────────────────────────────────────────────────

fn build_s3(mode: Mode, k: u64, ops: usize) -> InMemoryHost {
    let mut h = base(k);
    set_wallet(&mut h, TRADER, POP_WALLET);
    for m in 1..=k {
        if mode == Mode::Isolated {
            set_type(&mut h, TRADER, m, ISOLATED);
        }
        ok(&mut h, HOUSE, &order(m, SELL, P, Q, GTC));
        ok(&mut h, TRADER, &order(m, BUY, P, Q, IOC));
        assert_eq!(storage::load_position_ref(&mut h, TRADER, m).unwrap().amount, Q as i64);
    }
    // The taker loop's book: one tick either side of `P`, deep enough for every fill.
    let depth = ops as u64 + 10;
    ok(&mut h, LP, &order(1, BUY, P - 1, depth, GTC));
    ok(&mut h, LP, &order(1, SELL, P + 1, depth, GTC));
    seal(&mut h, k);
    h
}

/// Stored blob sizes of `u` in a sealed world: account, position(m1), user-market index, cross
/// legs (cross P4; 0 = no key).
fn blob_sizes(snap: &InMemoryHost, u: Address) -> (usize, usize, usize, usize) {
    let len = |key: B256| snap.committed.get(&key).map(|(b, _)| b.len()).unwrap_or(0);
    (
        len(keys::account_key(u)),
        len(keys::position_key(u, 1)),
        len(keys::user_markets_key(u)),
        len(keys::cross_legs_key(u)),
    )
}

/// Block-boundary bookkeeping of an s3 loop.
#[derive(Default)]
struct Blocks {
    every: usize,
    calls: usize,
    /// Boundary time: per op kind with single-call blocks (each boundary harvests exactly that
    /// call), under `"*"` otherwise. Op kind → (calls, ns).
    ns: BTreeMap<&'static str, (u64, u64)>,
    /// Per op kind (single-call blocks only).
    delta: DeltaStats,
}

/// Op kind → (calls, delta keys, delta bytes, calls whose delta wrote the account, calls whose
/// delta wrote the cross legs).
type DeltaStats = BTreeMap<&'static str, (u64, u64, u64, u64, u64)>;

impl Blocks {
    fn after(&mut self, h: &mut InMemoryHost, kind: &'static str) {
        self.calls += 1;
        let slot = if self.every == 1 { kind } else { "*" };
        self.ns.entry(slot).or_default().0 += 1;
        if self.every == 0 || !self.calls.is_multiple_of(self.every) {
            return;
        }
        let t = Instant::now();
        let delta = h.end_block();
        let c = compute_block_commitment(U256::ZERO, &delta);
        self.ns.entry(slot).or_default().1 += t.elapsed().as_nanos() as u64;
        std::hint::black_box(c);
        if self.every == 1 {
            let e = self.delta.entry(kind).or_default();
            e.0 += 1;
            e.1 += delta.len() as u64;
            e.2 += delta.values().map(|d| d.bytes.len() as u64).sum::<u64>();
            e.3 += delta.contains_key(&keys::account_key(TRADER)) as u64;
            e.4 += delta.contains_key(&keys::cross_legs_key(TRADER)) as u64;
        }
    }

    /// Mean boundary µs per call of `kind`.
    fn per_call(&self, kind: &'static str) -> f64 {
        let slot = if self.every == 1 { kind } else { "*" };
        let (calls, ns) = self.ns.get(slot).copied().unwrap_or_default();
        ns as f64 / 1e3 / calls.max(1) as f64
    }
}

fn timed_call(h: &mut InMemoryHost, input: &[u8], trades: usize) -> (u64, PerpOutput) {
    let t = Instant::now();
    let out = call(h, TRADER, input);
    let ns = t.elapsed().as_nanos() as u64;
    assert!(!out.reverted, "s3 call reverted: {}", reason(&out));
    assert_eq!(count::<Trade>(&h.logs), trades, "s3: unexpected fill count");
    h.logs.clear();
    (ns, out)
}

fn s3_header() {
    println!(
        "| scenario | mode | k | op | ops/rep | reps | median µs/op | p90 µs/op | pooled p50 µs | \
         pooled p90 µs | block-end µs/call (median) | engine + block-end µs |"
    );
    println!("|---|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|");
}

fn run_s3(mode: Mode, k: u64, ops: usize, reps: usize, block_ops: usize, notes: &mut String) {
    let t = Instant::now();
    let snap = build_s3(mode, k, ops);
    let (acct, pos, umkt, legs) = blob_sizes(&snap, TRADER);
    eprintln!(
        "s3 {} k={k}: built in {:.1} s; blobs: account {acct} B, position(m1) {pos} B, \
         umkt {umkt} B, cross legs {legs} B",
        mode.name(),
        t.elapsed().as_secs_f64()
    );
    let mut deltas: Vec<(&'static str, DeltaStats)> = Vec::new();
    for lp in ["place/cancel", "taker"] {
        // op kind → (per-rep mean µs, pooled ns)
        let mut means: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
        let mut pooled: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
        let mut blk: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
        for rep in 0..reps {
            let mut h = restore(&snap);
            let mut b = Blocks { every: block_ops, ..Blocks::default() };
            let mut samples: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
            if lp == "place/cancel" {
                for _ in 0..ops {
                    let (ns, out) = timed_call(&mut h, &order(1, BUY, P - 2, 1, GTC), 0);
                    samples.entry("place").or_default().push(ns);
                    b.after(&mut h, "place");
                    let oid: [u8; 32] = out.bytes[..32].try_into().unwrap();
                    let (ns, _) = timed_call(&mut h, &cancel(oid, 1), 0);
                    samples.entry("cancel").or_default().push(ns);
                    b.after(&mut h, "cancel");
                }
            } else {
                for i in 0..ops {
                    let (kind, input) = if i % 2 == 0 {
                        ("taker buy (opening)", order(1, BUY, P + 1, 1, IOC))
                    } else {
                        ("taker sell (reducing)", order(1, SELL, P - 1, 1, IOC))
                    };
                    let (ns, _) = timed_call(&mut h, &input, 1);
                    samples.entry(kind).or_default().push(ns);
                    b.after(&mut h, kind);
                }
            }
            for (kind, v) in samples {
                let mean = v.iter().sum::<u64>() as f64 / v.len().max(1) as f64 / 1e3;
                means.entry(kind).or_default().push(mean);
                pooled.entry(kind).or_default().extend(v);
                blk.entry(kind).or_default().push(b.per_call(kind));
            }
            if rep == 0 && block_ops == 1 {
                deltas.push((lp, std::mem::take(&mut b.delta)));
            }
        }
        for (kind, m) in &means {
            let v = pooled.get_mut(kind).unwrap();
            let blk = median(&blk[kind]);
            println!(
                "| s3 order path | {} | {k} | {kind} | {ops} | {reps} | {:.3} | {:.3} | {:.3} | \
                 {:.3} | {blk:.3} | {:.3} |",
                mode.name(),
                median(m),
                p90(m),
                pct_ns(v, 0.50),
                pct_ns(v, 0.90),
                median(m) + blk
            );
        }
    }
    for (lp, d) in deltas {
        for (kind, (calls, keys_n, bytes, acct_w, legs_w)) in d {
            out!(
                notes,
                "s3 {} k={k} {lp}: {kind}: delta {:.1} keys / {:.0} B per call, account written in \
                 {:.0}% of calls (account blob {acct} B), cross legs in {:.0}% ({legs} B)",
                mode.name(),
                keys_n as f64 / calls as f64,
                bytes as f64 / calls as f64,
                100.0 * acct_w as f64 / calls as f64,
                100.0 * legs_w as f64 / calls as f64
            );
        }
    }
}

fn run_blob() {
    println!(
        "| cross positions k | account blob B | position(m1) blob B | umkt blob B | cross legs \
         blob B |"
    );
    println!("|---:|---:|---:|---:|---:|");
    for k in [1u64, 4, 16] {
        let snap = build_s3(Mode::Cross, k, 0);
        let (a, p, u, l) = blob_sizes(&snap, TRADER);
        println!("| {k} | {a} | {p} | {u} | {l} |");
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |flag: &str| -> Option<String> {
        args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
    };
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let num = |flag: &str, default: usize| -> usize {
        get(flag).map(|v| v.parse().expect("a number")).unwrap_or(default)
    };
    let mode = || Mode::parse(&get("--mode").unwrap_or_else(|| "cross".into()));
    let seed = num("--seed", 42) as u64;
    let mut notes = String::new();
    match args.get(1).map(String::as_str).unwrap_or("") {
        "s1" => {
            s1_header();
            let liq_pct = get("--liq-pct").map(|v| v.parse().expect("a number")).unwrap_or(1.0);
            let (n, k, reps) = (num("--n", 1000), num("--k", 1) as u64, num("--reps", 7));
            run_s1(mode(), n, k, reps, liq_pct, seed, has("--analysis"), &mut notes);
        }
        "s2" => {
            s2_header();
            run_s2(num("--m", 100), num("--k", 4) as u64, num("--reps", 7), seed);
        }
        "s3" => {
            s3_header();
            let (k, ops, reps) = (num("--k", 1) as u64, num("--ops", 100_000), num("--reps", 5));
            run_s3(mode(), k, ops, reps, num("--block-ops", 1), &mut notes);
        }
        "blob" => run_blob(),
        "all" => {
            let quick = has("--quick");
            let (ns, ms, ops): (&[usize], &[usize], usize) =
                if quick { (&[100], &[20], 2_000) } else { (&[1000, 5000], &[100, 1000], 100_000) };
            let tick_reps = num("--reps", 7);
            let s3_reps = num("--s3-reps", 5);
            println!("## blob sizes\n");
            run_blob();
            println!("\n## s1 sweep\n");
            s1_header();
            run_s1(Mode::Cross, 0, 1, tick_reps, 0.0, seed, false, &mut notes);
            for &n in ns {
                for k in [1u64, 4, 16] {
                    for mode in [Mode::Cross, Mode::Isolated] {
                        run_s1(mode, n, k, tick_reps, 1.0, seed, true, &mut notes);
                    }
                }
            }
            println!("\n## s1 analysis\n{}", std::mem::take(&mut notes));
            println!("\n## s2 ADL\n");
            s2_header();
            for &m in ms {
                for k in [4u64, 16] {
                    run_s2(m, k, tick_reps, seed);
                }
            }
            println!("\n## s3 order path\n");
            s3_header();
            for k in [1u64, 4, 16] {
                for mode in [Mode::Cross, Mode::Isolated] {
                    run_s3(mode, k, ops, s3_reps, 1, &mut notes);
                }
            }
            println!("\n## s3 write sets (single-call blocks, first repetition)\n");
        }
        _ => {
            eprintln!("usage: cross_bench s1|s2|s3|blob|all [...] — see src/bin/cross_bench.rs");
            std::process::exit(2);
        }
    }
    print!("{notes}");
}
