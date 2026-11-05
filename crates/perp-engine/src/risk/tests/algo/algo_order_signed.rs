use super::*;
use crate::interface::IPerpDex::placeAlgoOrderSignedCall;
use crate::types::{AlgoOrderType, ApiKey};
use ed25519_dalek::{Signer, SigningKey};

const RO: u8 = 1;
const TS: u64 = 1;
const RECV: u64 = 60;

fn register(ctx: &mut TestCtx, user: Address, sk: &SigningKey) {
    storage::save_api_key(
        ctx,
        user,
        0,
        ApiKey { pubkey: sk.verifying_key().to_bytes(), expiry: 0 },
    )
    .unwrap();
}

/// Rebuilds the canonical message INDEPENDENTLY of the engine. If the two ever disagree the
/// signature stops verifying, which is the point — a helper that called the engine's own
/// writer would pin nothing.
fn msg(chain_id: u64, account: Address, trigger: u64, qty: u64) -> Vec<u8> {
    msg_flags(chain_id, account, trigger, qty, RO)
}

fn msg_flags(chain_id: u64, account: Address, trigger: u64, qty: u64, flags: u8) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(b"perpdex_v1_algo");
    m.extend_from_slice(&chain_id.to_be_bytes());
    m.extend_from_slice(account.as_slice());
    m.push(0); // algoType = CONDITIONAL
    m.extend_from_slice(&MARKET_ID.to_be_bytes());
    m.push(Side::Sell as u8);
    m.push(AlgoOrderType::StopMarket as u8);
    m.extend_from_slice(&trigger.to_be_bytes());
    m.extend_from_slice(&0u64.to_be_bytes()); // price
    m.extend_from_slice(&qty.to_be_bytes());
    m.push(0); // tif
    m.push(0); // workingType = MARK_PRICE
    m.extend_from_slice(&[0u8; 16]); // clientAlgoId
    m.push(flags);
    m.extend_from_slice(&TS.to_be_bytes());
    m.extend_from_slice(&RECV.to_be_bytes());
    m.push(0); // keyId
    m
}

fn calldata(sig: &[u8], trigger: u64, qty: u64, account: Address) -> Vec<u8> {
    calldata_flags(sig, trigger, qty, account, RO)
}

fn calldata_flags(sig: &[u8], trigger: u64, qty: u64, account: Address, flags: u8) -> Vec<u8> {
    placeAlgoOrderSignedCall {
        account,
        algoType: 0,
        marketId: MARKET_ID,
        side: Side::Sell as u8,
        orderType: AlgoOrderType::StopMarket as u8,
        triggerPrice: trigger,
        price: 0,
        quantity: qty,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags,
        timestamp: TS,
        recvWindow: RECV,
        keyId: 0,
        signature: sig.to_vec().into(),
    }
    .abi_encode()
}

/// The signed path decodes the STP bits with the same decoder: the reserved mode is refused
/// BEFORE the burn (it is a validation failure, so the signature stays unspent and nothing is
/// written), and a legal mode is stored for the trigger to inherit.
#[test]
fn the_signed_path_validates_and_stores_the_stp_mode() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register(&mut ctx, ALICE, &sk);
    let chain_id = ctx.chain_id();

    let bad = RO | 0b1100;
    let sig = sk.sign(&msg_flags(chain_id, ALICE, 8_000, QTY as u64, bad)).to_bytes();
    let writes = JournalTr::perp_write_count(ctx.journal_mut());
    let e = crate::trading::algo::run_place_algo_order_signed(
        &calldata_flags(&sig, 8_000, QTY as u64, ALICE, bad),
        &mut ctx,
    )
    .unwrap_err();
    assert!(format!("{e:?}").contains("invalid self-trade prevention mode"), "got {e:?}");
    assert_eq!(JournalTr::perp_write_count(ctx.journal_mut()), writes, "pre-burn, write-clean");

    let good = RO | crate::types::StpMode::ExpireTaker.to_flags();
    let sig = sk.sign(&msg_flags(chain_id, ALICE, 8_000, QTY as u64, good)).to_bytes();
    let out = crate::trading::algo::run_place_algo_order_signed(
        &calldata_flags(&sig, 8_000, QTY as u64, ALICE, good),
        &mut ctx,
    )
    .unwrap();
    let id: [u8; 32] = out[..32].try_into().unwrap();
    assert_eq!(
        storage::load_order(&mut ctx, &id).unwrap().unwrap().stp,
        crate::types::StpMode::ExpireTaker
    );
}

