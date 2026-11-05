use super::*;
use crate::{
    interface::IPerpDex::{placeOrderSignedCall, updateIndexPriceCall},
    risk::run_update_index_price,
    types::ApiKey,
};
use std::time::{Duration, Instant};

/// Extra funding so margin never runs out (~1.15e18 quote units).
const BIG: u64 = 1 << 60;

fn report(label: &str, total: Duration, iters: u64) -> f64 {
    let ns = total.as_nanos() as f64 / iters as f64;
    println!("PERF {label}: {iters} iters, total {total:?}, {ns:.0} ns/op");
    ns
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
                flags: 0,
    }
    .abi_encode()
}

/// (a)+(d): GTC limit buy that rests (empty book), then cancelOrder.
/// The two legs are timed separately inside the same loop, so the book is
/// empty again at the start of every iteration (true steady state).
#[test]
#[ignore = "perf baseline; run with --release --ignored --nocapture"]
fn perf_place_rest_then_cancel() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, ALICE, BIG);

    let input = place_input(0, PRICE, QTY, 0, 0);
    let warmup = 2_000u64;
    let iters = 20_000u64;
    let mut t_place = Duration::ZERO;
    let mut t_cancel = Duration::ZERO;

    for i in 0..(warmup + iters) {
        let timed = i >= warmup;
        let t0 = Instant::now();
        let ret = run_place_order(&input, ALICE, &mut ctx).unwrap();
        if timed {
            t_place += t0.elapsed();
        }
        let id: [u8; 32] = ret[..32].try_into().unwrap();
        let cancel = cancelOrderCall {
            orderId: id.into(),
            marketId: MARKET_ID,
        }
        .abi_encode();
        let t1 = Instant::now();
        run_cancel_order(&cancel, ALICE, &mut ctx).unwrap();
        if timed {
            t_cancel += t1.elapsed();
        }
    }
    let p = report("placeOrder rest (limit GTC, no match)", t_place, iters);
    let c = report("cancelOrder (resting order)", t_cancel, iters);
    report("place+cancel pair avg", t_place + t_cancel, iters * 2);
    println!("PERF note: place {p:.0} ns + cancel {c:.0} ns per round-trip");
}

/// (b): taker fully filled by one resting maker. Maker (rest) and taker
/// (single fill, settles both sides) timed separately; the book is empty
/// again after every iteration.
#[test]
#[ignore = "perf baseline; run with --release --ignored --nocapture"]
fn perf_match_single_fill() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, ALICE, BIG);
    fund(&mut ctx, BOB, BIG);

    let maker = place_input(1, PRICE, QTY, 0, 0); // BOB sell rests
    let taker = place_input(0, PRICE, QTY, 0, 0); // ALICE buy fills
    let warmup = 1_000u64;
    let iters = 10_000u64;
    let mut t_maker = Duration::ZERO;
    let mut t_taker = Duration::ZERO;

    for i in 0..(warmup + iters) {
        let timed = i >= warmup;
        let t0 = Instant::now();
        run_place_order(&maker, BOB, &mut ctx).unwrap();
        if timed {
            t_maker += t0.elapsed();
        }
        let t1 = Instant::now();
        run_place_order(&taker, ALICE, &mut ctx).unwrap();
        if timed {
            t_taker += t1.elapsed();
        }
    }
    report("placeOrder maker rest (sell side)", t_maker, iters);
    report("placeOrder taker, 1 fill (direct)", t_taker, iters);
    report("maker+taker pair total", t_maker + t_taker, iters);
}

