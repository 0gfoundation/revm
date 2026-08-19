//! Per-block chained off-trie commitment (catalog #16d): the off-trie analogue of the
//! state root. The precompile shell folds this once at block end and anchors the result
//! in 0x1003's single on-trie storage slot.

use context_interface::journaled_state::PerpDelta;
use primitives::{B256, U256};

/// Version byte mixed into the per-block commitment hash (catalog #16d). Bumped to 3 at the
/// switch from the per-call chained v2 (retired) to the per-block net-delta fold; bumped to 4
/// at the switch from keccak-derived storage keys to direct-packed keys (catalog #12), so the
/// two key framings never alias across the consensus transition (a devnet wipe accompanies the
/// bump); bumped to 5 at the price-index switch from sorted `Vec<u64>` to `Vec<u64>`
/// (catalog #22) — the serialized price-level bytes change (container + order); bumped to 6 at the
/// order-lifecycle redesign (commit-only #23): delete-on-terminal removes filled/cancelled orders
/// from the map, the new per-level live-order count + lazy FIFO change the level-key set, and the
/// signed-order replay guard moved to the seen-signature namespace — all shift the net-delta bytes;
/// bumped to 7 grouping the five per-market hot scalars (mark price, best bid/ask, last traded,
/// open interest) into one `MarketHot` blob — the delta keys + framing regroup (values identical);
/// bumped to 8 folding the two per-user scalar keys (fee-rate bps, order nonce) into the account
/// blob — the account blob grows and their standalone keys disappear from the delta; bumped to 9
/// moving mark_price out of `MarketHot` into the `Market` blob (write-rare + co-read with config) —
/// both blobs' bytes change (Market gains a field, MarketHot loses one); bumped to 10 folding each
/// level's live-order count INTO its FIFO blob (`LevelBlob`, count(8 BE) prefix) — the per-level
/// count keys disappear and the level blob framing changes; bumped to 11 when total perp
/// collateral was appended to the account blob.
// #A: bumped 11→12 for the PerpPosition reservation-aggregate fields (tbq/tbn/tsq/tsn) — a CHAIN
// change (position blob layout changed → persisted state + commitment differ). Requires a golden
// re-pin (below) + a coordinated wipe on deploy.
// Margin tiers (Phase 1): bumped 13→14 for the `tiers` field appended to the `Market` blob — every
// stored market blob grows by its (default, single-tier) table. CHAIN change; behaviour is
// arithmetically identical (mmr = 1/(2*3) = 1/6 == the deleted MAINTENANCE_MARGIN_DENOMINATOR).
// fee_reserved removal: bumped 14→15 — `PerpPosition` lost its "fr" field.
// Isolated funding (A1): bumped 15→16 — funding now settles against `pos.margin` instead of the
// account-global perp wallet, so the same funding event writes a different position/account value
// set (and `liquidate_position` no longer writes the account at all). No layout change; this is an
// EXECUTION-RULE change, so the values folded into the delta differ. CHAIN change: golden re-pin
// (below) + a coordinated wipe on deploy.
// Per-user market index (derived-ooIM Phase 0): bumped 16→17 — a NEW off-trie namespace
// ("umkt", per-user set of the markets the user is active in) enters the block delta whenever a
// user enters or leaves a market. Purely ADDITIVE: no existing blob's layout or value changes and
// no execution rule moves (the golden BusinessSnapshot is unchanged), but the delta gains keys, so
// the commitment shifts. CHAIN change: golden re-pin (below) + a coordinated wipe on deploy.
// Derived open-order margin (Phase 2): bumped 17→18 — the open-order margin ESCROW is DELETED.
// `PerpPosition` loses six fields ("mr", "mrn", "br", "brn", "sr", "srn"), so every stored
// position blob shortens and its later fields shift; and placement/cancel/fill/setLeverage no
// longer move the wallet for a reservation, so the account values folded into the delta differ
// too. Both a LAYOUT and an EXECUTION-RULE change. The open-order requirement is now DERIVED on
// read (`ooIM = ROUND_UP(max(|N + Bid|, |N − Ask|) / L) − ROUND_UP(|N| / L)`) and merely
// subtracted at the admission gate. CHAIN change: golden re-pin (below) + a coordinated wipe on
// deploy.
// Assuming-Price sell side + M1′ maker fills: bumped 18→19 — TWO execution-rule changes, no layout
// change. (1) `Ask` is now priced at each sell's ASSUMING PRICE `max(ROUND_UP(lastTraded × 1.0015),
// mark, limit)` instead of its limit price, so admission accepts/refuses a different set of resting
// sells (`math::assuming_price_floor`). (2) A maker fill whose wallet cannot cover the opening margin
// now FILLS and drives `perp_wallet_balance` negative instead of being cancelled, so the write set of
// such a match differs (a position + a negative wallet where there used to be a cancelled order).
// Both change what nodes write, so a node on 18 and a node on 19 would diverge on state — which is
// exactly what this version guards. CHAIN change: golden re-pin (below) + a coordinated wipe on
// deploy.
// M1 maker fills (the deficit lands in the SILO, not the wallet): bumped 19→20 — ONE execution-rule
// change, no layout change. An underfunded maker fill now funds its opening leg with
// `min(opening_value / L, cash at hand)` and lets `pos.margin` be SHORT by the rest
// (`settlement::OpeningMarginFunding::CappedAtCashAtHand`), where 19 funded the silo in full and
// drove `perp_wallet_balance` NEGATIVE by the shortfall (model M1′, which 19 adopted from a
// conjecture the docs marked ~50/50). R11 measured Binance doing the former
// (`derived-ooim-plan.md` §3a: `isolatedWallet == W0 + realized` digit-for-digit against a higher
// IM-implied figure). The write set of such a match differs — a thinner position blob and a
// non-negative account blob where 19 wrote a full silo and a negative wallet — and the divergence
// PROPAGATES: the thinner silo absorbs less of a later close's loss, so a different amount reaches
// the insurance fund. A node on 19 and a node on 20 would disagree on state, which is exactly what
// this version guards. CHAIN change: golden re-pin (below) + a coordinated wipe on deploy. (The
// golden SCENARIO never underfunds a maker, so its write set is byte-identical at 19 and 20 and its
// BusinessSnapshot is unchanged; the golden value moves only because this byte is hashed into it.)
// Frozen per-order Assuming Price (R12): bumped 20→21 — BOTH a layout and an execution-rule change.
// `OrderEntry` gains a trailing "ap" field (`assuming_price`), so EVERY per-user order-list blob
// grows by one integer; and a resting order's contribution to `Bid`/`Ask` is now the value frozen
// when it was placed instead of being re-derived at every read from the CURRENT
// `T = max(ROUND_UP(lastTraded × 1.0015), mark)`. `total_sell_notional` therefore carries the
// markup (it was the limit-price baseline before), which changes the value written for any user with
// a marked-up resting sell, and changes admission arithmetic: a later print or mark move no longer
// re-prices an order that is already resting. MEASURED — `misc/binance-flip-and-admission.md` §3.13
// (R12): 90 frames, the reported `askNotional` never moved, `H_live` refused by 1939 quanta with 9
// consecutive frames below the kink. `N = |position| × mark` stays LIVE, so `ooIM` still moves with
// the mark (R10) — only the per-order terms froze. A node on 20 and a node on 21 would disagree on
// both the blob bytes and the admission verdict, which is exactly what this version guards. CHAIN
// change: golden re-pin (below) + a coordinated wipe on deploy.
pub const BLOCK_COMMITMENT_VERSION: u8 = 21;

