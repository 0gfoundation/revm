//! Batch-call shell shared by every `batch*` selector: pre-decode array-length reading, the
//! up-front dynamic gas bound, the numeric reason-code / tag vocabulary, the index-aligned status
//! blob encoder, and the abort-forward driver.
//!
//! Everything here is selector-agnostic on purpose — Phase 1 (`batchCancelOrders`) and Phase 2
//! (`batchPlaceOrders`) share it verbatim; only the per-item closure differs.
//!
//! # Why the driver always returns `Ok` once the loop has begun
//!
//! Perp state is **commit-only** (#23): there is no perp undo. If item 0 has already committed and
//! item 3 propagates an `Err`, the frame revert does **not** roll back item 0's perp writes, but it
//! **does** truncate all logs (`checkpoint_revert` → `logs.truncate`) and returns `new_reverted`.
//! That is the worst possible outcome: item 0's state change is live with no events at all, and the
//! user is told the transaction failed — ghost orders. So "propagate → whole-batch revert" is
//! literally unimplementable here; the batch must never rely on frame revert once any item may have
//! committed. Hence abort-forward: mark the offending item `Aborted`, mark the tail `NotAttempted`,
//! stop, and return `Ok` so the committed prefix keeps its logs and stays consistent with state.
//!
//! Only a true [`PrecompileError::Fatal`] propagates. Note that `perp_invariant_err` is
//! `PrecompileError::Other` with an `[INVARIANT] ` prefix (`errors.rs:11`), **not** `Fatal` — an
//! invariant is an abort, not a propagate.

use core::sync::atomic::{AtomicU64, Ordering};

use context::{ContextTr, JournalTr};

use crate::{
    perp_dex::{
        errors::perp_err,
        interface::IPerpDex::{batchCancelOrdersCall, batchCancelOrdersSignedCall},
        CANCEL_ORDER_GAS,
    },
    PrecompileError,
};
use alloy_sol_types::SolCall;

// ── Limits & gas ──────────────────────────────────────────────────────────────

/// Maximum number of ids accepted by `batchCancelOrders` / `batchCancelOrdersSigned`.
///
/// Enforced on the **decoded** length (authoritative), never on the pre-decode length word, so a
/// legal-but-non-minimal encoding can never fail a valid batch.
pub const MAX_BATCH_CANCEL: usize = 256;

/// Fixed width of one status record in the returned blob: `tag(1) || orderId(32) || reason(1)`.
pub const BATCH_STATUS_RECORD_LEN: usize = 34;

/// Envelope gas of a batch call — calldata scan, statuses encode, signature verification. This is
/// also the batch selectors' entry in the `SELECTORS` table, i.e. the **floor** used by the early
/// `gas_cost > gas_limit` check; the real charge is [`batch_gas`].
pub const BASE_BATCH_GAS: u64 = 20_000;

/// Up-front gas for a batch: `BASE_BATCH_GAS + n * unit`.
///
/// `n` comes from an attacker-controlled calldata word, so the arithmetic is **saturating**: a huge
/// `n` saturates to `u64::MAX` and fails the `> gas_limit` check instead of wrapping into a small
/// number that would pass it.
#[inline]
pub fn batch_gas(n: usize, unit: u64) -> u64 {
    BASE_BATCH_GAS.saturating_add((n as u64).saturating_mul(unit))
}

// ── Pre-decode array length ───────────────────────────────────────────────────

/// Where a dynamic-array argument lives in a selector's calldata, so its length can be read
/// **before** ABI decoding (the gas decision in `run_perp_dex_call` happens pre-decode).
#[derive(Clone, Copy, Debug)]
pub struct BatchArrayLayout {
    /// 0-based index of the argument's head word, counted after the 4-byte selector.
    pub head_word: usize,
    /// Canonical byte offset (relative to the end of the selector) the argument's tail must sit at.
    /// For the first dynamic argument of a standard encoder this is `head_words * 32`.
    pub tail_offset: usize,
}

