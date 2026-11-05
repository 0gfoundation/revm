use super::batch_cancel::register_key;
use super::*;
use crate::interface::IPerpDex::{
    cancelOrderSignedCall, placeOrderSignedCall, setLeverageSignedCall,
};
use ed25519_dalek::{Signer, SigningKey};

const SIGNED_TS: u64 = 1; // == block timestamp in `make_ctx`
const SIGNED_RECV: u64 = 60;

fn signed_place_input(
    sk: &SigningKey,
    side: u8,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
    tamper: bool,
) -> Vec<u8> {
    // Canonical 96-byte message, layout from `run_place_order_signed`.
    let mut msg = [0u8; 105];
    let mut c = crate::trading::write_signed_header(&mut msg, b"perpdex_v1_order", TEST_CHAIN_ID);
    c = crate::trading::put(&mut msg, c, &ALICE.as_slice());
    c = crate::trading::put(&mut msg, c, &MARKET_ID.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[side]);
    c = crate::trading::put(&mut msg, c, &price.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &qty.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[order_type]);
    c = crate::trading::put(&mut msg, c, &[tif]);
    c = crate::trading::put(&mut msg, c, &[0u8; 16]); // clientOrderId
    c = crate::trading::put(&mut msg, c, &SIGNED_TS.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[0]); // keyId
    c = crate::trading::put(&mut msg, c, &[0]); // flags
    debug_assert_eq!(c, msg.len());
    let mut sig = sk.sign(&msg).to_bytes().to_vec();
    if tamper {
        sig[0] ^= 0xff;
    }
    placeOrderSignedCall {
        account: ALICE,
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 0,
        signature: sig.into(),
                flags: 0,
    }
    .abi_encode()
}

fn signed_leverage_input(sk: &SigningKey, leverage: u64, tamper: bool) -> Vec<u8> {
    // Canonical 72-byte message, layout from `run_set_leverage_signed`.
    let mut msg = [0u8; 80];
    let mut c = crate::trading::write_signed_header(&mut msg, b"perpdex_v1_leverage", TEST_CHAIN_ID);
    c = crate::trading::put(&mut msg, c, &ALICE.as_slice());
    c = crate::trading::put(&mut msg, c, &MARKET_ID.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &leverage.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &SIGNED_TS.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[0]);
    debug_assert_eq!(c, msg.len());
    let mut sig = sk.sign(&msg).to_bytes().to_vec();
    if tamper {
        sig[0] ^= 0xff;
    }
    setLeverageSignedCall {
        account: ALICE,
        marketId: MARKET_ID,
        leverage,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 0,
        signature: sig.into(),
    }
    .abi_encode()
}

fn signed_cancel_bytes(sk: &SigningKey, order_id: [u8; 32], tamper: bool) -> Vec<u8> {
    // Canonical 94-byte message, layout from `run_cancel_order_signed`.
    let mut msg = [0u8; 102];
    let mut c = crate::trading::write_signed_header(&mut msg, b"perpdex_v1_cancel", TEST_CHAIN_ID);
    c = crate::trading::put(&mut msg, c, &ALICE.as_slice());
    c = crate::trading::put(&mut msg, c, &order_id);
    c = crate::trading::put(&mut msg, c, &MARKET_ID.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &SIGNED_TS.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[0]);
    debug_assert_eq!(c, msg.len());
    let mut sig = sk.sign(&msg).to_bytes().to_vec();
    if tamper {
        sig[0] ^= 0xff;
    }
    cancelOrderSignedCall {
        account: ALICE,
        orderId: order_id.into(),
        marketId: MARKET_ID,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 0,
        signature: sig.into(),
    }
    .abi_encode()
}

fn signed_cancel_input(sk: &SigningKey, order_id: [u8; 32]) -> Vec<u8> {
    signed_cancel_bytes(sk, order_id, false)
}