/// Computes the per-BLOCK off-trie commitment over the block's NET delta (catalog #16d).
///
/// `C_block = blake3(C_prev ‖ BLOCK_COMMITMENT_VERSION ‖ Σ_sorted(key(32) ‖ len(u32 BE) ‖ value))`,
/// keys ascending. `delta` is the net block writes from [`JournalTr::take_perp_delta`] (already one
/// value per key, post-revert), so no coalescing is needed — only deterministic key-sorting (keys
/// are a total order; the HashMap is never iterated for the hash). An empty value is a deleted key,
/// framed with len 0 (same convention as the per-call path). Pure: the caller reads `C_prev` and
/// sstores the result. This commits the block's net STATE CHANGE; chained onto the previous block's
/// `C` it forms a block-granular running commitment, the off-trie analogue of the state root.
pub fn compute_block_commitment(prev: U256, delta: &PerpDelta) -> U256 {
    let mut keys: Vec<&B256> = delta.keys().collect();
    keys.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&prev.to_be_bytes::<32>());
    hasher.update(&[BLOCK_COMMITMENT_VERSION]);
    for key in keys {
        // Fold the canonical BYTES only — `decoded` never feeds the commitment (选项A), so the
        // byte-stream (and the on-trie anchor) is identical to the pre-Arc pipeline.
        let value = &delta[key].bytes;
        hasher.update(key.as_slice());
        hasher.update(&(value.len() as u32).to_be_bytes());
        hasher.update(value);
    }
    U256::from_be_bytes(*hasher.finalize().as_bytes())
}