/// (c): one taker sweeping 10 maker price levels. The 10 maker placements
/// and the sweep are timed separately; per-extra-fill increment is derived
/// against the 1-fill taker from scenario (b) offline.
#[test]
#[ignore = "perf baseline; run with --release --ignored --nocapture"]
fn perf_taker_sweep_10_levels() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, ALICE, BIG);
    fund(&mut ctx, BOB, BIG);

    let makers: Vec<Vec<u8>> = (0..10u64)
        .map(|i| place_input(1, PRICE + i * TICK, QTY, 0, 0))
        .collect();
    let taker = place_input(0, PRICE + 9 * TICK, QTY * 10, 0, 0);
    let warmup = 200u64;
    let iters = 2_000u64;
    let mut t_makers = Duration::ZERO;
    let mut t_taker = Duration::ZERO;

    for i in 0..(warmup + iters) {
        let timed = i >= warmup;
        let t0 = Instant::now();
        for m in &makers {
            run_place_order(m, BOB, &mut ctx).unwrap();
        }
        if timed {
            t_makers += t0.elapsed();
        }
        let t1 = Instant::now();
        run_place_order(&taker, ALICE, &mut ctx).unwrap();
        if timed {
            t_taker += t1.elapsed();
        }
    }
    report("maker rest avg (10 distinct levels)", t_makers, iters * 10);
    report("placeOrder taker sweeping 10 levels", t_taker, iters);
    report(
        "full iteration (10 rests + 1 sweep)",
        t_makers + t_taker,
        iters,
    );
}

// ── Block-level benches ────────────────────────────────────────────────
//
// The perf_* benches above each run ONE op against a fresh/steady-state book, so the off-trie
// overlay never accumulates and the SAME blob is never re-touched across txns — which makes the
// serialization redundancy a per-block deser cache (#14) / block-end serialization (#16d) would
// remove INVISIBLE. These run a whole "block" of txns against ONE ctx (overlay accumulates, hot
// blobs are re-touched), and instrument the load_blob/store_blob choke (`bench_counter`) to
// report ser/deser call-counts vs DISTINCT keys — the exact redundancy #14/#16d collapse.
//
// Like the other perf_* benches these call the inner `run_*` handlers directly (not
// `run_perp_dex_call`), so the per-call commitment flush (BLAKE3) is excluded — consistent with
// the existing baseline; this bench is about ser/deser VOLUME, not the commitment hash.

fn report_block(
    label: &str,
    elapsed: Duration,
    calls: u64,
    st: &crate::storage::bench_counter::Stats,
) {
    // Post-#14 + #16d the typed msgpack helpers no longer hit the byte choke: reads go through
    // the struct overlay (perp_get_struct, no decode) and writes are deferred (perp_store_struct,
    // no per-write encode). So load_blob/store_blob below are the RESIDUAL byte-path traffic
    // (raw-packed level queues + cold reads), and msgpack serialization now happens once per key
    // at block end (block_end_ser). Compare against the pre-#16d committed bench, where
    // store_blob carried ~all writes and per-write serialization.
    println!(
        "PERF BLOCK {label}: {calls} handler calls, {elapsed:?} ({:.2} us/call)",
        elapsed.as_nanos() as f64 / 1000.0 / calls.max(1) as f64
    );
    println!(
        "  load_blob  (residual byte/cold reads): {} calls, {} KiB",
        st.read_calls,
        st.read_bytes / 1024
    );
    println!(
        "  store_blob (residual byte-path writes): {} calls, {} KiB",
        st.write_calls,
        st.write_bytes / 1024
    );
    println!(
        "  block-end serialize (#16d, once per key): {} calls, {} KiB",
        st.block_end_ser_calls,
        st.block_end_ser_bytes / 1024
    );
}

/// Resting-only block: a few makers post GTC limit buys across an 8-level band, round after
/// round, against ONE ctx. No asks ever exist, so every buy rests (no matches). Maximises reuse
/// of: market config (read every order), best_bid + bid_prices list, the 8 level queues, and
/// each maker's account/position/nonce. The growing level queues also show the re-serialization
/// cost (#14/#16d serialize each final queue once instead of once per push).
#[test]
#[ignore = "block-level perf; run with --release --ignored --nocapture"]
fn perf_block_rest_heavy() {
    use crate::storage::bench_counter as bc;
    const N_MAKERS: u64 = 4;
    const N_LEVELS: u64 = 8;
    const ROUNDS: u64 = 500; // N_MAKERS * ROUNDS = 2000 resting placements

    let mut ctx = make_ctx();
    setup(&mut ctx);
    let makers: Vec<Address> = (1..=N_MAKERS).map(user_addr).collect();
    for &m in &makers {
        JournalTr::load_account(ctx.journal_mut(), m).unwrap();
        fund(&mut ctx, m, BIG);
    }

    bc::reset();
    bc::enable();
    let t0 = Instant::now();
    let mut calls = 0u64;
    for r in 0..ROUNDS {
        for (mi, &m) in makers.iter().enumerate() {
            let lvl = (r + mi as u64) % N_LEVELS;
            let price = PRICE - (lvl + 1) * TICK; // strictly below PRICE; no asks => always rests
            bc::next_txn();
            let _ = place(&mut ctx, m, 0, price, QTY, 0, 0); // buy / limit / GTC
            calls += 1;
        }
    }
    // #16d block-end harvest: serialize each struct key ONCE (counted as block_end_ser).
    let _ = ctx.journal_mut().take_perp_delta();
    let elapsed = t0.elapsed();
    bc::disable();
    report_block(
        "rest-heavy (resting limits only)",
        elapsed,
        calls,
        &bc::snapshot(),
    );
}

