//! Signed-entrypoint helpers shared by every `*Signed` call: the best-effort seen-signature GC
//! that follows the replay-marker burn, the recv-window and API-key expiry checks, the
//! signed-message domain header, and ed25519 verification.

use crate::{host::PerpHost, storage, types::ApiKey, PerpError};
use ed25519_dalek::{Signature, VerifyingKey};

/// Best-effort seen-signature GC.
///
/// The sweep is pure housekeeping — it drops replay markers whose recv window expired a full
/// retention margin ago — but it is a fallible WRITE, and it runs after the signature has been
/// burned. Under commit-only an `Err` here would revert a call whose burn is already committed:
/// that signature becomes permanently unusable while its work never happened. So an ordinary error
/// is dropped on the floor. A half-done sweep is harmless by the same retention argument
/// (`storage::gc_seen_buckets`): the markers it drops can no longer cause a false reject, and a
/// bucket it fails to finish is re-visited or simply leaks stale markers until a wipe.
///
/// A [`PerpError::Fatal`] still propagates — that is a node/DB fault, not a housekeeping
/// failure, and it must not be masked (same rule `drive_batch` applies to per-item errors).
pub(crate) fn gc_seen_buckets_best_effort<H: PerpHost>(
    context: &mut H,
    block_ts: u64,
) -> Result<(), PerpError> {
    match storage::gc_seen_buckets(context, block_ts) {
        Err(PerpError::Fatal(e)) => Err(PerpError::Fatal(e)),
        _ => Ok(()),
    }
}

// ── Timestamp / recvWindow helpers ────────────────────────────────────────────

const MAX_RECV_WINDOW: u64 = 60; // seconds
const CLOCK_SKEW_ALLOWANCE: u64 = 5; // seconds of future tolerance

pub(crate) fn check_api_key_expiry<H: PerpHost>(
    context: &mut H,
    key: &ApiKey,
) -> Result<(), &'static str> {
    if key.expiry == 0 {
        return Ok(());
    }
    let block_ts: u64 = context.timestamp();
    if block_ts >= key.expiry {
        return Err("api key has expired");
    }
    Ok(())
}

pub(crate) fn check_recv_window<H: PerpHost>(
    context: &mut H,
    timestamp: u64,
    recv_window: u64,
) -> Result<(), &'static str> {
    let block_ts: u64 = context.timestamp();
    let window = recv_window.min(MAX_RECV_WINDOW);

    if timestamp > block_ts + CLOCK_SKEW_ALLOWANCE {
        return Err("timestamp is in the future");
    }
    if block_ts.saturating_sub(timestamp) > window {
        return Err("timestamp expired (outside recvWindow)");
    }
    Ok(())
}

// ── Signed-message domain ─────────────────────────────────────────────────────

/// Writes `prefix || chainId(8 BE)` at the head of a signed message; returns the offset just past
/// it. Every signed entrypoint starts here, so the domain is defined once rather than seven times.
///
/// # Why the chain id is in the message
///
/// Without it a signed payload is valid on EVERY deployment that shares the `perpdex_v1_*` prefix:
/// testnet, mainnet, and both sides of any fork. The seen-signature replay guard does not help —
/// it is per-chain state, so burning a signature on one chain leaves it live on the others. An
/// observer only needs the owner to have registered the same ed25519 key on both (routine — the
/// same key management usually spans environments) and the ~65s recv window to still be open.
/// `transferFromPerpSigned` is the sharpest case: a legitimate withdrawal on A replays as an
/// unauthorised one on B.
///
/// This is EIP-155 / EIP-712's `chainId`, with the prefix playing `name` + `version`. We do NOT
/// bind the contract address: there is exactly one PerpDEX precompile address and it is the same
/// on every deployment, so it would separate nothing.
///
/// ⚠️ Callers write the rest of the message with a CURSOR, not hardcoded offsets. The offsets used
/// to be literals, and adding this field shifted every one of them — exactly the edit where a
/// silent off-by-one produces a message that still verifies against itself and against nothing
/// else. Each site ends with a `debug_assert_eq!` on the final cursor.
#[inline]
pub(crate) fn write_signed_header(buf: &mut [u8], prefix: &[u8], chain_id: u64) -> usize {
    let p = prefix.len();
    buf[..p].copy_from_slice(prefix);
    buf[p..p + 8].copy_from_slice(&chain_id.to_be_bytes());
    p + 8
}

/// Appends `bytes` at `cursor`, returning the new cursor.
#[inline]
pub(crate) fn put(buf: &mut [u8], cursor: usize, bytes: &[u8]) -> usize {
    buf[cursor..cursor + bytes.len()].copy_from_slice(bytes);
    cursor + bytes.len()
}

// ── ed25519 helpers ───────────────────────────────────────────────────────────

/// ⚠️ `verify_strict`, not `verify`, and the seen-signature replay guard DEPENDS on that.
///
/// Every signed entrypoint keys its replay marker on `keccak256(signature)` — the signature BYTES,
/// not the message. So the guard is only sound while an observed valid signature cannot be
/// transformed into a different signature that is also valid over the same message: a malleated
/// copy would hash differently, miss the seen marker, and (for `placeOrderSigned`) derive a fresh
/// orderId as well, walking straight past the guard.
///
/// `verify_strict` is what rules that out — it rejects non-canonical encodings and small-order
/// public keys. Downgrading it to `verify` would weaken the replay guards at a distance, from a
/// change site that looks unrelated to replay.
pub(crate) fn verify_ed25519(
    pubkey_bytes: &[u8; 32],
    message: &[u8],
    signature_bytes: &[u8],
) -> Result<(), &'static str> {
    let pubkey =
        VerifyingKey::from_bytes(pubkey_bytes).map_err(|_| "invalid ed25519 public key")?;
    let sig_arr: &[u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| "signature must be 64 bytes")?;
    let signature = Signature::from_bytes(sig_arr);
    pubkey
        .verify_strict(message, &signature)
        .map_err(|_| "signature verification failed")
}
