//! Canonical blob codec: positional msgpack for struct blobs, plus the raw (non-msgpack)
//! price-level codec. Every byte produced here feeds the chained block commitment —
//! field order in [`crate::types`] is layout-significant.

use serde::{Deserialize, Serialize};

use crate::error::{perp_err, PerpError};
use rmp_serde::{Deserializer as RMPDeserializer, Serializer as RMPSerializer};

// ── Generic msgpack helpers ───────────────────────────────────────────────────

pub fn encode<T: Serialize>(val: &T) -> Result<Vec<u8>, PerpError> {
    let mut buf = Vec::new();
    // Positional (array) msgpack: struct field NAMES are NOT serialized (P4/#20) — smaller blobs
    // and faster decode; the decoder reads fields by position via serde's `visit_seq`. Cross-
    // version blob compatibility is intentionally dropped (the chain is wiped on any encoding
    // change anyway, since blob bytes feed the on-trie commitment), so field ORDER in `types/` is
    // now layout-significant — only append fields, never reorder/insert.
    val.serialize(&mut RMPSerializer::new(&mut buf))
        .map_err(|_| perp_err("msgpack encode error"))?;
    Ok(buf)
}

pub fn decode<T: for<'de> Deserialize<'de>>(buf: &[u8]) -> Result<T, PerpError> {
    let mut de = RMPDeserializer::new(buf);
    Deserialize::deserialize(&mut de).map_err(|_| perp_err("msgpack decode error"))
}


pub fn unpack_order_ids(buf: &[u8]) -> Result<Vec<[u8; 32]>, PerpError> {
    if buf.len() % 32 != 0 {
        return Err(perp_err("corrupt order-id queue blob"));
    }
    Ok(buf
        .chunks_exact(32)
        .map(|chunk| {
            let mut id = [0u8; 32];
            id.copy_from_slice(chunk);
            id
        })
        .collect())
}

/// A price level's FIFO + its live-order count, in ONE blob (count folded in — they are written
/// together on rest/match, so this halves the per-level keys in the delta/commitment and lets the
/// match walk read ids + count in one probe). `count` = LIVE (Open/PartiallyFilled) orders; `ids`
/// = the FIFO (live + lazily-swept stale). Packed raw as `count(8 BE) ++ id0(32) ++ id1(32) ...`
/// (no msgpack, P4/#20 compactness). `count == 0` means the level is EMPTY → packs to an empty buf
/// (the delete convention); stale `ids` are discarded on empty.
#[derive(Clone, Debug, Default)]
pub struct LevelBlob {
    pub count: u64,
    pub ids: Vec<[u8; 32]>,
}

/// Packs a [`LevelBlob`]: empty when the level is empty (count 0) → delete; else count prefix + ids.
/// Public so the typed store's `take_delta` (and the engine) reproduce the RAW (non-msgpack) level codec.
pub fn pack_level(b: &LevelBlob) -> Vec<u8> {
    if b.count == 0 {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity(8 + b.ids.len() * 32);
    buf.extend_from_slice(&b.count.to_be_bytes());
    for id in &b.ids {
        buf.extend_from_slice(id);
    }
    buf
}

/// Inverse of [`pack_level`]. Empty buf → empty level (count 0, no ids).
pub fn unpack_level(buf: &[u8]) -> Result<LevelBlob, PerpError> {
    if buf.is_empty() {
        return Ok(LevelBlob::default());
    }
    if buf.len() < 8 {
        return Err(perp_err("corrupt level blob (short)"));
    }
    let count = u64::from_be_bytes(buf[..8].try_into().unwrap());
    let ids = unpack_order_ids(&buf[8..])?;
    Ok(LevelBlob { count, ids })
}