/// Mixed block: resting bids + periodic IOC taker sweeps (cross the top 2 levels) + periodic
/// cancels of guaranteed-still-open deep bids. Exercises the rest, match, and cancel paths
/// against ONE ctx so maker accounts/positions, level queues and best_bid are re-touched by all
/// three paths.
#[test]
#[ignore = "block-level perf; run with --release --ignored --nocapture"]
fn perf_block_mixed() {
    use crate::storage::bench_counter as bc;
    const N_MAKERS: u64 = 4;
    const N_LEVELS: u64 = 8;
    const ROUNDS: u64 = 600;
    const SWEEP_EVERY: u64 = 20;

    let mut ctx = make_ctx();
    setup(&mut ctx);
    let makers: Vec<Address> = (1..=N_MAKERS).map(user_addr).collect();
    for &m in &makers {
        JournalTr::load_account(ctx.journal_mut(), m).unwrap();
        fund(&mut ctx, m, BIG);
    }
    let taker = ALICE;
    fund(&mut ctx, taker, BIG);

    // Deep bids (level >= 2, price <= PRICE-3*TICK) are never crossed by a sweep at PRICE-2*TICK,
    // so they stay open and are safe to cancel deterministically.
    let mut deep: Vec<([u8; 32], Address)> = Vec::new();
    bc::reset();
    bc::enable();
    let t0 = Instant::now();
    let mut calls = 0u64;
    for r in 0..ROUNDS {
        for (mi, &m) in makers.iter().enumerate() {
            let lvl = (r + mi as u64) % N_LEVELS;
            let price = PRICE - (lvl + 1) * TICK;
            bc::next_txn();
            let id = place(&mut ctx, m, 0, price, QTY, 0, 0);
            calls += 1;
            if lvl >= 2 {
                deep.push((id, m));
            }
        }
        if r % SWEEP_EVERY == SWEEP_EVERY - 1 {
            // IOC sell crossing only the top ~2 bid levels; unfilled remainder expires (no ask rests).
            bc::next_txn();
            let _ = place(&mut ctx, taker, 1, PRICE - 2 * TICK, QTY * 3, 0, 1); // sell / limit / IOC
            calls += 1;
        }
        if deep.len() > 32 {
            let (id, owner) = deep.remove(0);
            let cancel = cancelOrderCall {
                orderId: id.into(),
                marketId: MARKET_ID,
            }
            .abi_encode();
            bc::next_txn();
            run_cancel_order(&cancel, owner, &mut ctx).unwrap();
            calls += 1;
        }
    }
    // #16d block-end harvest: serialize each struct key ONCE (counted as block_end_ser).
    let _ = ctx.journal_mut().take_perp_delta();
    let elapsed = t0.elapsed();
    bc::disable();
    report_block(
        "mixed (rests + sweeps + cancels)",
        elapsed,
        calls,
        &bc::snapshot(),
    );
}