/// The closePosition-parity refusals reach the signed path too, AFTER the burn: both read
/// what can change under an unchanged message — "would immediately trigger" the stored mark,
/// the position binding the owner's position — so both are checked in `place_core` beside the
/// caps. Each is a sanctioned reject after the burn: the signature is spent and a replay is a
/// duplicate. (The static RO|CP refusal stays a validation failure before the burn.)
#[test]
fn the_signed_path_applies_the_close_position_parity_refusals() {
    const CP: u8 = 2;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let sk = SigningKey::from_bytes(&[9u8; 32]);
    register(&mut ctx, ALICE, &sk);
    let chain_id = ctx.chain_id();
    let submit = |ctx: &mut TestCtx, trigger: u64, qty: u64, flags: u8| {
        let sig = sk.sign(&msg_flags(chain_id, ALICE, trigger, qty, flags)).to_bytes();
        crate::trading::algo::run_place_algo_order_signed(
            &calldata_flags(&sig, trigger, qty, ALICE, flags),
            ctx,
        )
    };

    // A SELL stop AT the $100.00 mark would fire on the next tick.
    let e = submit(&mut ctx, ENTRY_PRICE, QTY as u64, RO).unwrap_err();
    assert!(
        matches!(e, PerpError::RejectAfterRetainedWrite { .. })
            && format!("{e:?}").contains("order would immediately trigger"),
        "got {e:?}"
    );
    assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
    let e = submit(&mut ctx, ENTRY_PRICE, QTY as u64, RO).unwrap_err();
    assert!(
        format!("{e:?}").contains("duplicate signature"),
        "the refusal can lift a tick later, so the signature is spent: {e:?}"
    );
    // A static refusal (RO|CP together) still costs nothing and stays resubmittable.
    let writes = JournalTr::perp_write_count(ctx.journal_mut());
    let e = submit(&mut ctx, 9_000, 0, RO | CP).unwrap_err();
    assert!(format!("{e:?}").contains("reduceOnly cannot be sent with closePosition"), "{e:?}");
    assert_eq!(JournalTr::perp_write_count(ctx.journal_mut()), writes, "pre-burn, write-clean");
    let e = submit(&mut ctx, 9_000, 0, RO | CP).unwrap_err();
    assert!(format!("{e:?}").contains("reduceOnly cannot be sent"), "not a duplicate: {e:?}");

    // closePosition with ALICE flat.
    let e = submit(&mut ctx, 9_000, 0, CP).unwrap_err();
    assert!(
        matches!(e, PerpError::RejectAfterRetainedWrite { .. })
            && format!("{e:?}").contains("closePosition requires an open position"),
        "got {e:?}"
    );
    assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
    let e = submit(&mut ctx, 9_000, 0, CP).unwrap_err();
    assert!(format!("{e:?}").contains("duplicate signature"), "the burn held: {e:?}");
}

#[test]
fn a_relayer_can_submit_a_signed_conditional_order() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register(&mut ctx, ALICE, &sk);
    let chain_id = ctx.chain_id();
    let sig = sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes();

    // Submitted by someone who is NOT the owner — that is the whole point of the signed path.
    crate::trading::algo::run_place_algo_order_signed(&calldata(&sig, 8_000, QTY as u64, ALICE), &mut ctx)
        .unwrap();

    assert_eq!(
        storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(),
        1,
        "the order belongs to the SIGNER, not the sender"
    );
}

/// ⚠️ The cross-chain replay guard. The same signature on a chain with a different id must not
/// verify — this is what stops a message captured on one deployment being replayed onto
/// another, and it is the reason `chainId` is inside the signed bytes rather than merely
/// checked alongside them.
#[test]
fn a_signature_from_another_chain_does_not_verify() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register(&mut ctx, ALICE, &sk);
    let other_chain = ctx.chain_id() ^ 1;
    let sig = sk.sign(&msg(other_chain, ALICE, 8_000, QTY as u64)).to_bytes();

    let e = crate::trading::algo::run_place_algo_order_signed(
        &calldata(&sig, 8_000, QTY as u64, ALICE),
        &mut ctx,
    )
    .unwrap_err();
    assert!(format!("{e:?}").contains("signature"), "got {e:?}");
    assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
}