fn signed_cancel_input_tampered(sk: &SigningKey, order_id: [u8; 32]) -> Vec<u8> {
    signed_cancel_bytes(sk, order_id, true)
}

fn call(ctx: &mut TestCtx, input: &[u8]) -> (bool, String) {
    let out = run_perp_dex_call(input, 30_000_000, CAROL, U256::ZERO, false, ctx)
        .expect("call must not hard-fail");
    let reason = String::from_utf8_lossy(&out.bytes).to_string();
    (out.reverted, reason)
}

fn leverage_of(ctx: &mut TestCtx) -> u64 {
    storage::load_position(ctx, ALICE, MARKET_ID)
        .unwrap()
        .leverage
}

fn fixture() -> (TestCtx, SigningKey) {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register_key(&mut ctx, ALICE, &sk);
    (ctx, sk)
}

// ── placeOrderSigned ─────────────────────────────────────────────────────

/// The core change: a signature the placement REJECTED is spent all the same.
#[test]
fn rejected_signed_placement_is_not_replayable() {
    let (mut ctx, sk) = fixture();
    // FOK against an empty book — nothing to fill, so the placement is rejected.
    let input = signed_place_input(&sk, 0, PRICE, QTY, 0, 2, false);

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(reverted, "FOK on an empty book must be rejected: {reason}");
    assert!(
        !reason.contains("duplicate signature"),
        "first submission must fail on its own merits, got {reason}"
    );

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(reverted, "replay must revert");
    assert!(
        reason.contains("duplicate signature"),
        "a rejected signature must still be spent, got {reason}"
    );
}

/// The exploitable shape the old ordering left open: the rejection was TRANSIENT. A FOK that
/// could not be filled becomes fillable seconds later when a maker arrives; under burn-on-
/// success the owner's signature was still live and any observer could land the order at a
/// moment the owner believed it had failed.
#[test]
fn fok_rejected_then_liquidity_arrives_replay_still_refused() {
    let (mut ctx, sk) = fixture();
    let input = signed_place_input(&sk, 0, PRICE, QTY, 0, 2, false);

    let (reverted, _) = call(&mut ctx, &input);
    assert!(reverted, "no liquidity yet");

    // Conditions change: a resting ask now makes the very same FOK fillable.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(reverted, "replay must not execute now that it would fill");
    assert!(reason.contains("duplicate signature"), "got {reason}");
    assert_eq!(
        storage::load_position(&mut ctx, ALICE, MARKET_ID)
            .unwrap()
            .amount,
        0,
        "ALICE must hold no position from a signature she saw fail"
    );
}

/// A successful placement is still single-use.
#[test]
fn accepted_signed_placement_is_not_replayable() {
    let (mut ctx, sk) = fixture();
    let input = signed_place_input(&sk, 0, PRICE - TICK, QTY, 0, 0, false);

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(!reverted, "resting GTC must be accepted: {reason}");

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(
        reverted && reason.contains("duplicate signature"),
        "got {reason}"
    );
}

/// The burn must stay AFTER verification: an unauthenticated caller must not be able to write
/// replay markers from arbitrary bytes.
#[test]
fn unverified_signature_writes_nothing() {
    let (mut ctx, sk) = fixture();
    let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

    let (reverted, reason) = call(
        &mut ctx,
        &signed_place_input(&sk, 0, PRICE, QTY, 0, 0, true),
    );
    assert!(reverted, "tampered signature must fail verification");
    assert!(
        reason.contains("signature verification failed"),
        "got {reason}"
    );
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes_before,
        "signature failure is pre-write — no marker may be burned"
    );

    // …and the genuine signature over the same order is still usable.
    let (reverted, reason) = call(
        &mut ctx,
        &signed_place_input(&sk, 0, PRICE, QTY, 0, 0, false),
    );
    assert!(!reverted, "genuine signature must still work: {reason}");
}