/// BBO-churn block: frequent place+cancel at exactly the best-bid and best-ask prices, NO matches,
/// only TWO price levels ever touched (the case the user asked to measure). Runs the IDENTICAL
/// workload twice against fresh ctxs — once with per-call ser/deser (`force_percall`, pre-#14/#16d)
/// and once deferred (#14 read cache + #16d block-end write serialization) — and reports the
/// execution-time SAVING (per-call wall-time − deferred wall-time).
///
/// What deferral collapses here: the per-user account / position / nonce / best-bid+ask blobs are
/// re-written on every place AND cancel (huge reuse → one ser/deser per key for the whole block);
/// each order entry is written ~twice (place + cancel) → 2→1. What it does NOT touch: the per-price
/// FIFO level queues are the raw-byte path (`store_blob`, not msgpack `save_cached`), so they
/// serialize per-op in BOTH passes and cancel out of the delta = the residual deferral can't remove
/// for this workload (catalog #21). The commitment is asserted identical across passes = byte-identity.
#[test]
#[ignore = "block-level perf; run with --release --ignored --nocapture"]
fn perf_block_bbo_churn() {
    use crate::storage::bench_counter as bc;
    const ROUNDS: u64 = 2000; // each round = place+cancel @ bid AND place+cancel @ ask (4 calls)

    let bid_px = PRICE - TICK; // best bid
    let ask_px = PRICE + TICK; // best ask (bid_px < ask_px => orders never cross => no matches)

    // One block of identical work; returns (wall-time, counter snapshot, block commitment).
    let run_block = |force_percall: bool| -> (Duration, bc::Stats, U256) {
        let mut ctx = make_ctx();
        // Set the mode BEFORE any write so the whole overlay is one representation (bytes vs
        // struct) — otherwise the first churn read of a setup-written key pays a serialize-on-read.
        bc::set_force_percall(force_percall);
        setup(&mut ctx);
        let m = user_addr(1);
        JournalTr::load_account(ctx.journal_mut(), m).unwrap();
        fund(&mut ctx, m, BIG);
        // Resting bid + ask define a stable BBO that the churn never crosses; they stay open.
        let _rest_bid = place(&mut ctx, m, 0, bid_px, QTY, 0, 0); // buy  GTC @ bid
        let _rest_ask = place(&mut ctx, m, 1, ask_px, QTY, 0, 0); // sell GTC @ ask (no bid >= ask => rests)

        bc::reset();
        bc::enable();
        let t0 = Instant::now();
        for _ in 0..ROUNDS {
            bc::next_txn();
            let b = place(&mut ctx, m, 0, bid_px, QTY, 0, 0); // rests at best bid (no ask <= bid)
            bc::next_txn();
            let cb = cancelOrderCall {
                orderId: b.into(),
                marketId: MARKET_ID,
            }
            .abi_encode();
            run_cancel_order(&cb, m, &mut ctx).unwrap();
            bc::next_txn();
            let a = place(&mut ctx, m, 1, ask_px, QTY, 0, 0); // rests at best ask (no bid >= ask)
            bc::next_txn();
            let ca = cancelOrderCall {
                orderId: a.into(),
                marketId: MARKET_ID,
            }
            .abi_encode();
            run_cancel_order(&ca, m, &mut ctx).unwrap();
        }
        // Block-end harvest INSIDE the timed region: deferred mode serializes each struct key once
        // here (block_end_ser); per-call mode drains already-serialized bytes (no ser).
        let delta = ctx.journal_mut().take_perp_delta();
        let elapsed = t0.elapsed();
        bc::disable();
        bc::set_force_percall(false);
        let commit = storage::compute_block_commitment(U256::ZERO, &delta);
        (elapsed, bc::snapshot(), commit)
    };

    let calls = ROUNDS * 4;
    let (t_off, s_off, c_off) = run_block(true);
    let (t_on, s_on, c_on) = run_block(false);

    // #16d invariant: deferral must not change the net delta → same on-trie commitment.
    assert_eq!(
        c_off, c_on,
        "force_percall changed the block commitment — deferral must be byte-identical"
    );

    let us = |d: Duration| d.as_nanos() as f64 / 1000.0 / calls as f64;
    println!(
        "PERF BLOCK bbo-churn (place+cancel @ best bid/ask, no match, 2 levels): {calls} calls / {ROUNDS} rounds"
    );
    println!(
        "  PER-CALL  ser/deser (pre-#14/#16d): {t_off:?} ({:.3} us/call)",
        us(t_off)
    );
    println!(
        "    reads {} calls / {} KiB | writes {} calls / {} KiB | block-end ser {} calls",
        s_off.read_calls,
        s_off.read_bytes / 1024,
        s_off.write_calls,
        s_off.write_bytes / 1024,
        s_off.block_end_ser_calls
    );
    println!(
        "  DEFERRED  ser/deser (#14+#16d):     {t_on:?} ({:.3} us/call)",
        us(t_on)
    );
    println!(
        "    residual byte reads {} calls / {} KiB | residual byte writes {} calls / {} KiB | block-end ser {} calls / {} KiB",
        s_on.read_calls, s_on.read_bytes / 1024, s_on.write_calls, s_on.write_bytes / 1024, s_on.block_end_ser_calls, s_on.block_end_ser_bytes / 1024
    );
    let saved = t_off.saturating_sub(t_on);
    let pct = saved.as_nanos() as f64 / (t_off.as_nanos().max(1)) as f64 * 100.0;
    println!("  SAVED by deferring ser+deser to block end: {saved:?} ({pct:.1}% of per-call execution time)");
}

