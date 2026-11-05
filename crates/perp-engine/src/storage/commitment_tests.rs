use super::*;
use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::hardfork::SpecId;

use context::ContextTr;

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

/// Mirror of `trading::tests::make_ctx`: build a journal-backed context and warm the
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

/// #16d: `compute_block_commitment` hashes the block NET delta once. Cross-checked against an
/// independent BTreeMap reference (sorted, v3 framing) — a different impl than production's
/// HashMap+sort, so agreement is a genuine check. Includes a deleted key (empty value, len 0).
/// `finalize_block_commitment` writes the result to the 0x1003 slot; an empty delta is a no-op.
#[test]
fn block_commitment_over_net_delta() {
    let (k1, b1) = (B256::with_last_byte(1), vec![0xAAu8]);
    let (k2, b2) = (B256::with_last_byte(2), vec![0xBBu8, 0xCC]);
    let (k3, b3) = (B256::with_last_byte(3), Vec::<u8>::new()); // deleted key, framed len 0
    let mut delta = PerpDelta::default();
    let e = |b: &Vec<u8>| context::journaled_state::PerpDeltaEntry {
        decoded: None,
        bytes: b.clone(),
    };
    delta.insert(k1, e(&b1));
    delta.insert(k2, e(&b2));
    delta.insert(k3, e(&b3));

    // Independent reference: BTreeMap (sorted), v3 framing.
    let mut sorted: std::collections::BTreeMap<B256, Vec<u8>> =
        std::collections::BTreeMap::new();
    sorted.insert(k1, b1);
    sorted.insert(k2, b2);
    sorted.insert(k3, b3);
    let mut p = Vec::new();
    p.extend_from_slice(&U256::ZERO.to_be_bytes::<32>());
    p.push(BLOCK_COMMITMENT_VERSION);
    for (k, v) in &sorted {
        p.extend_from_slice(k.as_slice());
        p.extend_from_slice(&(v.len() as u32).to_be_bytes());
        p.extend_from_slice(v);
    }
    let expected = U256::from_be_bytes(*blake3::hash(&p).as_bytes());
    assert_eq!(compute_block_commitment(U256::ZERO, &delta), expected);

    // finalize_block_commitment writes it to the on-trie slot.
    let mut ctx = new_test_ctx();
    finalize_block_commitment(&mut ctx, &delta).unwrap();
    assert_eq!(read_commitment(&mut ctx), expected);

    // Empty delta is a no-op (slot stays at genesis 0).
    let mut ctx2 = new_test_ctx();
    finalize_block_commitment(&mut ctx2, &PerpDelta::default()).unwrap();
    assert_eq!(read_commitment(&mut ctx2), U256::ZERO);
}

/// #16d end-to-end: `store_blob` writes accumulate in the overlay; the on-trie slot stays
/// untouched until the block-end finalize folds the net delta ONCE (mirroring the executor:
/// `take_perp_delta` → `finalize_block_commitment`). A key overwritten in-block contributes
/// only its final value.
#[test]
fn block_commitment_from_overlay_writes() {
    let mut ctx = new_test_ctx();
    let (k1, b1) = (B256::with_last_byte(0x11), vec![0x01u8, 0x02]);
    let (k2, b2) = (B256::with_last_byte(0x22), vec![0x03u8]);
    store_blob(&mut ctx, k1, &b1).unwrap();
    store_blob(&mut ctx, k2, &b2).unwrap();
    store_blob(&mut ctx, k1, &[0x09]).unwrap(); // overwrite k1; net delta keeps the last value
                                                // No per-call flush: the on-trie slot stays at genesis until block end.
    assert_eq!(
        read_commitment(&mut ctx),
        U256::ZERO,
        "slot must not move until block end"
    );

    let delta = ctx.journal_mut().take_perp_delta();
    let expected = compute_block_commitment(U256::ZERO, &delta);
    finalize_block_commitment(&mut ctx, &delta).unwrap();
    assert_eq!(read_commitment(&mut ctx), expected);
    assert_ne!(expected, U256::ZERO);
}

/// Signed-order replay guard (commit-only #23, decoupled from the order map): a marked
/// signature reads back seen; an unrelated one does not; and the time-bucket GC reclaims the
/// marker once the block clock is `RETENTION` buckets past the signature's own bucket.
#[test]
fn seen_signature_guard_and_bucket_gc() {
    let mut ctx = new_test_ctx();
    let sig_a = [0x11u8; 32];
    let sig_b = [0x22u8; 32];
    let ts = 1_000u64;

    assert!(!is_signature_seen(&mut ctx, &sig_a).unwrap());
    mark_signature_seen(&mut ctx, &sig_a, ts).unwrap();
    assert!(
        is_signature_seen(&mut ctx, &sig_a).unwrap(),
        "marked signature reads back seen"
    );
    assert!(
        !is_signature_seen(&mut ctx, &sig_b).unwrap(),
        "unrelated signature is not seen"
    );

    let bucket_a = ts / SEEN_BUCKET_WIDTH_SECS;
    // GC targeting a bucket BEFORE sig_a's must not touch it.
    gc_seen_buckets(
        &mut ctx,
        (bucket_a + SEEN_RETENTION_BUCKETS - 1) * SEEN_BUCKET_WIDTH_SECS,
    )
    .unwrap();
    assert!(
        is_signature_seen(&mut ctx, &sig_a).unwrap(),
        "marker survives until its bucket is RETENTION behind the clock"
    );
    // GC exactly at sig_a's bucket + RETENTION reclaims it.
    gc_seen_buckets(
        &mut ctx,
        (bucket_a + SEEN_RETENTION_BUCKETS) * SEEN_BUCKET_WIDTH_SECS,
    )
    .unwrap();
    assert!(
        !is_signature_seen(&mut ctx, &sig_a).unwrap(),
        "GC reclaimed the expired marker"
    );
}
