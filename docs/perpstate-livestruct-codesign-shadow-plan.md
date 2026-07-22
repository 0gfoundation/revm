# PerpState live-struct + dirty-set co-design — shadow-phase change-list

**Date:** 2026-07-22 · **Branch:** perpdex · **Status:** plan (not started)

## Why this exists

The off-trie perp overlay `PerpSection.working: HashMap<B256, PerpEntry>` conflates **two jobs**:

1. **In-block store** (read-your-own-write + overlay-vs-committed precedence).
2. **The per-block commitment write-set** — `take_delta` drains `working` to a `PerpDelta`, which `compute_block_commitment` folds into the on-trie 0x1003 anchor (→ `state_root`, every block).

Job 2 is *why* the store is `B256`-keyed and type-erased (`dyn Any` + ser/clone fn-ptrs): `take_delta` must emit `(B256, canonical bytes)` pairs. That forces **every** `load`/`save` onto the expensive path (measured: ~4 hashbrown probes on the same 32-byte key + downcast + clone per load = the 180/700 ns).

**The per-block commitment stays** (a periodic-only hash is a CometBFT safety fork — verified, wf_f70f5be1). But the commitment only *minimally* needs, at block end: `{keys touched this block} × {final canonical bytes}`, sorted, folded. It does **not** need that set maintained as a type-erased value-store probed on every access. So split the two jobs: a **live typed store** (hot, direct `&mut`) + a **cheap append-only dirty-key list** (the commitment's real input).

## Invariants that make it safe (established this session — verify before relying)

- **Write choke points:** every delta-eligible entry (`Struct`/`Bytes`) enters `working` via **exactly two** methods — `PerpSection::store_struct` (inner.rs:172) and `PerpSection::store_bytes` (inner.rs:163). `get_struct_mut` (inner.rs:147) only re-touches an already-stored `Struct`; `cache_put` inserts `Cached` (excluded from the delta). `working` is `mem::take`-drained each block (take_delta, inner.rs:189), so it starts empty → any `Struct`/`Bytes` present this block entered via those two methods this block.
  ⇒ **delta key-set == set of keys passed to store_struct/store_bytes this block (dedup'd).**
- **Commitment fold** (storage/mod.rs:230, `compute_block_commitment`):
  `C_block = blake3(C_prev ‖ VERSION(1 byte) ‖ Σ_sorted( key(32) ‖ len(u32 BE) ‖ bytes ))`, keys ascending, **empty bytes (len 0) = deleted key**. Input = `PerpDelta = HashMap<B256, PerpDeltaEntry{decoded, bytes}>`; **only `bytes` feeds the hash** (`decoded` never does — 选项A). `VERSION` = `BLOCK_COMMITMENT_VERSION` = 10.
- **Persistence** consumes the SAME delta, co-committed in the same MDBX txn as EVM state (0g-reth) → `durable_perp_head == durable_evm_head`. Must stay.

## Phase 1 — Shadow (golden-neutral, NO store change)

Add a parallel dirty-key list and assert it reproduces the overlay-drained delta. Proves capture completeness + materialization byte-identity **before** any storage change. All edits in `crates/context/src/journal/inner.rs`, `PerpSection`.

1. **Field:** `dirty_keys: Vec<B256>` on `PerpSection` (next to `write_count` / `dirty_this_tx`).
2. **Push** `key` in `store_struct` and `store_bytes` — right where `self.write_count += 1` sits (inner.rs:166, :182). (Pushing in `get_struct_mut` too is harmless but redundant — dedup covers it.)
3. **Assert in `take_delta`** (inner.rs:189), before `mem::take(&mut self.working)`:
   - (a) *Key-set check:* build `dedup(sort(self.dirty_keys))` and `debug_assert_eq!` it against the `Struct`/`Bytes` keys of `working`. Catches any write bypassing the two choke points + the key-level coalescing.
   - (b) *Materialization check (stronger, optional but recommended):* build a `PerpDelta` **entirely from `dirty_keys`** — for each unique key read `working.get(key)`, serialize (`Struct`→`ser`, `Bytes`→clone), tombstone on empty — and `debug_assert_eq!` its `bytes` map against the drain-based delta. This validates the **phase-2 producer** byte-for-byte while the store is still the overlay.
   - Then clear `self.dirty_keys` (like `dirty_this_tx = false`). Production still returns the drain-based delta.
4. **Run full battery.** Golden unchanged (delta identical; asserts only). This is the cheap safety net.

## Phase 2 — Store swap (the actual win; golden-neutral if canonical bytes preserved)

Replace type-erased `working` values with a typed live store; `dirty_keys` becomes the primary delta source.

- **`PerpStore`** typed maps (accounts / positions / markets / levels / orders …). Two shapes:
  - *(a) enum-in-place:* keep it in `JournalInner` but store `enum PerpTyped { Account(UserAccount), Position(PerpPosition), Market(Market), Level(LevelBlob), … }` — kills `dyn Any` without relocating out of the generic revm-context crate.
  - *(b) relocate:* move the store to a concrete slot the precompile owns (fully typed, arena/`SlotMap` + `HashMap<Identity, Handle>` for one-probe-per-entity-per-tx). More invasive; enables Vec-index repeated access.
- **load/save/mutate** → direct typed access (resolve-once: one identity→handle probe per entity per tx, then `&mut` / Vec index). No B256 hashing, no downcast, no per-write serialize.
- **Writes push key (or handle) to `dirty_keys`.**
- **`take_delta` materializes from `dirty_keys`:** dedup → per key `canonical_key` (B256, direct construct per #12) + `canonical_bytes` (canonical pack/msgpack) or tombstone → `PerpDelta`. Feeds `compute_block_commitment` (**unchanged**) + persistence (**unchanged, same MDBX txn**).
- **Deletions:** on delete push key + mark removed; materialization emits empty bytes (len 0). **Centralize this one rule** (don't re-derive "present-default == removed" at ~25 sites).
- **RPC isolation:** RPC sim paths (eth_estimateGas binary-search, eth_call/trace/debug) re-execute on the shared read handle → they get a **CoW view** over the live store, dropped after the call; their `dirty_keys` discarded (never committed). The consensus execution (payload_validator execute_block) gets `&mut` canonical.
- **Restart-determinism:** handles NEVER feed the hash — only content-addressed B256 keys + canonical bytes do. (Interned-slot layout is restart-unstable → would be a latent consensus split.)

**Golden-neutral iff** the materialized sorted `(key, bytes)` stream equals today's. Phase-1 check (b) is exactly the pre-validation of this.

## The single migration hazard

A write path that doesn't reach `store_struct`/`store_bytes` (phase 1) or doesn't push to `dirty_keys` (phase 2) → a missing delta entry → golden diverges → consensus split. Phase-1 assert (a) catches phase-1; phase-2 relies on the golden battery + phase-1 check (b).

## What is explicitly KEPT (do not delete)

- The **EVM journal** itself — perp still writes USDC (journaled sstore), the 0x1003 anchor (journaled sstore), and ALL events (40+ `journal.log()`), which need real intra-tx `checkpoint_revert` (unlike commit-only perp).
- A **slim P3 tripwire** (`write_count` + `dirty_this_tx` assert) — the only runtime detector that validate-then-apply holds across ~55 write-then-error paths.
- **Content-addressed B256 keys + sort-at-hash**; the per-block commitment cadence + the same-MDBX-txn atomic persistence.