/// (e): placeOrderSigned taker, single fill — ed25519 verify path.
/// Signatures are pre-generated OUTSIDE the timed loop so only the
/// precompile-side cost (decode, api-key load, recvWindow, verify_strict,
/// keccak(sig) orderId, duplicate check, matching) is measured. Compare the
/// taker leg against perf_match_single_fill's direct taker to isolate the
/// signed-path overhead.
#[test]
#[ignore = "perf baseline; run with --release --ignored --nocapture"]
fn perf_match_single_fill_signed() {
    use ed25519_dalek::{Signer, SigningKey};

    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, ALICE, BIG);
    fund(&mut ctx, BOB, BIG);

    let sk = SigningKey::from_bytes(&[7u8; 32]);
    storage::save_api_key(
        &mut ctx,
        ALICE,
        0,
        ApiKey {
            pubkey: sk.verifying_key().to_bytes(),
            expiry: 0,
        },
    )
    .unwrap();

    let block_ts: u64 = 1; // BlockEnv::default() timestamp
    let recv_window: u64 = 60;
    let warmup = 500u64;
    let iters = 5_000u64;

    // Pre-generate one uniquely-signed input per iteration (unique
    // clientOrderId => unique signature => unique orderId).
    let signed_inputs: Vec<Vec<u8>> = (0..(warmup + iters))
        .map(|i| {
            let mut client_id = [0u8; 16];
            client_id[8..].copy_from_slice(&i.to_be_bytes());
            // Canonical 96-byte message, layout from run_place_order_signed.
            let mut msg = [0u8; 105];
            let mut c = crate::trading::write_signed_header(&mut msg, b"perpdex_v1_order", TEST_CHAIN_ID);
            c = crate::trading::put(&mut msg, c, &ALICE.as_slice());
            c = crate::trading::put(&mut msg, c, &MARKET_ID.to_be_bytes());
            c = crate::trading::put(&mut msg, c, &[0]);
            c = crate::trading::put(&mut msg, c, &PRICE.to_be_bytes());
            c = crate::trading::put(&mut msg, c, &QTY.to_be_bytes());
            c = crate::trading::put(&mut msg, c, &[0]);
            c = crate::trading::put(&mut msg, c, &[0]);
            c = crate::trading::put(&mut msg, c, &client_id);
            c = crate::trading::put(&mut msg, c, &block_ts.to_be_bytes());
            c = crate::trading::put(&mut msg, c, &recv_window.to_be_bytes());
            c = crate::trading::put(&mut msg, c, &[0]);
            c = crate::trading::put(&mut msg, c, &[0]);
            c = crate::trading::put(&mut msg, c, &[0]);
            debug_assert_eq!(c, msg.len());
            let sig = sk.sign(&msg);
            placeOrderSignedCall {
                account: ALICE,
                marketId: MARKET_ID,
                side: 0,
                price: PRICE,
                quantity: QTY,
                orderType: 0,
                tif: 0,
                clientOrderId: FixedBytes(client_id),
                timestamp: block_ts,
                recvWindow: recv_window,
                keyId: 0,
                signature: sig.to_bytes().to_vec().into(),
                                flags: 0,
            }
            .abi_encode()
        })
        .collect();

    let maker = place_input(1, PRICE, QTY, 0, 0);
    let mut t_taker = Duration::ZERO;

    for (i, signed) in signed_inputs.iter().enumerate() {
        run_place_order(&maker, BOB, &mut ctx).unwrap(); // untimed maker rest
        let timed = (i as u64) >= warmup;
        let t0 = Instant::now();
        run_place_order_signed(signed, &mut ctx).unwrap();
        if timed {
            t_taker += t0.elapsed();
        }
    }
    report("placeOrderSigned taker, 1 fill (ed25519)", t_taker, iters);
}