/// Tampering with a field the signature covers must fail. `quantity` is the sharpest one: a
/// relayer that could inflate it would be placing an order the owner never authorised.
#[test]
fn tampering_with_the_wire_quantity_does_not_verify() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register(&mut ctx, ALICE, &sk);
    let chain_id = ctx.chain_id();
    let sig = sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes();

    let e = crate::trading::algo::run_place_algo_order_signed(
        &calldata(&sig, 8_000, QTY as u64 * 2, ALICE),
        &mut ctx,
    )
    .unwrap_err();
    assert!(format!("{e:?}").contains("signature"), "got {e:?}");
}

// ── cancelAlgoOrderSigned ────────────────────────────────────────────────────────────────
//
// ⚠️ This entrypoint shipped with ZERO coverage — its `debug_assert_eq!` on the canonical
// message length had never once been executed. It is a signed path anyone may relay, so the
// gap mattered more than its size suggested.

/// Rebuilt INDEPENDENTLY of the engine, same reason as the place-side helper: a helper that
/// called the engine's own writer would pin nothing.
fn cancel_msg(chain_id: u64, account: Address, algo_id: [u8; 32]) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(b"perpdex_v1_algocancel");
    m.extend_from_slice(&chain_id.to_be_bytes());
    m.extend_from_slice(account.as_slice());
    m.extend_from_slice(&algo_id);
    m.extend_from_slice(&MARKET_ID.to_be_bytes());
    m.extend_from_slice(&TS.to_be_bytes());
    m.extend_from_slice(&RECV.to_be_bytes());
    m.push(0); // keyId
    m
}

fn cancel_calldata(sig: &[u8], account: Address, algo_id: [u8; 32]) -> Vec<u8> {
    crate::interface::IPerpDex::cancelAlgoOrderSignedCall {
        account,
        algoId: FixedBytes(algo_id),
        marketId: MARKET_ID,
        timestamp: TS,
        recvWindow: RECV,
        keyId: 0,
        signature: sig.to_vec().into(),
    }
    .abi_encode()
}

/// Places one, then cancels it over the signed path with a third party relaying.
#[test]
fn a_relayer_can_cancel_a_signed_conditional_order() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register(&mut ctx, ALICE, &sk);
    let chain_id = ctx.chain_id();
    let place_sig = sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes();
    let out = crate::trading::algo::run_place_algo_order_signed(
        &calldata(&place_sig, 8_000, QTY as u64, ALICE),
        &mut ctx,
    )
    .unwrap();
    let algo_id: [u8; 32] = out[..32].try_into().unwrap();

    let sig = sk.sign(&cancel_msg(chain_id, ALICE, algo_id)).to_bytes();
    crate::trading::algo::run_cancel_algo_order_signed(
        &cancel_calldata(&sig, ALICE, algo_id),
        &mut ctx,
    )
    .unwrap();

    assert!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().is_empty());
    assert!(
        storage::load_algo_down_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
        "and it left the market trigger index, not just her own list"
    );
    assert!(storage::load_order(&mut ctx, &algo_id).unwrap().is_none());
}

/// ⚠️ The sharpest tamper on this entrypoint: `algoId` is what says WHICH order dies. A relayer
/// able to swap it could cancel any order the owner holds — a signature authorising the cancel
/// of one stop would become authority to strip them all.
///
/// It signs for one REAL order and submits against another REAL one, so a pass means the
/// AIMED-AT order survived rather than merely that some error was returned.
///
/// ⚠️ What this test does NOT catch, measured rather than assumed: deleting `algoId` from the
/// engine's signed message entirely. No independently-built-message test can — the helper
/// still includes the field, so the two messages differ either way and verification fails for
/// a reason unrelated to tampering. The mutation that removes the field is caught by the
/// HAPPY-PATH tests above, which need a verify to SUCCEED. Do not read a green here as proof
/// the field is covered; read the pair.
#[test]
fn tampering_with_the_cancelled_algo_id_does_not_verify() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register(&mut ctx, ALICE, &sk);
    let chain_id = ctx.chain_id();

    let place = |ctx: &mut TestCtx, trigger: u64| -> [u8; 32] {
        let sig = sk.sign(&msg(chain_id, ALICE, trigger, QTY as u64)).to_bytes();
        let out = crate::trading::algo::run_place_algo_order_signed(
            &calldata(&sig, trigger, QTY as u64, ALICE),
            ctx,
        )
        .unwrap();
        out[..32].try_into().unwrap()
    };
    let signed_for = place(&mut ctx, 8_000);
    let victim = place(&mut ctx, 7_000);
    assert_ne!(signed_for, victim);

    // Authority to cancel `signed_for`, aimed at `victim`.
    let sig = sk.sign(&cancel_msg(chain_id, ALICE, signed_for)).to_bytes();
    let e = crate::trading::algo::run_cancel_algo_order_signed(
        &cancel_calldata(&sig, ALICE, victim),
        &mut ctx,
    )
    .unwrap_err();
    assert!(format!("{e:?}").contains("signature"), "got {e:?}");
    assert_eq!(
        storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(),
        2,
        "BOTH orders must survive — neither the aimed-at one nor the authorised one"
    );
}