/// `batchCancelOrders(bytes32[])` — one head word, tail immediately after it.
pub const CANCEL_DIRECT_LAYOUT: BatchArrayLayout = BatchArrayLayout {
    head_word: 0,
    tail_offset: 0x20,
};

/// `batchCancelOrdersSigned(address,uint8,uint64,uint64,bytes32[],bytes)` — six head words; the
/// `orderIds` head is word 4 and, being the first dynamic argument, its tail starts at 6*32 = 0xc0.
pub const CANCEL_SIGNED_LAYOUT: BatchArrayLayout = BatchArrayLayout {
    head_word: 4,
    tail_offset: 0xc0,
};

impl BatchArrayLayout {
    /// Reads the array's declared length without decoding, rejecting anything the calldata cannot
    /// actually back.
    ///
    /// 1. bounds-check the head word;
    /// 2. require the head offset to be **canonical** (`tail_offset`, full 32-byte compare) — a
    ///    non-canonical but ABI-legal layout is rejected rather than mis-read;
    /// 3. require the length to fit `u32` and, crucially, **to be backed by real calldata bytes**:
    ///    `alloy-sol-types`' `DynSeqToken::decode_from` calls `vec_try_with_capacity(len)` on this
    ///    attacker-controlled word *before* reading any element, so a ~100-byte call carrying a huge
    ///    length word is a memory-DoS vector. The gas path is not allowed to be the only thing that
    ///    distrusts this word, so the same check gates the decode.
    pub fn checked_len(&self, input: &[u8]) -> Result<usize, PrecompileError> {
        let head_start = 4 + self.head_word * 32;
        let head = input
            .get(head_start..head_start + 32)
            .ok_or_else(|| perp_err("batch: truncated calldata (array head)"))?;
        let mut canonical = [0u8; 32];
        canonical[24..].copy_from_slice(&(self.tail_offset as u64).to_be_bytes());
        if head != canonical.as_slice() {
            return Err(perp_err("batch: non-canonical array offset"));
        }

        let len_start = 4 + self.tail_offset;
        let len_word = input
            .get(len_start..len_start + 32)
            .ok_or_else(|| perp_err("batch: truncated calldata (array length)"))?;
        if len_word[..28] != [0u8; 28] {
            return Err(perp_err("batch: array length out of range"));
        }
        let len = u32::from_be_bytes(len_word[28..32].try_into().unwrap()) as usize;

        let need = len
            .checked_mul(32)
            .and_then(|body| body.checked_add(len_start + 32))
            .ok_or_else(|| perp_err("batch: array length out of range"))?;
        if input.len() < need {
            return Err(perp_err("batch: array length exceeds calldata"));
        }
        Ok(len)
    }
}

/// The up-front gas of the dynamic-cost batch selectors, computed from the **pre-decode** array
/// length; `None` for every other selector.
///
/// `None` is also returned when the length word cannot be trusted ([`BatchArrayLayout::checked_len`]
/// failed). In that case only the envelope floor is charged and the batch handler turns the very
/// same defensive read into a clean `Error(string)` revert — no work happens on either path.
///
/// The per-item unit is taken straight from the single-order selector's cost
/// (`CANCEL_ORDER_GAS`) — never a re-invented number. Phase 2 adds a `batchPlaceOrders` arm here
/// with the `placeOrder` cost.
pub fn batch_dynamic_gas(selector: [u8; 4], input: &[u8]) -> Option<u64> {
    let (layout, unit) = if selector == batchCancelOrdersCall::SELECTOR {
        (CANCEL_DIRECT_LAYOUT, CANCEL_ORDER_GAS)
    } else if selector == batchCancelOrdersSignedCall::SELECTOR {
        (CANCEL_SIGNED_LAYOUT, CANCEL_ORDER_GAS)
    } else {
        return None;
    };
    Some(batch_gas(layout.checked_len(input).ok()?, unit))
}

