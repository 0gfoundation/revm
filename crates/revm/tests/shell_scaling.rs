//! MANUAL perf probe (L2-2 premise retest, not a CI test): does the pure EVM transaction SHELL
//! (validation + nonce + balance + journal + State commit) parallelize across DISTINCT senders?
//!
//! Context: the old "EOA parallel ceiling ~1.2-1.4x" was measured when PerpDEX matching still ran
//! INLINE inside each tx (a serial precompile dominated every tx, so parallelism couldn't help).
//! With matching moved to the pre-phase, a perp replay tx's remaining work is shell-only and
//! per-sender independent — this probe measures the ALGORITHMIC scaling of that shell on plain
//! value transfers (a slightly heavier proxy: 2 touched accounts/tx vs the replay tx's 1).
//!
//! Serial: one EVM, N transact_commit in a loop. Parallel: K threads, txs partitioned by sender,
//! each thread its own Context/Journal/CacheDB overlay over the SAME read-only funded base
//! (the L2-2 design shape: per-worker overlays merged at block end; merge cost not measured here).
//!
//! Caveats vs the real node: InMemoryDB (no mdbx page-fault/TLB term), system allocator (reth uses
//! jemalloc), no receipt/bloom build, gas_price=0 (no beneficiary write — the real design
//! aggregates fees at merge). This answers "is the shell parallelizable at all"; the node-level
//! win still needs the real implementation.
//!
//! Run: cargo test -p revm --release --test shell_scaling -- --ignored --nocapture

use revm::{
    context::TxEnv,
    database::{CacheDB, InMemoryDB},
    primitives::{Address, TxKind, KECCAK_EMPTY, U256},
    state::AccountInfo,
    Context, ExecuteCommitEvm, MainBuilder, MainContext,
};
use std::time::Instant;

const N: usize = 8192;

fn addr(tag: u8, i: usize) -> Address {
    let mut b = [0u8; 20];
    b[0] = tag;
    b[12..].copy_from_slice(&(i as u64).to_be_bytes());
    Address::from(b)
}

fn funded_base() -> InMemoryDB {
    let mut db = InMemoryDB::default();
    for i in 0..N {
        db.insert_account_info(
            addr(0xAA, i),
            AccountInfo {
                balance: U256::from(10).pow(U256::from(18)),
                nonce: 0,
                code_hash: KECCAK_EMPTY,
                code: None,
            },
        );
    }
    db
}

fn tx_for(i: usize) -> TxEnv {
    TxEnv::builder()
        .caller(addr(0xAA, i))
        .kind(TxKind::Call(addr(0xBB, i)))
        .value(U256::from(1))
        .gas_limit(50_000)
        .nonce(0)
        .build()
        .unwrap()
}

/// Runs txs [lo, hi) on a fresh EVM over an overlay of `base`; returns elapsed.
fn run_range(base: &InMemoryDB, lo: usize, hi: usize) -> std::time::Duration {
    let mut evm = Context::mainnet()
        .with_db(CacheDB::new(base))
        .build_mainnet();
    let t0 = Instant::now();
    for i in lo..hi {
        let r = evm.transact_commit(tx_for(i)).expect("transact");
        assert!(r.is_success(), "tx {i} failed: {r:?}");
    }
    t0.elapsed()
}

#[test]
#[ignore = "manual perf probe, run with --release --ignored --nocapture"]
fn shell_scaling_across_senders() {
    let base = funded_base();

    // Warm-up (JIT-less but page/alloc warm).
    run_range(&base, 0, 512);

    // Serial baseline.
    let serial = run_range(&base, 0, N);
    let serial_us = serial.as_micros() as f64 / N as f64;
    println!("serial: {N} txs in {serial:?} -> {serial_us:.2} µs/tx");

    for k in [2usize, 4, 8, 16] {
        let chunk = N / k;
        let t0 = Instant::now();
        std::thread::scope(|s| {
            let mut hs = Vec::new();
            for t in 0..k {
                let base = &base;
                hs.push(s.spawn(move || run_range(base, t * chunk, (t + 1) * chunk)));
            }
            for h in hs {
                h.join().unwrap();
            }
        });
        let wall = t0.elapsed();
        let speedup = serial.as_secs_f64() / wall.as_secs_f64();
        println!(
            "K={k:>2}: wall {wall:?} -> {:.2} µs/tx effective, speedup {speedup:.2}x (ideal {k}x)",
            wall.as_micros() as f64 / N as f64
        );
    }
    println!("(box: check `nproc`; HT beyond physical cores flattens the curve)");
}
