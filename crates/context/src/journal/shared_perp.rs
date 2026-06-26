//! Off-trie PerpDEX overlay substrate for PARALLEL block execution (catalog #21 spike, step 1).
//!
//! [`SharedPerpBook`] is the concurrent analogue of the single-threaded [`super::inner::PerpSection`]
//! overlay: N execution slots share one `Arc<SharedPerpBook>` and mutate it under fine-grained locks
//! (added in step 2). It is a generic `DashMap<B256, SharedPerpEntry>` (decision D1) so the existing
//! serialization + per-block commitment machinery is reused unchanged — `take_delta` yields the same
//! [`PerpDelta`] the serial path does.
//!
//! Reversibility is per-transaction: each slot carries its own [`PerpWriteSet`] (the move-based undo
//! log), so a reverting transaction restores ONLY its own writes without disturbing concurrent slots.
//! This replaces the single linear `PerpSection.undo` (which is per-journal and single-threaded).
//!
//! Step-1 scope: the data structure + per-tx rollback + a single-threaded equivalence proof vs
//! `PerpSection`. NO locks (step 2), NO threads/driver (step 3), NO precompile/journal routing yet
//! (that migration moves the bare-`&mut dyn Any` call sites to the closure API below and lands with
//! the driver).

use context_interface::journaled_state::PerpDelta;
use core::any::Any;
use dashmap::DashMap;
use primitives::B256;
use std::boxed::Box;
use std::vec::Vec;

/// A type-erased blob safe to share across worker threads. `DashMap<_, V>` requires `V: Send` to be
/// `Send`, and `Sync` lets the `Arc<SharedPerpBook>` be shared by reference; the concrete blob types
/// (order lists, accounts, level FIFOs) are all plain `Send + Sync` data, so this bound is free.
/// (The serial [`super::inner::PerpEntry`] uses a looser `Box<dyn Any>`; the two unify when the
/// precompile routing migrates in step 3.)
type AnyBox = Box<dyn Any + Send + Sync>;
/// Lowers a blob to canonical off-trie bytes (monomorphized in the precompile, so this crate stays
/// format-agnostic).
type SerFn = fn(&dyn Any) -> Vec<u8>;
/// Deep-clones a blob behind the type-erased box (for the undo snapshot).
type CloneFn = fn(&dyn Any) -> AnyBox;

/// One off-trie overlay value: either a deferred deserialized blob (`Struct`, serialized once at
/// `take_delta`) or raw bytes (`Bytes`, e.g. level queues / `store_blob`). Mirrors `PerpEntry`.
enum SharedPerpEntry {
    Struct {
        val: AnyBox,
        ser: SerFn,
        clone: CloneFn,
    },
    Bytes(Vec<u8>),
}

impl SharedPerpEntry {
    /// Lowers to canonical bytes by reference (read path / delta).
    fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Struct { val, ser, .. } => ser(val.as_ref()),
            Self::Bytes(b) => b.clone(),
        }
    }

    /// Deep-clones the entry (for the undo snapshot) via the per-entry clone fn-pointer.
    fn snapshot(&self) -> Self {
        match self {
            Self::Struct { val, ser, clone } => Self::Struct {
                val: clone(val.as_ref()),
                ser: *ser,
                clone: *clone,
            },
            Self::Bytes(b) => Self::Bytes(b.clone()),
        }
    }
}

impl core::fmt::Debug for SharedPerpEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Struct { .. } => f.write_str("SharedPerpEntry::Struct(..)"),
            Self::Bytes(b) => write!(f, "SharedPerpEntry::Bytes({} bytes)", b.len()),
        }
    }
}

/// Per-transaction, move-based undo log against a [`SharedPerpBook`]. Records the prior entry for
/// each key the transaction touched (`None` = key was absent), reverse-replayed on revert. One per
/// execution slot, so reverts never disturb concurrent slots.
#[derive(Debug, Default)]
pub struct PerpWriteSet {
    undo: Vec<(B256, Option<SharedPerpEntry>)>,
}

/// Concurrent off-trie PerpDEX overlay shared across execution slots. See module docs.
#[derive(Debug, Default)]
pub struct SharedPerpBook {
    /// In-block write overlay (one value per key); drained as a [`PerpDelta`] at block end.
    working: DashMap<B256, SharedPerpEntry>,
}

impl SharedPerpBook {
    /// Creates an empty shared book.
    pub fn new() -> Self {
        Self {
            working: DashMap::new(),
        }
    }