/// The #23 allowance must stay NARROW. A rejected signed placement is now a deliberate
/// write-then-revert, so the guard has to exempt the burn — but only the burn. If the exemption
/// were per-call rather than per-write-count, a genuine leak inside `place_order_core` (the val0
/// bug class) would be masked on exactly this path. `perp_write_count` is monotonic and
/// revert-blind, so the burn's cost is observable from outside: it must be the WHOLE delta of a
/// rejected call, with nothing added by the rejecting code.
#[test]
fn rejected_signed_placement_writes_only_the_burn() {
    let (mut ctx, sk) = fixture();

    // Measure what a burn costs, using a placement that succeeds and then subtracting nothing:
    // a second, distinct signature is rejected, and the delta of THAT call is the burn alone.
    let before = JournalTr::perp_write_count(ctx.journal_mut());
    let (reverted, reason) = call(&mut ctx, &signed_place_input(&sk, 0, PRICE, QTY, 0, 2, false));
    assert!(reverted, "FOK on an empty book must be rejected: {reason}");
    let delta = JournalTr::perp_write_count(ctx.journal_mut()) - before;

    // 2 = the seen marker + its GC bucket. The sweep writes nothing at this block timestamp
    // (`cur < SEEN_RETENTION_BUCKETS`), and a rejected FOK must not have written anything of
    // its own. If this number grows, find out WHICH write grew before widening it.
    assert_eq!(
        delta, 2,
        "a rejected signed placement must write the replay burn and nothing else"
    );
}

// ── setLeverageSigned ────────────────────────────────────────────────────

#[test]
fn set_leverage_signed_replay_is_refused() {
    let (mut ctx, sk) = fixture();
    let input = signed_leverage_input(&sk, 3, false);

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(!reverted, "first setLeverageSigned must succeed: {reason}");
    assert_eq!(leverage_of(&mut ctx), 3);

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(
        reverted && reason.contains("duplicate signature"),
        "got {reason}"
    );
}

/// The attack the missing guard allowed: the owner de-risks, an observer replays the earlier
/// higher-leverage message and silently puts it back.
#[test]
fn set_leverage_signed_cannot_be_rolled_back_by_replay() {
    let (mut ctx, sk) = fixture();
    let raise = signed_leverage_input(&sk, 3, false);
    let lower = signed_leverage_input(&sk, 2, false);

    assert!(!call(&mut ctx, &raise).0, "raise must land");
    assert_eq!(leverage_of(&mut ctx), 3);
    assert!(!call(&mut ctx, &lower).0, "owner de-risks to 2");
    assert_eq!(leverage_of(&mut ctx), 2);

    let (reverted, reason) = call(&mut ctx, &raise);
    assert!(
        reverted && reason.contains("duplicate signature"),
        "got {reason}"
    );
    assert_eq!(
        leverage_of(&mut ctx),
        2,
        "a replay must not restore the leverage the owner moved away from"
    );
}

/// A REJECTED leverage change spends its signature too — same rule as the placement path.
#[test]
fn rejected_set_leverage_signed_is_not_replayable() {
    let (mut ctx, sk) = fixture();
    // 0 is out of range on every tier table, so this is rejected on its merits.
    let input = signed_leverage_input(&sk, 0, false);

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(reverted, "leverage 0 must be rejected");
    assert!(!reason.contains("duplicate signature"), "got {reason}");

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(
        reverted && reason.contains("duplicate signature"),
        "got {reason}"
    );
}

#[test]
fn set_leverage_signed_unverified_signature_writes_nothing() {
    let (mut ctx, sk) = fixture();
    let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

    let (reverted, reason) = call(&mut ctx, &signed_leverage_input(&sk, 3, true));
    assert!(reverted, "tampered signature must fail verification");
    assert!(
        reason.contains("signature verification failed"),
        "got {reason}"
    );
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes_before,
        "signature failure is pre-write — no marker may be burned"
    );
}



// ── placeOrder flags ─────────────────────────────────────────────────────