/// Pre-loop length gate on the **decoded** array: an empty or oversized batch reverts the whole
/// call. Both faults are pre-write, so the commit-only write tripwire stays clean.
pub fn check_batch_len(n: usize, max: usize, what: &str) -> Result<(), PrecompileError> {
    if n == 0 {
        return Err(perp_err(format!("{what}: empty batch")));
    }
    if n > max {
        return Err(perp_err(format!("{what}: batch too large (max {max})")));
    }
    Ok(())
}

// ── Status vocabulary ─────────────────────────────────────────────────────────

/// Byte 0 of a status record.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerpBatchTag {
    /// Genuine per-item reject — write-clean, the loop continued.
    Rejected = 0,
    /// Accepted: the cancel removed the order (or, in Phase 2, the placement rests in the book).
    Accepted = 1,
    /// Reserved for `batchPlaceOrders` (Phase 2): accepted AND fully filled. Never produced by
    /// `batchCancelOrders`; the numbering is fixed here so Phase 2 reuses this enum unchanged.
    Filled = 2,
    /// The item errored **after** writing perp state (invariant / arithmetic / post-write). Its
    /// writes cannot be undone, so the loop stops here and the call still returns `Ok`.
    Aborted = 3,
    /// Never run: an earlier item aborted.
    NotAttempted = 4,
}

/// Byte 33 of a status record — the numeric reason. **Never** an error string: batch results must
/// not carry `format!`/`{:?}` output.
///
/// Bands are fixed so Phase 2 can extend without renumbering:
///   `0` none · `1..=15` cancel-path rejects · `16..=63` reserved for place-path rejects ·
///   `254` invariant · `255` catch-all.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerpBatchReason {
    /// No reason — the item was accepted, or was never attempted.
    None = 0,
    /// `cancelOrder: order not found`
    OrderNotFound = 1,
    /// `cancelOrder: not owner`
    NotOwner = 2,
    /// `cancelOrder: order not cancellable`
    NotCancellable = 3,
    /// `cancelOrder: unknown market`
    UnknownMarket = 4,
    /// An `[INVARIANT] `-prefixed error (`perp_invariant_err`). Always paired with
    /// [`PerpBatchTag::Aborted`] in practice.
    Invariant = 254,
    /// Anything not in the table above.
    Other = 255,
}

/// Maps an item error onto its numeric reason code.
///
/// This is a **reporting-only** mapping: it never decides control flow. Whether an item is a
/// genuine reject or an abort is decided at runtime from the perp write counter (see
/// [`drive_batch`]), exactly so that this table cannot silently change semantics. An unrecognised
/// message degrades to [`PerpBatchReason::Other`]; `reason_code_table_is_pinned` in the trading
/// tests pins each mapping so a message rename fails loudly instead of silently.
pub fn reason_code(err: &PrecompileError) -> PerpBatchReason {
    let msg = match err {
        PrecompileError::Other(msg) => msg.as_str(),
        _ => return PerpBatchReason::Other,
    };
    if msg.starts_with("[INVARIANT] ") {
        return PerpBatchReason::Invariant;
    }
    // Cancel path (`cancel_order_core`): all four are pure reads, hence write-clean.
    if msg.contains("order not found") {
        PerpBatchReason::OrderNotFound
    } else if msg.contains("not owner") {
        PerpBatchReason::NotOwner
    } else if msg.contains("order not cancellable") {
        PerpBatchReason::NotCancellable
    } else if msg.contains("unknown market") {
        PerpBatchReason::UnknownMarket
    } else {
        PerpBatchReason::Other
    }
}

/// Index-aligned status blob: `n` fixed-width [`BATCH_STATUS_RECORD_LEN`]-byte records in input
/// order.
#[derive(Debug)]
pub struct BatchStatusBlob(Vec<u8>);

impl BatchStatusBlob {
    /// Allocates for exactly `n` records. `n` is already bounded by `MAX_BATCH_*` here.
    pub fn with_capacity(n: usize) -> Self {
        Self(Vec::with_capacity(n * BATCH_STATUS_RECORD_LEN))
    }