#[test]
fn a_cancel_signature_from_another_chain_does_not_verify() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register(&mut ctx, ALICE, &sk);
    let chain_id = ctx.chain_id();
    let a = crate::trading::algo::run_place_algo_order_signed(
        &calldata(&sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes(), 8_000, QTY as u64, ALICE),
        &mut ctx,
    )
    .unwrap();
    let algo_id: [u8; 32] = a[..32].try_into().unwrap();

    let sig = sk.sign(&cancel_msg(chain_id ^ 1, ALICE, algo_id)).to_bytes();
    let e = crate::trading::algo::run_cancel_algo_order_signed(
        &cancel_calldata(&sig, ALICE, algo_id),
        &mut ctx,
    )
    .unwrap_err();
    assert!(format!("{e:?}").contains("signature"), "got {e:?}");
    assert_eq!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(), 1);
}

/// A replayed signed cancel is refused as a DUPLICATE SIGNATURE: the signature is burned on
/// submission, as on every other signed selector. A FRESH signature for the same, already
/// cancelled order still finds nothing (the order is deleted).
#[test]
fn replaying_a_cancel_is_refused_as_a_duplicate_signature() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register(&mut ctx, ALICE, &sk);
    let chain_id = ctx.chain_id();
    let a = crate::trading::algo::run_place_algo_order_signed(
        &calldata(&sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes(), 8_000, QTY as u64, ALICE),
        &mut ctx,
    )
    .unwrap();
    let algo_id: [u8; 32] = a[..32].try_into().unwrap();
    let sig = sk.sign(&cancel_msg(chain_id, ALICE, algo_id)).to_bytes();
    let data = cancel_calldata(&sig, ALICE, algo_id);

    crate::trading::algo::run_cancel_algo_order_signed(&data, &mut ctx).unwrap();
    let e = crate::trading::algo::run_cancel_algo_order_signed(&data, &mut ctx).unwrap_err();
    assert!(format!("{e:?}").contains("duplicate signature"), "got {e:?}");

    // A different, valid signature over the same cancel (one second later, still inside the
    // recv window) is not a replay — it reaches the cancel and finds the order already gone.
    let mut m = cancel_msg(chain_id, ALICE, algo_id);
    let ts_at = m.len() - 1 - 8 - 8; // …‖timestamp(8)‖recvWindow(8)‖keyId(1)
    m[ts_at..ts_at + 8].copy_from_slice(&(TS + 1).to_be_bytes());
    let fresh = crate::interface::IPerpDex::cancelAlgoOrderSignedCall {
        account: ALICE,
        algoId: FixedBytes(algo_id),
        marketId: MARKET_ID,
        timestamp: TS + 1,
        recvWindow: RECV,
        keyId: 0,
        signature: sk.sign(&m).to_bytes().to_vec().into(),
    }
    .abi_encode();
    let e = crate::trading::algo::run_cancel_algo_order_signed(&fresh, &mut ctx).unwrap_err();
    assert!(format!("{e:?}").contains("not found"), "got {e:?}");
}

/// Replay: one signature, one order. The id IS `keccak256(signature)`, and the same hash keys
/// the seen-signature set, so a resubmission bounces off the guard rather than placing a
/// second order.
#[test]
fn the_same_signature_cannot_be_submitted_twice() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    register(&mut ctx, ALICE, &sk);
    let chain_id = ctx.chain_id();
    let sig = sk.sign(&msg(chain_id, ALICE, 8_000, QTY as u64)).to_bytes();
    let data = calldata(&sig, 8_000, QTY as u64, ALICE);

    crate::trading::algo::run_place_algo_order_signed(&data, &mut ctx).unwrap();
    let e = crate::trading::algo::run_place_algo_order_signed(&data, &mut ctx).unwrap_err();
    assert!(format!("{e:?}").contains("duplicate signature"), "got {e:?}");
    assert_eq!(storage::load_user_algo_orders(&mut ctx, ALICE, MARKET_ID).unwrap().len(), 1);
}