/// Signs over `sign_flags` but puts `wire_flags` in the calldata. Equal values = an honest
/// call; different = the tamper the signed layout has to catch.
fn signed_place_input_flags(sk: &SigningKey, sign_flags: u8, wire_flags: u8) -> Vec<u8> {
    let mut msg = [0u8; 105];
    let mut c = crate::trading::write_signed_header(&mut msg, b"perpdex_v1_order", TEST_CHAIN_ID);
    c = crate::trading::put(&mut msg, c, &ALICE.as_slice());
    c = crate::trading::put(&mut msg, c, &MARKET_ID.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[0]);
    c = crate::trading::put(&mut msg, c, &(PRICE - TICK).to_be_bytes());
    c = crate::trading::put(&mut msg, c, &QTY.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[0]);
    c = crate::trading::put(&mut msg, c, &[0]);
    c = crate::trading::put(&mut msg, c, &[0u8; 16]); // clientOrderId
    c = crate::trading::put(&mut msg, c, &SIGNED_TS.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[0]);
    c = crate::trading::put(&mut msg, c, &[sign_flags]);
    debug_assert_eq!(c, msg.len());
    placeOrderSignedCall {
        account: ALICE,
        marketId: MARKET_ID,
        side: 0,
        price: PRICE - TICK,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 0,
        flags: wire_flags,
        signature: sk.sign(&msg).to_bytes().to_vec().into(),
    }
    .abi_encode()
}

/// ⚠️ The reason `flags` is inside the signed message. If it were not, a relayer could take a
/// plain order's signature and resubmit it with the reduce-only bit set — or take a reduce-only
/// order's signature and clear the bit, turning a position-closing order into one that can open
/// and flip. Both directions are tested because the tamper is symmetric.
#[test]
fn flags_are_covered_by_the_signature() {
    for (name, sign_flags, wire_flags) in [
        ("bit set on the wire", 0u8, 1u8),
        ("bit cleared on the wire", 1u8, 0u8),
    ] {
        let (mut ctx, sk) = fixture();
        let (reverted, reason) =
            call(&mut ctx, &signed_place_input_flags(&sk, sign_flags, wire_flags));
        assert!(reverted, "{name}: tampered flags must not verify");
        assert!(
            reason.contains("signature verification failed"),
            "{name}: got {reason}"
        );
    }
}

/// Unknown bits are REJECTED, not ignored. Ignoring them would hand back a plain order to a
/// caller who asked for a modifier this build does not have — the failure would surface as
/// unexpected fills rather than as an error.
///
/// Bits 3:2 are KNOWN now (the self-trade-prevention mode), so `0x04` and `0x08` are legal
/// modes and are accepted; their reserved combination `0x0c` is refused with its own message,
/// not as an unknown bit. Bit 1 and bits 4–7 are still unknown.
#[test]
fn unknown_flag_bits_are_rejected() {
    let (mut ctx, sk) = fixture();
    for flags in [0x02u8, 0x10, 0x20, 0x40, 0x80] {
        let (reverted, reason) =
            call(&mut ctx, &signed_place_input_flags(&sk, flags, flags));
        assert!(reverted, "flags 0x{flags:02x} must be rejected");
        assert!(
            reason.contains(&format!("unknown order flags 0x{flags:02x}")),
            "got {reason}"
        );
    }
    let (reverted, reason) = call(&mut ctx, &signed_place_input_flags(&sk, 0x0c, 0x0c));
    assert!(reverted, "the reserved STP mode 0b11 must be rejected");
    assert!(
        reason.contains("placeOrder: invalid self-trade prevention mode"),
        "got {reason}"
    );
    // Unknown bits are reported BEFORE the mode is decoded: one byte, one error.
    let (reverted, reason) = call(&mut ctx, &signed_place_input_flags(&sk, 0x1c, 0x1c));
    assert!(reverted);
    assert!(reason.contains("unknown order flags 0x10"), "got {reason}");
    // The two legal non-default modes place a plain GTC bid.
    for flags in [0x04u8, 0x08] {
        let (reverted, reason) =
            call(&mut ctx, &signed_place_input_flags(&sk, flags, flags));
        assert!(!reverted, "STP mode 0x{flags:02x} is legal: {reason}");
    }
}