    /// Reads the overlay for `key` as canonical bytes; `None` = not written in this block.
    pub fn get_bytes(&self, key: B256) -> Option<Vec<u8>> {
        self.working.get(&key).map(|e| e.to_bytes())
    }

    /// Writes raw `Bytes`, recording the prior entry into `ws` for revert.
    pub fn store_bytes(&self, key: B256, value: Vec<u8>, ws: &mut PerpWriteSet) {
        let prev = self.working.insert(key, SharedPerpEntry::Bytes(value));
        ws.undo.push((key, prev));
    }

    /// Writes a deferred `Struct` (serialization deferred to `take_delta`), recording prior for revert.
    pub fn store_struct(
        &self,
        key: B256,
        val: AnyBox,
        ser: SerFn,
        clone: CloneFn,
        ws: &mut PerpWriteSet,
    ) {
        let prev = self
            .working
            .insert(key, SharedPerpEntry::Struct { val, ser, clone });
        ws.undo.push((key, prev));
    }

    /// Runs `f` against the deferred `Struct` value at `key` (read-only). `None` if absent or stored
    /// as raw bytes. The DashMap shard guard is held only for the duration of `f`.
    pub fn with_struct<R>(&self, key: B256, f: impl FnOnce(&dyn Any) -> R) -> Option<R> {
        let guard = self.working.get(&key)?;
        match &*guard {
            SharedPerpEntry::Struct { val, .. } => Some(f(val.as_ref())),
            SharedPerpEntry::Bytes(_) => None,
        }
    }

    /// Snapshots the value at `key` into `ws`, then runs `f` against it for IN-PLACE mutation. `None`
    /// if absent or stored as raw bytes. Closure form (not a returned `&mut`) because the DashMap
    /// guard cannot outlive the call. One undo snapshot per call, so callers fetch the handle ONCE
    /// per logical mutation, not in a loop (mirrors `PerpSection::get_struct_mut`).
    pub fn with_struct_mut<R>(
        &self,
        key: B256,
        ws: &mut PerpWriteSet,
        f: impl FnOnce(&mut dyn Any) -> R,
    ) -> Option<R> {
        let mut guard = self.working.get_mut(&key)?;
        // Only deferred `Struct` entries are mutable in place.
        if !matches!(&*guard, SharedPerpEntry::Struct { .. }) {
            return None;
        }
        // Snapshot the pre-mutation value ONCE for revert (deep-clone via the entry's clone fn-ptr).
        let snapshot = guard.snapshot();
        ws.undo.push((key, Some(snapshot)));
        match &mut *guard {
            SharedPerpEntry::Struct { val, .. } => Some(f(val.as_mut())),
            // Unreachable: matched `Struct` above and the guard still holds the same entry.
            SharedPerpEntry::Bytes(_) => None,
        }
    }

    /// Reverts the writes recorded in `ws`, in reverse order (consumes the write-set). A `None` prior
    /// means the key was absent before the write, so revert removes it.
    pub fn undo(&self, ws: PerpWriteSet) {
        for (key, prev) in ws.undo.into_iter().rev() {
            match prev {
                Some(entry) => {
                    self.working.insert(key, entry);
                }
                None => {
                    self.working.remove(&key);
                }
            }
        }
    }