/// (f): updateIndexPrice — oracle hot path. Timestamp advances by the
/// market's price_update_interval (15s) per call so every call does full
/// work (stale updates short-circuit). Test market has funding_interval=0,
/// so premium-index accumulation is skipped in the base variant; the
/// `_with_funding` variant enables it (production-like).
#[test]
#[ignore = "perf baseline; run with --release --ignored --nocapture"]
fn perf_update_index_price() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    run_update_index_price_loop(&mut ctx, "updateIndexPrice (funding_interval=0)");
}

#[test]
#[ignore = "perf baseline; run with --release --ignored --nocapture"]
fn perf_update_index_price_with_funding() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    // Same market but with funding enabled (premium-index accumulation
    // every update + funding-rate computation every 3600/15 = 240 updates).
    storage::save_market(
        &mut ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 8,
            price_decimals: 9,
            tick_size: TICK,
            step_size: QTY,
            min_quantity: QTY,
            max_quantity: QTY * 1_000,
            max_price: PRICE * 1_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 3_600,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 0,
            mark_price: 0,
            tiers: MarginTiers::default(),
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
        },
    )
    .unwrap();
    run_update_index_price_loop(
        &mut ctx,
        "updateIndexPrice (funding_interval=3600, premium accumulation)",
    );
}

fn run_update_index_price_loop(ctx: &mut TestCtx, label: &str) {
    let warmup = 1_000u64;
    let iters = 20_000u64;
    let mut ts = 15u64;
    let mut t = Duration::ZERO;

    for i in 0..(warmup + iters) {
        let input = updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: PRICE,
            timestamp: ts,
        }
        .abi_encode();
        ts += 15;
        let timed = i >= warmup;
        let t0 = Instant::now();
        run_update_index_price(&input, ADMIN, ctx).unwrap();
        if timed {
            t += t0.elapsed();
        }
    }
    report(label, t, iters);
}

/// Micro: raw keccak256 cost on this machine, by input size. Sizes map to
/// hot-path uses: 32 B ≈ key-derivation input, 71 B = 64+7 scalar-blob fold
/// preimage, 136 B = exactly one keccak rate block, 219 B = 64+155 Order
/// fold, 584 B = 64+520 PriceBasisWindow fold (largest hot fold), 4 KiB =
/// long-string reference for batched-fold designs.
#[test]
#[ignore = "perf measurement: cargo test --release perf_ -- --ignored --nocapture"]
fn perf_keccak256_by_size() {
    use std::hint::black_box;
    for &(size, iters) in &[
        (32usize, 2_000_000u64),
        (71, 2_000_000),
        (136, 2_000_000),
        (219, 1_000_000),
        (584, 1_000_000),
        (4096, 200_000),
    ] {
        let buf = vec![0xA5u8; size];
        for _ in 0..10_000 {
            black_box(primitives::keccak256(black_box(&buf[..])));
        }
        let t0 = Instant::now();
        for _ in 0..iters {
            black_box(primitives::keccak256(black_box(&buf[..])));
        }
        report(&format!("keccak256 over {size} B"), t0.elapsed(), iters);
    }
}

/// Micro: BLAKE3 vs keccak256 at the same sizes, to justify #24 (the commitment
/// hash swap). The commitment now hashes a per-call framed log (C_prev 32B +
/// 1 ver byte + Σ framed writes), so the relevant sizes are the larger ones.
#[test]
#[ignore = "perf measurement: cargo test --release perf_ -- --ignored --nocapture"]
fn perf_blake3_by_size() {
    use std::hint::black_box;
    for &(size, iters) in &[
        (32usize, 2_000_000u64),
        (71, 2_000_000),
        (136, 2_000_000),
        (219, 1_000_000),
        (584, 1_000_000),
        (4096, 200_000),
    ] {
        let buf = vec![0xA5u8; size];
        for _ in 0..10_000 {
            black_box(blake3::hash(black_box(&buf[..])));
        }
        let t0 = Instant::now();
        for _ in 0..iters {
            black_box(blake3::hash(black_box(&buf[..])));
        }
        report(&format!("blake3 over {size} B"), t0.elapsed(), iters);
    }
}