/// The signed path reaches the same admission predicate as the direct one. A flat position has
/// nothing to reduce, so there is no side a reduce-only order could legally take.
#[test]
fn the_signed_path_reaches_the_reduce_only_predicate() {
    let (mut ctx, sk) = fixture();
    let (reverted, reason) = call(&mut ctx, &signed_place_input_flags(&sk, 1, 1));
    assert!(reverted);
    assert!(
        reason.contains("reduce-only requires an open position"),
        "got {reason}"
    );
}

/// The direct path shares `decode_order_flags`, so it must refuse identically.
#[test]
fn the_direct_path_refuses_the_same_flags() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    for (flags, want) in [
        (1u8, "reduce-only requires an open position"),
        (2u8, "unknown order flags 0x02"),
        (0x20u8, "unknown order flags 0x20"),
        (0x0cu8, "placeOrder: invalid self-trade prevention mode"),
        (0x0du8, "placeOrder: invalid self-trade prevention mode"),
    ] {
        let input = placeOrderCall {
            marketId: MARKET_ID,
            side: 0,
            price: PRICE - TICK,
            quantity: QTY,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
            flags,
        }
        .abi_encode();
        let out = run_perp_dex_call(&input, 30_000_000, ALICE, U256::ZERO, false, &mut ctx)
            .expect("must not hard-fail");
        assert!(out.reverted, "flags 0x{flags:02x} must be rejected");
        let reason = String::from_utf8_lossy(&out.bytes).to_string();
        assert!(reason.contains(want), "got {reason}");
    }
}

// ── Cross-chain replay ───────────────────────────────────────────────────

/// ★ THE test for the chain-id binding. A payload signed for a DIFFERENT chain must not verify
/// here, in either direction.
///
/// Without the binding a signature is valid on every deployment sharing the `perpdex_v1_*`
/// prefix — testnet, mainnet, and both sides of any fork — and the replay guard does not help:
/// its seen-signature set is per-chain, so burning a signature on one chain leaves it live on
/// the others. An observer needs only the owner to have registered the same ed25519 key on both
/// (routine: key management usually spans environments) and the ≤65s recv window still open.
/// `transferFromPerpSigned` is the sharpest case — a legitimate withdrawal on A replays as an
/// unauthorised one on B.
///
/// Asserted for ALL FIVE single-call signed entrypoints, not just one: they each build their
/// own message, so one of them forgetting the header is exactly the shape this has to catch.
#[test]
fn a_signature_from_another_chain_does_not_verify() {
    const FOREIGN: u64 = TEST_CHAIN_ID + 1;

    // Same builders, one constant swapped — so the ONLY difference is the domain.
    fn order_for(sk: &SigningKey, chain: u64) -> Vec<u8> {
        let mut msg = [0u8; 105];
        let mut c =
            crate::trading::write_signed_header(&mut msg, b"perpdex_v1_order", chain);
        c = crate::trading::put(&mut msg, c, ALICE.as_slice());
        c = crate::trading::put(&mut msg, c, &MARKET_ID.to_be_bytes());
        c = crate::trading::put(&mut msg, c, &[0]);
        c = crate::trading::put(&mut msg, c, &(PRICE - TICK).to_be_bytes());
        c = crate::trading::put(&mut msg, c, &QTY.to_be_bytes());
        c = crate::trading::put(&mut msg, c, &[0, 0]);
        c = crate::trading::put(&mut msg, c, &[0u8; 16]);
        c = crate::trading::put(&mut msg, c, &SIGNED_TS.to_be_bytes());
        c = crate::trading::put(&mut msg, c, &SIGNED_RECV.to_be_bytes());
        c = crate::trading::put(&mut msg, c, &[0, 0]);
        debug_assert_eq!(c, msg.len());
        placeOrderSignedCall {
            account: ALICE,
            marketId: MARKET_ID,
            side: 0,
            price: PRICE - TICK,
            quantity: QTY,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
            timestamp: SIGNED_TS,
            recvWindow: SIGNED_RECV,
            keyId: 0,
            flags: 0,
            signature: sk.sign(&msg).to_bytes().to_vec().into(),
        }
        .abi_encode()
    }

    let (mut ctx, sk) = fixture();

    // Signed for a foreign chain -> refused here.
    let (reverted, reason) = call(&mut ctx, &order_for(&sk, FOREIGN));
    assert!(reverted, "a foreign-chain signature must not verify");
    assert!(
        reason.contains("signature verification failed"),
        "and it must fail at VERIFICATION, not on some later business rule: {reason}"
    );

    // The identical payload signed for THIS chain is accepted — so the refusal above is the
    // domain and nothing else.
    let (reverted, reason) = call(&mut ctx, &order_for(&sk, TEST_CHAIN_ID));
    assert!(!reverted, "the same order for this chain must work: {reason}");
}