    /// Drains the block's net writes as a [`PerpDelta`], serializing each entry to canonical bytes
    /// ONCE here (deferred `Struct` writes serialized at this block boundary). Clears the overlay.
    pub fn take_delta(&self) -> PerpDelta {
        let mut delta = PerpDelta::default();
        for entry in self.working.iter() {
            delta.insert(*entry.key(), entry.value().to_bytes());
        }
        self.working.clear();
        delta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(n: u8) -> B256 {
        B256::with_last_byte(n)
    }
    fn ser_u32(v: &dyn Any) -> Vec<u8> {
        v.downcast_ref::<u32>().unwrap().to_le_bytes().to_vec()
    }
    fn clone_u32(v: &dyn Any) -> AnyBox {
        Box::new(*v.downcast_ref::<u32>().unwrap())
    }

    #[test]
    fn shared_store_bytes_roundtrip() {
        let book = SharedPerpBook::new();
        let mut ws = PerpWriteSet::default();
        book.store_bytes(k(1), vec![7, 8, 9], &mut ws);
        assert_eq!(book.get_bytes(k(1)), Some(vec![7, 8, 9]));
        assert_eq!(book.get_bytes(k(2)), None);
    }

    #[test]
    fn shared_store_struct_defers_and_serializes_at_delta() {
        let book = SharedPerpBook::new();
        let mut ws = PerpWriteSet::default();
        book.store_struct(k(1), Box::new(7u32), ser_u32, clone_u32, &mut ws);
        // Typed fast-path read returns the live struct (no serialization).
        let read = book.with_struct(k(1), |v| *v.downcast_ref::<u32>().unwrap());
        assert_eq!(read, Some(7u32));
        // Block-end drain serializes ONCE to the same bytes the serial path produces.
        let delta = book.take_delta();
        assert_eq!(delta.get(&k(1)), Some(&7u32.to_le_bytes().to_vec()));
    }

    #[test]
    fn shared_with_struct_mut_mutates_in_place_and_reverts() {
        let book = SharedPerpBook::new();
        // Seed a struct as the committed baseline (its write-set is dropped = permanent).
        let mut seed = PerpWriteSet::default();
        book.store_struct(k(1), Box::new(10u32), ser_u32, clone_u32, &mut seed);
        drop(seed);

        // In-place mutation via the &mut handle records one undo snapshot.
        let mut ws = PerpWriteSet::default();
        let r = book.with_struct_mut(k(1), &mut ws, |v| {
            *v.downcast_mut::<u32>().unwrap() = 99;
        });
        assert_eq!(r, Some(()));
        assert_eq!(
            book.with_struct(k(1), |v| *v.downcast_ref::<u32>().unwrap()),
            Some(99u32)
        );

        // Revert restores the pre-mutation value (snapshot-on-mutate undo).
        book.undo(ws);
        assert_eq!(
            book.with_struct(k(1), |v| *v.downcast_ref::<u32>().unwrap()),
            Some(10u32)
        );

        // Absent and raw-`Bytes` keys cannot be mutated in place.
        let mut ws2 = PerpWriteSet::default();
        assert_eq!(book.with_struct_mut(k(2), &mut ws2, |_| ()), None);
        book.store_bytes(k(3), vec![1, 2, 3], &mut ws2);
        assert_eq!(book.with_struct_mut(k(3), &mut ws2, |_| ()), None);
    }

    /// Step-1 acceptance gate: the SharedPerpBook must produce a byte-identical net block delta to
    /// the serial `PerpSection` (driven via `JournalInner`'s public `perp_*` API) for the same
    /// logical op sequence — overwrite (last write wins), a deferred struct, and in-place mutation.
    #[test]
    fn shared_book_delta_matches_perpsection() {
        use crate::journal::{JournalEntry, JournalInner};

        // PerpSection's clone fn-ptr returns the looser `Box<dyn Any>` (vs the shared book's
        // `Send + Sync` box); both serialize via the same `ser_u32`, so the bytes match.
        fn clone_u32_serial(v: &dyn Any) -> Box<dyn Any> {
            Box::new(*v.downcast_ref::<u32>().unwrap())
        }

        // Serial side.
        let mut j = JournalInner::<JournalEntry>::new();
        j.perp_store(k(1), vec![0xAA]);
        j.perp_store(k(1), vec![0xBB]); // overwrite -> last write wins
        j.perp_store_struct(k(2), Box::new(5u32), ser_u32, clone_u32_serial);
        *j.perp_get_struct_mut(k(2)).unwrap().downcast_mut::<u32>().unwrap() = 7; // in-place
        j.perp_store(k(3), vec![1, 2, 3]);
        let serial_delta = j.take_perp_delta();

        // Shared side: same logical ops.
        let book = SharedPerpBook::new();
        let mut ws = PerpWriteSet::default();
        book.store_bytes(k(1), vec![0xAA], &mut ws);
        book.store_bytes(k(1), vec![0xBB], &mut ws);
        book.store_struct(k(2), Box::new(5u32), ser_u32, clone_u32, &mut ws);
        book.with_struct_mut(k(2), &mut ws, |v| *v.downcast_mut::<u32>().unwrap() = 7);
        book.store_bytes(k(3), vec![1, 2, 3], &mut ws);
        let shared_delta = book.take_delta();

        assert_eq!(serial_delta, shared_delta);
        // Pin the expected net delta too.
        assert_eq!(shared_delta.get(&k(1)), Some(&vec![0xBBu8]));
        assert_eq!(shared_delta.get(&k(2)), Some(&7u32.to_le_bytes().to_vec()));
        assert_eq!(shared_delta.get(&k(3)), Some(&vec![1u8, 2, 3]));
    }
}