    /// Appends one record: `tag(1) || order_id(32) || reason(1)`.
    pub fn push(&mut self, tag: PerpBatchTag, order_id: &[u8; 32], reason: PerpBatchReason) {
        self.0.push(tag as u8);
        self.0.extend_from_slice(order_id);
        self.0.push(reason as u8);
    }

    /// Number of records appended so far.
    pub fn len(&self) -> usize {
        self.0.len() / BATCH_STATUS_RECORD_LEN
    }

    /// True when no record has been appended.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The raw blob.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

// ── Abort tripwire ────────────────────────────────────────────────────────────

/// Process-wide count of abort-forward events (an item that errored **after** writing perp state).
/// Diagnostic only — the same class of leak the commit-only `PERP_WRITE_THEN_REVERT_COUNT` tripwire
/// watches, except a batch abort returns `Ok` so that counter cannot see it.
pub static PERP_BATCH_ABORT_COUNT: AtomicU64 = AtomicU64::new(0);

/// Reads [`PERP_BATCH_ABORT_COUNT`].
pub fn perp_batch_abort_count() -> u64 {
    PERP_BATCH_ABORT_COUNT.load(Ordering::Relaxed)
}

// ── Abort-forward driver ──────────────────────────────────────────────────────

/// Runs `n` items in strict calldata order and encodes an index-aligned status blob.
///
/// * `Ok((tag, id))` → that tag, reason `None`.
/// * `Err(Fatal)` → propagated untouched (node/infra bug; block-level).
/// * `Err(e)` with the perp write counter **unchanged** → genuine reject; record `Rejected` and
///   continue. `perp_write_count` is monotonic and is never decremented by a revert
///   (`inner.rs:260`), which is what makes the delta a sound witness: a reject that wrote nothing
///   contributes zero keys to the block delta and is invisible to the commitment.
/// * `Err(e)` with the counter **bumped** → the item wrote and then failed. Under commit-only that
///   write cannot be undone, so record `Aborted`, fill the tail with `NotAttempted`, stop, and
///   still return `Ok` (see the module docs).
///
/// `echo_id(k)` supplies the id recorded for items that produce no id of their own (rejects, the
/// abort, the untouched tail). `batchCancelOrders` echoes the input `orderIds[k]` there.
pub fn drive_batch<CTX, EchoFn, ItemFn>(
    context: &mut CTX,
    n: usize,
    echo_id: EchoFn,
    mut run_item: ItemFn,
) -> Result<Vec<u8>, PrecompileError>
where
    CTX: ContextTr,
    EchoFn: Fn(usize) -> [u8; 32],
    ItemFn: FnMut(&mut CTX, usize) -> Result<(PerpBatchTag, [u8; 32]), PrecompileError>,
{
    let mut blob = BatchStatusBlob::with_capacity(n);
    let mut k = 0usize;
    while k < n {
        // Runtime genuine-vs-abort witness, snapshotted per item (NOT a string match on the error).
        let writes_before = context.journal_mut().perp_write_count();
        match run_item(context, k) {
            Ok((tag, order_id)) => blob.push(tag, &order_id, PerpBatchReason::None),
            Err(PrecompileError::Fatal(e)) => return Err(PrecompileError::Fatal(e)),
            Err(e) => {
                let wrote = context.journal_mut().perp_write_count() != writes_before;
                let code = reason_code(&e);
                if !wrote {
                    // Write-clean: HL-style per-item reject, the batch carries on.
                    blob.push(PerpBatchTag::Rejected, &echo_id(k), code);
                } else {
                    PERP_BATCH_ABORT_COUNT.fetch_add(1, Ordering::Relaxed);
                    blob.push(PerpBatchTag::Aborted, &echo_id(k), code);
                    for tail in (k + 1)..n {
                        blob.push(
                            PerpBatchTag::NotAttempted,
                            &echo_id(tail),
                            PerpBatchReason::None,
                        );
                    }
                    break;
                }
            }
        }
        k += 1;
    }
    debug_assert_eq!(blob.len(), n, "status blob must stay index-aligned");
    Ok(blob.into_bytes())
}