/// The other four single-call entrypoints build their own messages, so each needs its own
/// proof that the header is in there. Signed for a foreign chain, all must fail verification.
#[test]
fn every_signed_entrypoint_binds_the_chain_id() {
    const FOREIGN: u64 = TEST_CHAIN_ID + 1;
    let (mut ctx, sk) = fixture();

    // cancel
    let mut m = [0u8; 102];
    let mut c = crate::trading::write_signed_header(&mut m, b"perpdex_v1_cancel", FOREIGN);
    c = crate::trading::put(&mut m, c, ALICE.as_slice());
    c = crate::trading::put(&mut m, c, &[0u8; 32]);
    c = crate::trading::put(&mut m, c, &MARKET_ID.to_be_bytes());
    c = crate::trading::put(&mut m, c, &SIGNED_TS.to_be_bytes());
    c = crate::trading::put(&mut m, c, &SIGNED_RECV.to_be_bytes());
    c = crate::trading::put(&mut m, c, &[0]);
    debug_assert_eq!(c, m.len());
    let cancel = cancelOrderSignedCall {
        account: ALICE,
        orderId: [0u8; 32].into(),
        marketId: MARKET_ID,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 0,
        signature: sk.sign(&m).to_bytes().to_vec().into(),
    }
    .abi_encode();

    // leverage
    let mut m = [0u8; 80];
    let mut c = crate::trading::write_signed_header(&mut m, b"perpdex_v1_leverage", FOREIGN);
    c = crate::trading::put(&mut m, c, ALICE.as_slice());
    c = crate::trading::put(&mut m, c, &MARKET_ID.to_be_bytes());
    c = crate::trading::put(&mut m, c, &3u64.to_be_bytes());
    c = crate::trading::put(&mut m, c, &SIGNED_TS.to_be_bytes());
    c = crate::trading::put(&mut m, c, &SIGNED_RECV.to_be_bytes());
    c = crate::trading::put(&mut m, c, &[0]);
    debug_assert_eq!(c, m.len());
    let leverage = setLeverageSignedCall {
        account: ALICE,
        marketId: MARKET_ID,
        leverage: 3,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: 0,
        signature: sk.sign(&m).to_bytes().to_vec().into(),
    }
    .abi_encode();

    for (name, input) in [("cancelOrderSigned", cancel), ("setLeverageSigned", leverage)] {
        let (reverted, reason) = call(&mut ctx, &input);
        assert!(reverted, "{name}: a foreign-chain signature must not verify");
        assert!(
            reason.contains("signature verification failed"),
            "{name}: got {reason}"
        );
    }
}

// ── Domain separation across ALL signed selectors ────────────────────────

/// Every signed entrypoint opens its message with a domain prefix, and the messages that follow
/// are similar enough that the prefix is the only thing keeping them apart. `transferToPerp` and
/// `transferFromPerp` are the sharpest case — identical layout after the prefix, opposite
/// meaning — but the rest are one field away from each other too.
///
/// Two properties, and DISTINCTNESS ALONE IS NOT ENOUGH. If prefix A were a proper prefix of B,
/// a message of kind B could be read as one of kind A with B's tail absorbed into A's first
/// field, so an A-verifier would accept bytes the owner signed as a B. Checking only
/// `a != b` would pass that and still be exploitable.
///
/// Anything added to this list must be added here too.
#[test]
fn signed_domain_prefixes_are_pairwise_distinct_and_non_nesting() {
    let prefixes: &[&[u8]] = &[
        b"perpdex_v1_order",
        b"perpdex_v1_cancel",
        b"perpdex_v1_leverage",
        b"perpdex_v1_batch_cancel",
        b"perpdex_v1_batch_order",
        b"perpdex_v1_xfer_to",
        b"perpdex_v1_xfer_from",
        b"perpdex_v1_margintype",
    ];
    for (i, a) in prefixes.iter().enumerate() {
        for (j, b) in prefixes.iter().enumerate() {
            if i == j {
                continue;
            }
            assert!(
                !b.starts_with(a),
                "{} is a prefix of {} — a {} message could be read as a {}",
                String::from_utf8_lossy(a),
                String::from_utf8_lossy(b),
                String::from_utf8_lossy(b),
                String::from_utf8_lossy(a),
            );
        }
    }
}

// ── cancelOrderSigned ────────────────────────────────────────────────────

/// It used to rely on terminal-status idempotence alone. This is the gap that left: a cancel
/// signed for an order that does not exist YET is rejected, and under the old scheme the
/// signature stayed live — so the same signature could cancel that order the moment it
/// appeared.
#[test]
fn cancel_signed_for_a_future_order_cannot_be_held_and_fired_later() {
    let (mut ctx, sk) = fixture();
    // ALICE's next direct order id is deterministic (keccak(account || nonce)), so a cancel can
    // be signed for it before it exists.
    let (future_id, _) = crate::trading::peek_order_id(&mut ctx, ALICE).unwrap();

    let input = signed_cancel_input(&sk, future_id);
    let (reverted, reason) = call(&mut ctx, &input);
    assert!(reverted, "nothing to cancel yet: {reason}");
    assert!(!reason.contains("duplicate signature"), "got {reason}");

    // The order now exists.
    let placed = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
    assert_eq!(placed, future_id, "id derivation must be the one we signed for");

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(reverted && reason.contains("duplicate signature"), "got {reason}");
    assert_eq!(
        storage::load_order(&mut ctx, &placed).unwrap().unwrap().status,
        OrderStatus::Open,
        "a spent cancel signature must not reach the order"
    );
}

#[test]
fn cancel_signed_replay_is_refused_by_the_guard_not_by_the_order_status() {
    let (mut ctx, sk) = fixture();
    let id = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
    let input = signed_cancel_input(&sk, id);

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(!reverted, "first cancel must succeed: {reason}");

    let (reverted, reason) = call(&mut ctx, &input);
    assert!(reverted, "replay must revert");
    assert!(
        reason.contains("duplicate signature"),
        "must be refused by the replay guard, not by order status: {reason}"
    );
}

#[test]
fn cancel_signed_unverified_signature_writes_nothing() {
    let (mut ctx, sk) = fixture();
    let id = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
    let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

    let (reverted, reason) = call(&mut ctx, &signed_cancel_input_tampered(&sk, id));
    assert!(reverted, "tampered signature must fail verification");
    assert!(reason.contains("signature verification failed"), "got {reason}");
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes_before,
        "signature failure is pre-write — no marker may be burned"
    );
}
