//! Storage helpers for the PerpDEX precompile.

pub use perp_core::keys;

use crate::host::PerpHost;
use context_interface::journaled_state::PerpDelta;
use context_interface::JournalTr;
use primitives::{Address, B256, U256};
use serde::Deserialize;

use crate::PERP_DEX_ADDRESS;
use crate::{
        errors::{perp_err, perp_invariant_err},
    types::{
        ApiKey, FundingState, IndexPriceHistory, IndexPriceState, Market, MarketHot, Order,
        OrderEntry, PerpPosition, PremiumIndexAccumulator, PriceBasisWindow, UserAccount,
        UserFeeRates, MAX_USER_MARKETS,
    },
    PerpError,
};

use keys::{
    account_key, admin_key, api_key_ids_key, api_key_key, ask_level_key, ask_prices_key,
    bid_level_key, bid_prices_key, commitment_slot, funding_state_key,
    index_price_history_key, index_price_state_key, insurance_fund_key, market_fee_total_key,
    market_hot_key, market_key, market_manager_key, oracle_key, order_key, position_key,
    position_registry_key, premium_accumulator_key, price_basis_window_key, seen_bucket_key,
    seen_sig_key, trade_count_key, user_buy_orders_key, user_markets_key, user_sell_orders_key,
};

// Canonical codec + block commitment live in `perp_core`; re-exported so in-crate callers
// (and the bench harness) keep their `storage::encode`-style paths.
pub use perp_core::codec::{encode, decode, pack_level, unpack_level, unpack_order_ids, LevelBlob};
pub use perp_core::commitment::{compute_block_commitment, BLOCK_COMMITMENT_VERSION};

/// Test-only instrumentation wrapped around the single off-trie blob read/write choke
/// (`load_blob`/`store_blob`). Quantifies the ser/deser VOLUME of a run and the redundancy that a
/// per-block deserialized-struct cache (catalog #14) or block-end serialization (#16d) would
/// collapse — measurable WITHOUT implementing either. Disabled by default (only the block-level
/// perf bench `enable()`s it around the timed block) and compiled out of production via
/// `cfg(test)`, so it adds ZERO hot-path cost on chain. Single-threaded use only (perf bench runs
/// `--test-threads=1`); state is thread-local.
#[cfg(test)]
pub(crate) mod bench_counter {
    use super::B256;
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;

    #[derive(Default, Clone)]
    pub(crate) struct Stats {
        /// `load_blob` calls (each is one potential deser; empty/absent reads decode to a default).
        pub read_calls: u64,
        pub read_bytes: u64,
        /// Distinct keys read — a per-block deser cache (#14) collapses `read_calls` → this.
        pub read_keys: HashSet<B256>,
        /// `store_blob` calls (each is one serialize by the typed `save_*` helper).
        pub write_calls: u64,
        pub write_bytes: u64,
        /// Distinct keys written — block-end serialization (#16d) collapses `write_calls` → this.
        pub write_keys: HashSet<B256>,
        /// Distinct (txn, key) pairs — a per-call cache (#14) collapses `write_calls` → this
        /// (still one serialize per written key per txn, for that txn's commitment fold).
        pub write_txn_keys: HashSet<(u32, B256)>,
        /// #16d block-end serializations: `ser_blob` calls during `take_perp_delta` — one per
        /// distinct struct key for the whole block. With deferral active these REPLACE the typed
        /// helpers' per-write `store_blob` serializations (which no longer hit the byte choke), so
        /// `write_calls` drops to the raw byte-path writers (level queues) and serialization is
        /// `block_end_ser_calls`.
        pub block_end_ser_calls: u64,
        pub block_end_ser_bytes: u64,
    }

    thread_local! {
        static STATS: RefCell<Stats> = RefCell::new(Stats::default());
        static TXN: Cell<u32> = const { Cell::new(0) };
        static ON: Cell<bool> = const { Cell::new(false) };
        static FORCE_PERCALL: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) fn reset() {
        STATS.with(|s| *s.borrow_mut() = Stats::default());
        TXN.with(|t| t.set(0));
    }
    pub(crate) fn enable() {
        ON.with(|o| o.set(true));
    }
    pub(crate) fn disable() {
        ON.with(|o| o.set(false));
    }
    /// When set, `save_cached`/`load_cached` bypass the #16d struct overlay + #14 read cache and
    /// serialize on every write / deserialize on every read (the pre-#14/#16d behavior). Lets one
    /// bench measure per-call vs deferred ser/deser on an identical workload. Independent of
    /// `enable()` (it changes BEHAVIOR, not counting); set explicitly per pass, not cleared by `reset`.
    pub(crate) fn set_force_percall(v: bool) {
        FORCE_PERCALL.with(|c| c.set(v));
    }
    pub(crate) fn force_percall() -> bool {
        FORCE_PERCALL.with(Cell::get)
    }
    /// Marks the start of a new logical transaction (handler call) for per-call write attribution.
    pub(crate) fn next_txn() {
        TXN.with(|t| t.set(t.get().wrapping_add(1)));
    }
    pub(crate) fn snapshot() -> Stats {
        STATS.with(|s| s.borrow().clone())
    }

    pub(crate) fn record_read(key: B256, bytes: usize) {
        if !ON.with(Cell::get) {
            return;
        }
        STATS.with(|s| {
            let mut s = s.borrow_mut();
            s.read_calls += 1;
            s.read_bytes += bytes as u64;
            s.read_keys.insert(key);
        });
    }

    pub(crate) fn record_write(key: B256, bytes: usize) {
        if !ON.with(Cell::get) {
            return;
        }
        let txn = TXN.with(Cell::get);
        STATS.with(|s| {
            let mut s = s.borrow_mut();
            s.write_calls += 1;
            s.write_bytes += bytes as u64;
            s.write_keys.insert(key);
            s.write_txn_keys.insert((txn, key));
        });
    }

    /// Records one block-end serialization (`ser_blob` in `take_perp_delta`).
    pub(crate) fn record_block_end_ser(bytes: usize) {
        if !ON.with(Cell::get) {
            return;
        }
        STATS.with(|s| {
            let mut s = s.borrow_mut();
            s.block_end_ser_calls += 1;
            s.block_end_ser_bytes += bytes as u64;
        });
    }
}

/// Reads an off-trie PerpDEX blob ("PerpState").
///
/// Returns the in-block overlay value if the key was written during this block, otherwise the
/// committed off-trie store. This replaces the previous chunked `sstore`/`sload` blob, which
/// lived in the state trie under `PERP_DEX_ADDRESS`; perp data now rides the journal's perp
/// section instead, so it gets the same revert lifecycle but never enters the state root.
/// See `docs/perpstate-journal集成方案.md` §4.2.
fn load_blob<H: PerpHost>(context: &mut H, key: B256) -> Result<Vec<u8>, PerpError> {
    let buf = context.perp_load(key)?;
    #[cfg(test)]
    bench_counter::record_read(key, buf.len());
    Ok(buf)
}

/// Writes an off-trie PerpDEX blob. An empty `buf` marks the key absent. The write is journaled
/// in the perp section (reverts in lock-step with the surrounding checkpoint / `discard_tx`) and
/// is never folded into the trie-bound `EvmState`.
///
/// #16d: the on-trie commitment is NO LONGER updated per write. It is folded ONCE at block end from
/// the net delta by [`finalize_block_commitment`] (which the block executor calls after
/// `take_perp_delta`), so a key written N times across the block is hashed once. This replaces the
/// former per-call `flush_commitment` over a per-write framed log.
fn store_blob<H: PerpHost>(
    context: &mut H,
    key: B256,
    buf: &[u8],
) -> Result<(), PerpError> {
    #[cfg(test)]
    bench_counter::record_write(key, buf.len());
    context.perp_store(key, buf.to_vec());
    Ok(())
}

/// Block-end hook (catalog #16d): folds the block's net perp delta into the on-trie 0x1003 anchor
/// ONCE, replacing the per-call [`flush_commitment`]. The block executor calls this after
/// [`JournalTr::take_perp_delta`], while the journal is still alive. No-op on an empty delta.
/// `warm_account` + `touch_account` mirror `flush_commitment` / `save_erc20_balance`.
///
/// This writes a journaled `sstore` into the journal overlay ONLY — it does NOT itself reach the
/// `State`/bundle. Because the block executor's `finish`/`into_db` merely extract
/// `journaled_state.database` (dropping the overlay), the caller MUST drain the journal afterwards
/// (`JournalTr::finalize`) and commit the returned changeset into its `State` DB so the slot change
/// becomes a bundle transition. The alloy-evm `Evm::finalize_perp_commitment` wrapper does exactly
/// that. (Pre-#16d this ran inside a tx, so the enclosing `transact`+commit carried it; the
/// block-end call site has no such enclosing commit.) Journaled like any sstore.
pub fn finalize_block_commitment<CTX: context_interface::ContextTr>(
    context: &mut CTX,
    delta: &PerpDelta,
) -> Result<(), PerpError> {
    if delta.is_empty() {
        return Ok(());
    }
    context
        .journal_mut()
        .warm_account(PERP_DEX_ADDRESS)
        .map_err(|e| perp_err(format!("Database error: {e:?}")))?;
    let c_prev = context
        .journal_mut()
        .sload(PERP_DEX_ADDRESS, commitment_slot().into())
        .map_err(|e| perp_err(format!("Database error: {e:?}")))?
        .data;
    let c_new = compute_block_commitment(c_prev, delta);
    context
        .journal_mut()
        .sstore(PERP_DEX_ADDRESS, commitment_slot().into(), c_new)
        .map_err(|e| perp_err(format!("Database error: {e:?}")))?;
    context.journal_mut().touch_account(PERP_DEX_ADDRESS);
    Ok(())
}

// ── Admin ─────────────────────────────────────────────────────────────────────

// ── (Stage C) type-erased Struct-tier read/write machinery REMOVED ──────────────
// `load_cached` / `save_cached` (the #14-cached deferred-`Struct` reader/writer) and their
// `ser_blob` / `clone_blob` fn-pointers are gone: every hot namespace reads/writes the strongly-
// typed live store, and every cold namespace uses the byte tier (`load_blob` / `store_blob`). The
// journal's `PerpEntry::Struct`/`Cached`, the #14 cache, and the `perp_get_struct*`/`perp_cache_*`/
// `perp_store_struct` seam are deleted in revm-context alongside this. The cross-block decoded read
// (`perp_load_arc`, 选项A) survives — the typed store's cold path uses it (see `cold_load`).

/// Returns `Address::ZERO` when no admin has been initialised yet.
pub fn load_admin<H: PerpHost>(context: &mut H) -> Result<Address, PerpError> {
    // Cold/admin-cadence namespace on the cheap BYTE tier (load_blob/store_blob), like the other
    // role/index/funding namespaces — NOT the type-erased Struct tier (that machinery is deleted in
    // Stage C). Byte-identical canonical bytes (`encode(Address)`), so golden-neutral.
    let buf = load_blob(context, admin_key())?;
    if buf.is_empty() {
        return Ok(Address::ZERO);
    }
    decode(&buf)
}

pub fn save_admin<H: PerpHost>(
    context: &mut H,
    admin: Address,
) -> Result<(), PerpError> {
    let buf = encode(&admin)?;
    store_blob(context, admin_key(), &buf)
}

// ── UserAccount ───────────────────────────────────────────────────────────────

// ── Typed live store (Stage B cutover) ────────────────────────────────────────
// The `acct` namespace is the first to run on the strongly-typed `TypedPerpStore` instead of the
// type-erased overlay: reads/writes hit a typed sub-map (one whole-store downcast + one natural-key
// probe) instead of the 4-probe B256 overlay dance. Canonical bytes (same `encode`) and delta
// key-set semantics are unchanged, so the block commitment is byte-identical (golden-neutral).

/// Per-op handle to the typed live store: installs it on first touch, then downcasts the opaque
/// journal slot (whole-store erasure — one `TypeId` compare, not per-blob).
fn typed_store_mut<H: PerpHost>(context: &mut H) -> &mut crate::typed_store::TypedPerpStore {
    context.live()
}

/// Generic cold read for a cutover namespace: cross-block decoded store (Arc, zero clone) →
/// committed bytes (decode once). Mirrors `load_cached`'s tier 2+3. The caller fills its typed
/// sub-map (fill is per-namespace).
fn cold_load<H: PerpHost, T>(
    context: &mut H,
    key: B256,
) -> Result<Option<std::sync::Arc<T>>, PerpError>
where
    T: Send + Sync + 'static + for<'de> Deserialize<'de>,
{
    // 选项A decoded store first (already-decoded Arc, no deserialization).
    let mut arc: Option<std::sync::Arc<T>> = context
        .perp_load_arc(key)?
        .and_then(|any| std::sync::Arc::downcast::<T>(any).ok());
    // Bytes fallback (overlay bytes or committed store) → decode once.
    if arc.is_none() {
        let buf = load_blob(context, key)?;
        if !buf.is_empty() {
            arc = Some(std::sync::Arc::new(decode::<T>(&buf)?));
        }
    }
    Ok(arc)
}

/// Cold path for the `acct` namespace: [`cold_load`] + fill (cache semantics, no dirty mark;
/// absence cached too).
fn cold_fill_account<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<Option<std::sync::Arc<UserAccount>>, PerpError> {
    let arc = cold_load::<_, UserAccount>(context, account_key(user))?;
    typed_store_mut(context).fill_account(user, arc.clone());
    Ok(arc)
}

/// Cold path for the `mkt` namespace (see [`cold_fill_account`]).
fn cold_fill_market<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<Option<std::sync::Arc<Market>>, PerpError> {
    let arc = cold_load::<_, Market>(context, market_key(market_id))?;
    typed_store_mut(context).fill_market(market_id, arc.clone());
    Ok(arc)
}

/// Cold path for the `pos` namespace (see [`cold_fill_account`]).
fn cold_fill_position<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<Option<std::sync::Arc<PerpPosition>>, PerpError> {
    let arc = cold_load::<_, PerpPosition>(context, position_key(user, market_id))?;
    typed_store_mut(context).fill_position(user, market_id, arc.clone());
    Ok(arc)
}

pub fn load_account<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<UserAccount, PerpError> {
    use crate::typed_store::Resident;
    match typed_store_mut(context).account(user) {
        Resident::Hit(a) => return Ok(a.clone()),
        Resident::Deleted => return Ok(UserAccount::default()),
        Resident::Miss => {}
    }
    Ok(cold_fill_account(context, user)?
        .map(|a| (*a).clone())
        .unwrap_or_default())
}

/// Zero-copy account read (点1): `Arc<UserAccount>`, no per-read clone. Defaulted like
/// [`load_account`]. PURE reads only (availability checks). Debit/credit RMW keep [`load_account`]
/// + [`save_account`].
pub fn load_account_ref<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<std::sync::Arc<UserAccount>, PerpError> {
    use crate::typed_store::Resident;
    // Hot path: resident Arc, refcount bump only.
    if let Some(arc) = typed_store_mut(context).account_arc(user) {
        return Ok(arc);
    }
    // Deleted (definitively absent) must NOT fall through to the stale committed store.
    if matches!(typed_store_mut(context).account(user), Resident::Deleted) {
        return Ok(std::sync::Arc::new(UserAccount::default()));
    }
    Ok(cold_fill_account(context, user)?
        .unwrap_or_else(|| std::sync::Arc::new(UserAccount::default())))
}

/// ── THE `AccountBalanceChanged` TRIGGER: WHICH WRITE HAPPENED ──────────────────────────────────
///
/// Marks `user` as owing an account snapshot from the end-of-call drain
/// ([`flush_account_snapshots`]). Nothing is logged here.
///
/// # ⚠️ The drain is now the FALLBACK, not the only emitter
///
/// The trading paths publish DIRECTLY, at the economic event, and clear the mark this function set
/// (see [`clear_account_snapshot_mark`]):
///
/// | party | granularity | emitted by |
/// |---|---|---|
/// | a filled MAKER | one per FILL (= one per maker order per sweep) | the match flush, from the registry working copy |
/// | a TAKER | one per ORDER | `trading::mod::match_order`, after `finalize_apply`, off the settled store |
/// | everyone else | coalesced, one per user per call | THIS mark + the drain |
///
/// "Everyone else" is deposit/withdraw/transfer, funding, `setLeverage`, liquidation and ADL — plus
/// the fee recipient, who is an incidental party to a fill rather than a counterparty (one row per
/// fill for a pure fee sink is noise, and its wallet move is not named by any `Trade` or
/// `PositionChanged`). The marking rules below are unchanged and still govern the drain; what
/// changed is that a maker no longer waits for the end of a transaction it never participated in.
///
/// # The trigger set (strict Binance alignment — choice A)
///
/// A user is marked at, and only at, a write that **moved the wallet or a position's stored state**:
///
/// | write | marks | why |
/// |---|---|---|
/// | [`save_account`] | ✅ | the wallet moved |
/// | [`mutate_account_balance`] | ✅ | the wallet moved (`credit_perp` / `debit_perp`) |
/// | [`save_position`] | ✅ | position state moved (`amount`, `v_quote_balance`, `margin`, `leverage`) |
/// | [`save_position_reservation_only`] | ❌ | per-side AGGREGATES only — `Bid`/`Ask`, i.e. `Σ ooIM` |
/// | [`mutate_account`] | ❌ | nonce bump / fee-rate update; provably cannot move a balance |
///
/// The last two lines are the whole point: **a pure placement and a pure cancel publish NOTHING.**
/// Resting and cancelling touch only `total_buy/sell_{qty,notional}`, and both routes for that go
/// through `save_position_reservation_only`.
///
/// ## The payload agrees with this trigger, field by field
///
/// The published payload is now exactly the set this table can guarantee — three balances:
///
/// * `usdcBalance` = `UserAccount::usdc_balance`, `totalCrossWalletBalance` =
///   `UserAccount::perp_wallet_balance`. Both live on the account blob, so only an account WRITE can
///   move them, and the only account writer that does not mark is [`mutate_account`], whose two call
///   sites set the nonce and the fee-rate pair. Neither balance is reachable from there.
/// * `totalWalletBalance` = `totalCrossWalletBalance + Σ pos.margin`. Both terms now live on the
///   ACCOUNT blob (`Σ pos.margin` is the stored `UserAccount::total_position_margin`), and the only
///   writer of the Σ leg is [`save_position`] (marks), maintaining it from the delta of the position
///   write that moved `pos.margin`; [`save_position_reservation_only`] `debug_assert`s that it does
///   not touch `margin` at all. The per-user market INDEX no longer enters this field, so an
///   order-list leg flipping index membership in [`sync_user_market_membership`] cannot move it even
///   in principle — which is what removed the subtle half of this argument (it used to rest on "a
///   market can only be added or dropped by that leg while the position is flat, and a flat position
///   carries no margin"; that invariant is still asserted, now in [`save_position`] itself, but
///   nothing published depends on it any more).
///
/// This was NOT true of the previous eleven-field payload: `totalOpenOrderInitialMargin` and
/// `availableBalance` move on every placement and cancel, so the event shipped fields its own trigger
/// did not keep fresh. Those seven fields are now `getAccount`-only. The property is pinned by
/// `trading::tests::account_snapshot_events::placement_and_cancel_cannot_move_any_published_field`.
///
/// ## Why silence is right, and why this is the THIRD time the decision moved
///
/// It is not that nothing observable changed — `availableBalance` = `cross − Σ ooIM` really does
/// move when an order rests or is cancelled. It is that we are matching a measured venue:
///
/// * **R14, Binance mainnet, 2026-08-21** (`misc/evidence/binance-run14-user-stream.json`, 18
///   frames): a pure placement pushes **only** `ORDER_TRADE_UPDATE x=NEW`, with **no**
///   `ACCOUNT_UPDATE`; a pure cancel pushes only `x=CANCELED`, again with no `ACCOUNT_UPDATE`. A
///   market fill pushes two `ORDER_TRADE_UPDATE` frames plus exactly one `ACCOUNT_UPDATE(m=ORDER)`.
/// * **The official docs, verbatim**: *"Unfilled orders or cancelled orders will not make the event
///   `ACCOUNT_UPDATE` pushed, since there's no change on positions."*
/// * **Mechanically**, Binance's payload carries `B[].{wb,cw,bc}` and `P[]` — a wallet and a position
///   array. A placement moves neither, which is exactly the condition this table encodes.
/// * `engineering/indexer/websocket-implementation.md`'s **Summary Matrix** (updated to those
///   measurements in docs `3067a82`) therefore reads `OrderPlaced` / `OrderRested` /
///   `OrderCancelled` → `@account`: **No**. Its older per-event mapping table, written the day
///   before and not revisited, still says "Yes if `availableBalance` changes"; the matrix is the
///   newer, measurement-backed side.
///
/// The accepted cost: between fills, a stream-only consumer's `availableBalance` goes STALE, because
/// the `Σ ooIM` term moves with no event. `getAccount` remains the source of truth for it. That is
/// the owner's decision, and it is deliberate — do not "fix" it by emitting on the rest/cancel path
/// again. This is the third flip (silent → emitting in `7019fadd` → silent here), which is why the
/// citation lives in the code next to the filter rather than in a commit message.
///
/// ## Derived from WHICH WRITE, never from comparing values
///
/// There is no before/after comparison anywhere on this path, and there must not be. A value-diff
/// baseline is precisely the machinery that was deleted for cost: a per-account `BTreeMap` of
/// pre-images plus a `PublicAccountBalance` construction per side, each cloning the decimal
/// `usdc_balance` String and parsing it to `U256`, to suppress events a consumer can ignore. The
/// caller always knows which of the five entry points above it chose, so the trigger is free: one
/// `BTreeSet` probe. Emitting for a write whose net effect happened to be zero is the fail-safe
/// direction — a redundant snapshot is harmless, a missing one is not.
///
/// # The drain: one event per marked user per call, emitted last, in address order
///
/// This is what remains of `websocket-implementation.md`'s **Transaction-Level Coalescing** #2
/// (*"Coalesce account updates to one final snapshot per affected user per transaction"*). Marks
/// accumulate in the call-scoped `TypedPerpStore::touched_accounts`; the shell drains them once,
/// in ascending address order, after the dispatch returns `Ok`.
///
/// It is now the FALLBACK for the non-trading paths and the fee recipient (see the table at the top).
/// The transaction was the wrong unit for a fill: the transaction belongs to the TAKER, so coalescing
/// a maker's fills into it made a maker's notification cadence a function of an unrelated party's
/// batching — a 64-item batch that filled maker M on items 3, 17 and 40 gave M one row, at the end of
/// a transaction M never participated in, for three separate economic events.
///
/// Of the three properties the pure-drain design bought, two are unconditional and one had to be
/// re-established per emit point:
///
/// * **A reverted call publishes nothing.** Still unconditional, and for the direct emits it is
///   stronger rather than weaker: they sit past the last genuine reject on their path (a maker's
///   inside the match flush, a taker's after `finalize_apply`), and a revert unwinds the logs with
///   the transaction regardless.
/// * **Deterministic order.** Now three ordered things rather than one: the match walk's fill order,
///   `MatchRegistry`'s `Vec` event replay, and this `BTreeSet`. None is a hash map. Pinned by
///   `trading::tests::account_snapshot_events::the_snapshot_stream_is_deterministic`.
/// * **No half-updated state.** This is the one that is now an argument per emit point, not a
///   consequence of draining last. A taker's snapshot is taken after `finalize_apply` has debited
///   her, so the intermediate that motivated the drain (the funded silo before the wallet paid for
///   it) is still absent. A maker's is derived from the `MatchRegistry` working copy the flush is
///   about to write — settled for that fill, and NOT a partial write of it, which is why the
///   registry's convergence guard exists. A LATER fill for the same maker publishes again; that is
///   a new event, not a correction, and consumers take the last row per user.
///
/// Cost. Each published snapshot costs one
/// [`crate::margin_view::index_account_wallet_balances`] call: **ONE** account `_ref` load, flat, with
/// no walk at all — all three published balances are stored scalars on that blob (`Σ pos.margin` is
/// the maintained `UserAccount::total_position_margin`). It was ≤ 18 loads while that Σ was walked off
/// the per-user market index at one position load per member market, and ≤ 33 before that, while the
/// payload carried the whole margin roll-up at two loads per market. The maker emit is cheaper still:
/// it reads NOTHING, because the working copy already holds the wallet and the Σ is reconstructed from
/// one `i64` captured when the maker joined the registry.
///
/// Against the old per-write emission the filter + drain still remove the placement/cancel row
/// (1 → 0) and the taker's duplicate at her debit; what the granularity change gives back is one row
/// per maker fill and one per taker order instead of one per user per transaction.
///
/// Gas stays FLAT PER SELECTOR — never per event, never dynamic. The per-selector reasoning is on
/// the `SELECTORS` table in [`crate::call`].
#[inline]
fn mark_account_snapshot_dirty<H: PerpHost>(context: &mut H, user: Address) {
    typed_store_mut(context).mark_account_touched(user);
}

/// Drops `user`'s pending mark, so [`flush_account_snapshots`] will not publish for them.
///
/// ⚠️ **Only for a site that already published this user's snapshot DIRECTLY** — per fill for a
/// maker, per order for a taker — and that has established the drain would merely repeat it. The
/// trading paths still go through [`save_position`] / [`save_account`] / [`mutate_account_balance`],
/// so a direct emit does not stop the drain from emitting a second, redundant event; clearing the
/// mark is what does.
///
/// Un-marking is the UNSAFE direction of this switch (a redundant snapshot is harmless, a missing
/// one is not), so both callers gate it on the settled payload being identical to what they
/// published, never on "I emitted something". A LATER economic event for the same user in the same
/// call re-marks them and the drain covers it — which is correct, that is a new event.
#[inline]
pub(crate) fn clear_account_snapshot_mark<H: PerpHost>(context: &mut H, user: Address) {
    typed_store_mut(context).unmark_account_touched(user);
}

/// Publishes ONE `AccountBalanceChanged` for `user` right now, off the SETTLED store, and clears
/// their pending mark so the end-of-call drain does not repeat it.
///
/// This is the taker-side emit point (`trading::settlement::finalize_apply` has just written the
/// last of the taker's state, so "settled" is literal here). Makers cannot use it: their state during
/// a sweep lives in the `MatchRegistry` working copy, not in the store — see
/// `trading::settlement::UserWork::wallet_balances`.
///
/// Clearing unconditionally is sound only because nothing further in the call writes this user's
/// account *for this order*: the placement path's remaining writes are `rest_in_book`'s
/// `save_position_reservation_only` and the nonce's `mutate_account`, neither of which marks (see the
/// trigger table on [`mark_account_snapshot_dirty`]). A later batch item DOES write, re-marks, and is
/// published again — one event per order, as intended.
///
/// # This is also what caps a SELF-MATCH at exactly two rows
///
/// A self-matching user is in the `MatchRegistry`, so the flush evaluates its mark-clear gate for
/// them — and that gate does NOT fire, because the taker leg evolved the same working copy past the
/// maker-leg payload it published. The flush's own `save_position` / `save_account` then re-mark
/// them, and so does `finalize_apply`'s debit. Every one of those is absorbed by the unconditional
/// clear here: the sequence is maker-leg row → (marked, marked, marked) → taker-leg row → cleared,
/// giving **exactly two** rows and no drain row. Verified by removing this line, which makes
/// `trading::tests::account_snapshot_events::a_self_trade_publishes_both_the_maker_leg_and_the_taker_leg`
/// report a third, duplicate row.
pub(crate) fn publish_account_snapshot_now<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<(), PerpError> {
    emit_account_snapshot(context, user)?;
    clear_account_snapshot_mark(context, user);
    Ok(())
}

/// Logs one `AccountBalanceChanged` from an ALREADY-COMPUTED payload, without touching the mark.
///
/// The maker path's emit: the payload is derived from the `MatchRegistry` working copy at fill time
/// (the store is not the maker's state yet), while the mark can only be cleared after the flush's
/// writes have done their marking — so the two halves are necessarily separate, and
/// `MatchRegistry::flush` owns the second one.
pub(crate) fn log_account_snapshot<H: PerpHost>(
    context: &mut H,
    user: Address,
    b: &crate::margin_view::AccountWalletBalances,
) {
    log_account_balance_changed(context, user, b);
}

/// Opens a perp call: drops any un-drained marks from a previous call. See
/// [`crate::typed_store::TypedPerpStore::begin_call`] — the set is CALL-scoped inside a store that is
/// otherwise BLOCK-scoped, and this is what makes it so.
pub fn begin_perp_call<H: PerpHost>(context: &mut H) {
    typed_store_mut(context).begin_call();
}

/// Publishes one `AccountBalanceChanged` per user marked by [`mark_account_snapshot_dirty`] during
/// this call, in **ascending address order**, and clears the set.
///
/// Called by `call::run_perp_dex_call` on the SUCCESS path only, after the dispatch returns `Ok` —
/// so every snapshot is a settled post-transaction state and a reverted call publishes nothing.
/// Top-level by construction: the shell rejects `call_depth() > 1` (commit-only #23, EOA-direct
/// only), so it cannot be re-entered and there is no inner call that could drain early. The
/// liquidation sweep inside `updateIndexPrice`, and the batch drivers, are plain function calls in
/// this same frame — their marks coalesce into this one drain, which is exactly the intent.
///
/// Failure mode, much narrower than it was: the lean fold has exactly ONE narrowing guard left
/// (`totalWalletBalance` must fit `int64`, i.e. a ≥ 9.2e12 USD sum at 6 dp), where the wide roll-up
/// had six of them plus every per-market reject in `compute_margin_info` and the "index holds unknown
/// market" invariant. Such a state still turns into a clean revert of a call whose perp writes have
/// already landed — the same write-then-error exposure the write-site emission had at `save_account`,
/// reported either way by the shell's `perp_write_count` tripwire — but the surface is one guard on a
/// sum of stored scalars instead of the whole derived-margin stack.
///
/// Tests that drive an engine handler directly (`run_place_order`, `storage::save_account`, …) rather
/// than through the shell must call this themselves; nothing else does.
pub fn flush_account_snapshots<H: PerpHost>(context: &mut H) -> Result<(), PerpError> {
    // Taken out of the store first: `emit_account_snapshot` borrows `context` mutably for its fold.
    let touched = typed_store_mut(context).take_touched_accounts();
    for user in touched {
        emit_account_snapshot(context, user)?;
    }
    Ok(())
}

/// Folds and logs one `AccountBalanceChanged` — **the one and only site that constructs this log.**
///
/// All three balances come from [`crate::margin_view::index_account_wallet_balances`], which is now
/// ZERO-WALK: **one account load** and an addition, because all three are stored scalars on that blob
/// (`Σ pos.margin` is [`crate::types::UserAccount::total_position_margin`], maintained by
/// [`save_position`]). The wide `index_account_scalars` roll-up `getAccount` folds is not on this path
/// any more — its seven margin totals are REST-only fields (Binance publishes them on
/// `/fapi/v2/account`, not on `ACCOUNT_UPDATE`), and dropping them from the payload is what let the
/// emit path drop the `Market` load, the maintenance-margin tier walk, the unrealized-PnL arithmetic
/// and the ooIM arithmetic; storing the Σ then removed the index and the per-market position loads
/// too. The two producers must still agree on the fields they share — and now, specifically, the
/// STORED aggregate must equal the wide fold's WALK; the `debug_assertions` cross-check inside
/// `index_account_scalars` compares exactly that on every published snapshot.
fn emit_account_snapshot<H: PerpHost>(context: &mut H, user: Address) -> Result<(), PerpError> {
    let b =
        crate::margin_view::index_account_wallet_balances(context, user, "AccountBalanceChanged")?;
    log_account_balance_changed(context, user, &b);
    Ok(())
}

/// **The one and only site that constructs the `AccountBalanceChanged` log.** Two producers feed it
/// — [`crate::margin_view::index_account_wallet_balances`] off the settled store (the drain and the
/// taker emit) and `trading::settlement::UserWork::wallet_balances` off a match working copy (the
/// per-fill maker emit) — and both hand it the same three-field `AccountWalletBalances`, so there is
/// exactly one definition of the wire payload no matter where the numbers came from.
fn log_account_balance_changed<H: PerpHost>(
    context: &mut H,
    user: Address,
    b: &crate::margin_view::AccountWalletBalances,
) {
    context.log(primitives::Log {
        address: PERP_DEX_ADDRESS,
        data: {
            use alloy_primitives::IntoLogData;
            crate::interface::IPerpDex::AccountBalanceChanged {
                user,
                usdcBalance: b.usdc_balance,
                totalWalletBalance: b.total_wallet_balance,
                totalCrossWalletBalance: b.total_cross_wallet_balance,
            }
            .to_log_data()
        },
    });
}

/// Whole-blob account write — the wallet / fee-rate / nonce legs.
///
/// # ⚠️ It does NOT write `total_position_margin`, and cannot
///
/// `UserAccount::total_position_margin` (`Σ pos.margin`) is owned exclusively by [`save_position`],
/// and this function OVERWRITES whatever the caller's struct carries in that field with the value
/// currently in the store. That is not defensive tidying, it closes a real hazard: several paths
/// (`trading::settlement`'s flush, both `liquidation` legs, `addPositionMargin`,
/// `removePositionMargin`) take an owned `UserAccount` copy, then write a POSITION, then write the
/// account back — so a whole-blob save from a copy taken BEFORE the position write would silently
/// revert the aggregate's increment and the published `totalWalletBalance` would go stale. Making the
/// field un-clobberable here means the ordering of `save_account` against `save_position` does not
/// matter anywhere, which is the only version of this that stays correct as call sites move.
///
/// Cost is one resident-account read (the account is already resident on every path that reaches
/// here, and `mutate_account` / `mutate_account_balance` — which RMW in place and therefore cannot
/// clobber anything — carry the hot paths anyway).
///
/// A test fixture that wants a non-zero aggregate must create it the way production does, through
/// [`save_position`].
pub fn save_account<H: PerpHost>(
    context: &mut H,
    user: Address,
    mut account: UserAccount,
) -> Result<(), PerpError> {
    account.total_position_margin = load_account_ref(context, user)?.total_position_margin;
    typed_store_mut(context).set_account(user, account);
    // The wallet moved ⇒ one snapshot at the end of the call. Ordering against the write no longer
    // matters (nothing is folded here); the drain reads the settled store.
    mark_account_snapshot_dirty(context, user);
    Ok(())
}

/// Position write for the paths that touch ONLY the per-side aggregates (`total_buy/sell_{qty,
/// notional}` — the `Bid`/`Ask` that derived `ooIM` is computed from): an order resting, an order
/// being cancelled, a cancel-all clearing both sides.
///
/// Two things it deliberately skips, and they are the same two facts:
///
/// * `save_position`'s `amount` zero-crossing hooks (open-position registry, per-user market index)
///   are dead work here — `amount` is unchanged, asserted below.
/// * it does **NOT** mark the user for an `AccountBalanceChanged` snapshot. Aggregates-only means no
///   wallet moved and no position state moved, which is exactly the condition under which Binance
///   pushes no `ACCOUNT_UPDATE` — see the trigger table on [`mark_account_snapshot_dirty`]. This is
///   what makes a pure placement and a pure cancel silent. Routing an aggregates-only write through
///   `save_position` instead would publish a snapshot for both.
///
/// `margin` is asserted unchanged alongside `amount`, and that assertion is load-bearing rather than
/// defensive: `Σ pos.margin` is half of the `totalWalletBalance` this event publishes, so a
/// `margin` edit slipping through this silent route would move a PUBLISHED field with no snapshot to
/// announce it — the exact payload/trigger mismatch the narrowed payload exists to close.
pub fn save_position_reservation_only<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    pos: PerpPosition,
) -> Result<(), PerpError> {
    #[cfg(debug_assertions)]
    {
        let old = load_position_ref(context, user, market_id)?;
        debug_assert_eq!(
            old.amount, pos.amount,
            "save_position_reservation_only on a path that changed pos.amount — the zero-crossing \
             registry hooks + collateral re-derivation in save_position are NOT dead there"
        );
        debug_assert_eq!(
            old.margin, pos.margin,
            "save_position_reservation_only on a path that changed pos.margin — that is a PUBLISHED \
             quantity (`Σ pos.margin` is `totalWalletBalance − totalCrossWalletBalance`), and this \
             route marks nobody, so the move would never reach the event stream"
        );
    }
    typed_store_mut(context).set_position(user, market_id, pos);
    Ok(())
}

/// [`save_order`] taking the order BY VALUE — the place path owns its `Order`, so this skips the
/// clone that `save_order(&Order)` forces.
pub fn save_order_owned<H: PerpHost>(
    context: &mut H,
    order_id: &[u8; 32],
    order: Order,
) -> Result<(), PerpError> {
    typed_store_mut(context).set_order(order_id, order);
    Ok(())
}

/// In-place RMW of a user's account blob (mirror of [`mutate_buy_orders`]): fast-path mutates the
/// deferred `Struct` already in the overlay (zero clone — no `usdc_balance` String copy); slow-path
/// loads once → mutate → store. Used by the folded fee-rate / nonce setters AND by wallet
/// credit/debit sites that previously did `load_account` (owned clone) → mutate → `save_account`
/// (clone again): routing those through here removes both `UserAccount` deep clones (each of which
/// heap-allocates the `usdc_balance` String) on the warm path. Byte-identical final blob to
/// load→modify→save, so it is golden-neutral.
pub fn mutate_account<H: PerpHost, R>(
    context: &mut H,
    user: Address,
    f: impl FnOnce(&mut UserAccount) -> R,
) -> Result<R, PerpError> {
    // Fast path: resident in the typed store → in-place &mut (CoW lift on first write).
    if let Some(a) = typed_store_mut(context).account_mut(user) {
        return Ok(f(a));
    }
    // Deleted/absent fallback: materialize the default, mutate, and store it.
    let mut a = load_account(context, user)?;
    let r = f(&mut a);
    typed_store_mut(context).set_account(user, a);
    Ok(r)
}

/// [`mutate_account`] for mutations that MOVE a balance (`credit_perp` / `debit_perp`): marks the
/// user for an end-of-call `AccountBalanceChanged` snapshot.
///
/// Split from `mutate_account` (which stays silent) so neither path pays for change DETECTION. The
/// caller always knows whether it moved money, and marking unconditionally is the fail-safe
/// direction — a redundant snapshot is harmless, a missing one is not. `mutate_account` therefore
/// serves only the writes that provably cannot move a balance: the per-placement nonce bump and
/// fee-rate updates. Detecting instead cost those a `usdc_balance` String clone per call for a
/// comparison that could never fire.
pub fn mutate_account_balance<H: PerpHost, R>(
    context: &mut H,
    user: Address,
    f: impl FnOnce(&mut UserAccount) -> R,
) -> Result<R, PerpError> {
    // No after-image is carried out of here: the drain re-reads the settled store, which is what
    // makes the event byte-identical to `getAccount`. That also removed the `a.clone()` the fast path
    // used to pay purely to hand the emitter a payload — a full `UserAccount` deep clone,
    // `usdc_balance` String allocation included, on every credit/debit.
    let r = if let Some(a) = typed_store_mut(context).account_mut(user) {
        f(a)
    } else {
        let mut a = load_account(context, user)?;
        let r = f(&mut a);
        typed_store_mut(context).set_account(user, a);
        r
    };
    mark_account_snapshot_dirty(context, user);
    Ok(r)
}

/// Attaches the batch single-initiator working-set for `owner` (see
/// [`crate::typed_store::TypedPerpStore::begin_batch`]). Called by `drive_batch` before
/// the item loop; every account/position/buy/sell access whose subject is `owner` then routes to a
/// batch-scoped local instead of the main store, flushed once by [`flush_batch_ws`].
pub fn begin_batch_ws<H: PerpHost>(context: &mut H, owner: Address) {
    typed_store_mut(context).begin_batch(owner);
}

/// Flushes the batch working-set into the main store and detaches it (see
/// [`crate::typed_store::TypedPerpStore::flush_batch`]). Called by `drive_batch` on both
/// the normal-completion and abort-forward paths, before the driver returns.
pub fn flush_batch_ws<H: PerpHost>(context: &mut H) {
    typed_store_mut(context).flush_batch();
}


// Fee rates + nonce are folded into UserAccount (per-user, co-read with the account on the hot
// placement path). Reads go through `load_account_ref` (Arc bump — never clones the usdc_balance
// String); writes RMW the account blob in place.

pub fn load_user_fee_rates<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<UserFeeRates, PerpError> {
    let a = load_account_ref(context, user)?;
    Ok(UserFeeRates {
        maker_fee_bps: a.maker_fee_bps,
        taker_fee_bps: a.taker_fee_bps,
    })
}

pub fn save_user_fee_rates<H: PerpHost>(
    context: &mut H,
    user: Address,
    rates: UserFeeRates,
) -> Result<(), PerpError> {
    mutate_account(context, user, |a| {
        a.maker_fee_bps = rates.maker_fee_bps;
        a.taker_fee_bps = rates.taker_fee_bps;
    })
}

pub fn load_market_fee_total<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<u64, PerpError> {
    // Byte tier (the write side `add_market_fee_total` already uses store_blob) — off the Struct tier.
    let buf = load_blob(context, market_fee_total_key(market_id))?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn add_market_fee_total<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    amount: u64,
) -> Result<(), PerpError> {
    if amount == 0 {
        return Ok(());
    }
    let total = load_market_fee_total(context, market_id)?
        .checked_add(amount)
        .ok_or_else(|| perp_err("fee total overflow"))?;
    let buf = encode(&total)?;
    store_blob(context, market_fee_total_key(market_id), &buf)
}

// ── ERC-20 balance helpers ─────────────────────────────────────────────────────

pub fn load_erc20_balance<H: PerpHost>(
    context: &mut H,
    token: Address,
    account: Address,
) -> Result<U256, PerpError> {
    context.external_balance(token, account)
}

pub fn save_erc20_balance<H: PerpHost>(
    context: &mut H,
    token: Address,
    account: Address,
    balance: U256,
) -> Result<(), PerpError> {
    context.set_external_balance(token, account, balance)
}

// ── PerpPosition ──────────────────────────────────────────────────────────────

pub fn load_position<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<PerpPosition, PerpError> {
    use crate::typed_store::Resident;
    match typed_store_mut(context).position(user, market_id) {
        Resident::Hit(p) => return Ok(p.clone()),
        Resident::Deleted => return Ok(PerpPosition::default()),
        Resident::Miss => {}
    }
    Ok(cold_fill_position(context, user, market_id)?
        .map(|p| (*p).clone())
        .unwrap_or_default())
}

/// Zero-copy position read (点1): `Arc<PerpPosition>`, no per-read clone. Defaulted like
/// [`load_position`] (absent → default position) so call sites keep `p.field` (Deref) ergonomics.
/// PURE reads only (margin / size / entry checks). Fill/settlement RMW keep [`load_position`] +
/// [`save_position`].
pub fn load_position_ref<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<std::sync::Arc<PerpPosition>, PerpError> {
    use crate::typed_store::Resident;
    // Hot path: resident Arc, refcount bump only.
    if let Some(arc) = typed_store_mut(context).position_arc(user, market_id) {
        return Ok(arc);
    }
    // Deleted (definitively absent) must NOT fall through to the stale committed store.
    if matches!(
        typed_store_mut(context).position(user, market_id),
        Resident::Deleted
    ) {
        return Ok(std::sync::Arc::new(PerpPosition::default()));
    }
    Ok(cold_fill_position(context, user, market_id)?
        .unwrap_or_else(|| std::sync::Arc::new(PerpPosition::default())))
}


pub fn save_position<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    pos: &PerpPosition,
) -> Result<(), PerpError> {
    // Maintain the per-market open-position registry on an `amount` zero-crossing.
    // save_position is the single choke point for ALL position writes, so this hook
    // cannot be missed. The registry is only touched when membership changes
    // (open: 0 -> !=0, close: !=0 -> 0); every other save pays just one
    // typed-sub-map-cheap position read (the position is already resident).
    //
    // The SAME already-paid read supplies `old.margin` for the `Σ pos.margin` aggregate below —
    // which is the whole reason that aggregate is affordable here and the deleted `total_perp_collateral`
    // was not (see `types::UserAccount::total_position_margin`).
    let (old_amount, old_margin) = {
        let old = load_position_ref(context, user, market_id)?;
        (old.amount, old.margin)
    };
    // "A flat position holds no margin", enforced at the ONE door every `pos.margin` write comes
    // through rather than per market at snapshot time.
    //
    // This assertion used to live in `margin_view::index_account_wallet_balances`, per member market
    // of the per-user index, where it guarded the emit path's Σ walk: an order-list leg can flip index
    // membership (a placement adds a market, a cancel drops one) WITHOUT marking the user for a
    // snapshot, so a flat market carrying margin would have made a pure placement move
    // `totalWalletBalance` silently. That walk is GONE — `totalWalletBalance` reads the stored
    // aggregate and no longer depends on the index at all — so the argument is no longer
    // load-bearing. The INVARIANT is still true and still worth stating, and it belongs here: this is
    // where it can be violated, it is checked on every position write instead of only on markets that
    // happen to be in an index when a snapshot is published, and it is what makes
    // `Σ_index pos.margin == Σ_all pos.margin` (so `getAccount`'s index-driven cross-check below can
    // validate an aggregate maintained over ALL markets).
    debug_assert!(
        pos.amount != 0 || pos.margin == 0,
        "save_position: {user} market {market_id} written flat but holding margin {} — every close \
         must zero `margin` alongside `amount`",
        pos.margin
    );
    if old_amount == 0 && pos.amount != 0 {
        registry_add(context, market_id, user)?;
    } else if old_amount != 0 && pos.amount == 0 {
        registry_remove(context, market_id, user)?;
    }
    typed_store_mut(context).set_position(user, market_id, pos.clone());
    // Per-user market index: the SAME zero-crossing, but evaluated AFTER the write — the
    // leave branch re-reads the position to decide whether the user still has anything here,
    // so it has to see the new `amount`.
    sync_user_market_membership(context, user, market_id, old_amount != 0, pos.amount != 0)?;
    // ── `Σ pos.margin` on the account blob — THE single maintenance point ──────────────────────
    //
    // `totalWalletBalance = perp_wallet_balance + Σ pos.margin` is a PUBLISHED field (both account
    // views and `AccountBalanceChanged`), and the Σ leg is now stored rather than walked, so the emit
    // path is one account load instead of an index load plus a position load per member market. The
    // delta comes from the `old_margin` the registry hook already paid for, so this costs no extra
    // read — only the account write, and only when the margin actually moved.
    //
    // ⚠️ This is the ONLY place the aggregate may be touched. `save_position` is the single door for
    // every `pos.margin` write (`TypedPerpStore::set_position` / `position_mut` have no other
    // caller — verified by grep), and `save_position_reservation_only` `debug_assert`s that it cannot
    // change `margin`, which is precisely why the aggregate is cheap: it does NOT move on an order
    // resting or being cancelled. A second maintenance site, or one added to the reservation-only
    // route, re-opens exactly the drift `margin_view::index_account_scalars`' `debug_assertions`
    // cross-check exists to catch.
    //
    // Conditional on a non-zero delta so the many saves that move only `amount` / `v_quote_balance` /
    // `leverage` (e.g. `setLeverage`) do not dirty the account key for a no-op. Deterministic — the
    // condition is a comparison of two stored integers, identical on every node.
    if pos.margin != old_margin {
        let delta = pos
            .margin
            .checked_sub(old_margin)
            .ok_or_else(|| perp_err("save_position: position margin delta overflow"))?;
        // `mutate_account`, not `mutate_account_balance`: this is not a WALLET move (no
        // `credit_perp`/`debit_perp`), and `save_position` marks the snapshot itself two lines below,
        // so marking here as well would be redundant.
        mutate_account(context, user, |a| {
            a.total_position_margin = a
                .total_position_margin
                .checked_add(delta)
                .ok_or_else(|| perp_err("save_position: total position margin overflow"))?;
            Ok::<(), PerpError>(())
        })??;
    }
    // Position state moved (`amount` / `v_quote_balance` / `margin` / `leverage`) ⇒ one snapshot at
    // the end of the call. Binance's `ACCOUNT_UPDATE` carries a `P[]` array for exactly this, and its
    // own trigger sentence is "since there's no change on positions" — so a change IS the trigger.
    // Aggregates-only writes must NOT come through here; they have
    // [`save_position_reservation_only`].
    mark_account_snapshot_dirty(context, user);
    Ok(())
}

// ── Per-market open-position registry ──────────────────────────────────────
// The first enumerable set of open positions per market. Written by the
// save_position zero-crossing hook (above); enumerated by the liquidation sweep.
// Stored as raw concatenated 20-byte addresses (mirrors the level-FIFO
// pack_order_ids pattern); insertion order is preserved (deterministic across
// validators). An empty blob deletes the key. Membership is exact — closed
// positions are removed — so the set never grows unbounded.

fn pack_addresses(addrs: &[Address]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(addrs.len() * 20);
    for a in addrs {
        buf.extend_from_slice(a.as_slice());
    }
    buf
}

fn unpack_addresses(buf: &[u8]) -> Result<Vec<Address>, PerpError> {
    if buf.len() % 20 != 0 {
        return Err(perp_err("corrupt position-registry blob"));
    }
    Ok(buf.chunks_exact(20).map(Address::from_slice).collect())
}

/// Loads the set of addresses with an open position in `market_id` (insertion order).
pub fn load_position_registry<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<Vec<Address>, PerpError> {
    unpack_addresses(&load_blob(context, position_registry_key(market_id))?)
}

/// Persists the registry; an empty slice deletes the key.
fn save_position_registry<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    addrs: &[Address],
) -> Result<(), PerpError> {
    store_blob(
        context,
        position_registry_key(market_id),
        &pack_addresses(addrs),
    )
}

fn registry_add<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    user: Address,
) -> Result<(), PerpError> {
    let mut regs = load_position_registry(context, market_id)?;
    if !regs.contains(&user) {
        regs.push(user);
        save_position_registry(context, market_id, &regs)?;
    }
    Ok(())
}

fn registry_remove<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    user: Address,
) -> Result<(), PerpError> {
    let mut regs = load_position_registry(context, market_id)?;
    let before = regs.len();
    regs.retain(|a| a != &user);
    if regs.len() != before {
        save_position_registry(context, market_id, &regs)?;
    }
    Ok(())
}

// ── Per-user market index (`umkt`) ─────────────────────────────────────────
// The INVERSE of the per-market position registry above: for a user, which markets are they
// active in? "Active" means **a non-zero position OR at least one resting order** — the position
// registry cannot answer this, both because its direction is market → users and because it is
// blind to a market where the user holds only resting orders.
//
// Stored as an ASCENDING `Vec<u64>` in the typed store (same shape as the per-market price
// indexes). Ascending is what makes the blob CANONICAL: the same logical set reached by different
// operation orders serialises to the same bytes. Membership is exact — leaving a market removes
// the id, and emptying the set deletes the key (the `preg` convention) — so it never grows
// unbounded, and it is additionally capped at [`MAX_USER_MARKETS`].
//
// Maintained by [`sync_user_market_membership`], called from the write choke points of the
// activity predicate: [`save_position`] (position leg) and [`save_buy_orders`] /
// [`save_sell_orders`] / [`mutate_buy_orders`] / [`mutate_sell_orders`] (order-list legs).
// NOTHING reads it yet — it is deliberately inert until the derived-ooIM admission path lands.

/// The markets `user` is active in (ascending). Empty when the user is flat everywhere.
pub fn load_user_markets<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<Vec<u64>, PerpError> {
    Ok((*load_user_markets_ref(context, user)?).clone())
}

/// Zero-clone read of [`load_user_markets`] (`Arc<Vec<u64>>`, refcount bump).
pub fn load_user_markets_ref<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<std::sync::Arc<Vec<u64>>, PerpError> {
    use crate::typed_store::Resident;
    if let Some(arc) = typed_store_mut(context).user_markets_arc(user) {
        return Ok(arc);
    }
    // Deleted (left the last market THIS block) is definitively empty — it must not fall through
    // to the committed store, which still holds the pre-delete set.
    if matches!(
        typed_store_mut(context).user_markets(user),
        Resident::Deleted
    ) {
        return Ok(std::sync::Arc::new(Vec::new()));
    }
    let arc = cold_load::<_, Vec<u64>>(context, user_markets_key(user))?;
    typed_store_mut(context).fill_user_markets(user, arc.clone());
    Ok(arc.unwrap_or_else(|| std::sync::Arc::new(Vec::new())))
}

/// Pre-write admission gate for the [`MAX_USER_MARKETS`] cap.
///
/// Called from the VALIDATION phase of order placement — the only way a user can enter a market
/// they are not already in (a position can only appear through a fill of an order they placed;
/// a maker fill, a liquidation, an ADL or a funding settlement always acts on a market the user
/// is already active in). Pure read: it writes nothing, so it is a genuine reject in the
/// commit-only sense and may precede every write on the path.
pub fn ensure_user_market_admission<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<(), PerpError> {
    let markets = load_user_markets_ref(context, user)?;
    if markets.len() >= MAX_USER_MARKETS && markets.binary_search(&market_id).is_err() {
        return Err(perp_err(format!(
            "placeOrder: user market limit reached ({MAX_USER_MARKETS} markets)"
        )));
    }
    Ok(())
}

/// Adds `market_id` to `user`'s set (idempotent; keeps it ascending).
///
/// The cap here is an INVARIANT backstop, not the enforcement point: every path that can reach it
/// passed [`ensure_user_market_admission`] before writing anything, so a full set at this point
/// means the gate was bypassed. It is reported rather than silently dropped — a dropped id would
/// desynchronise the index from the state it mirrors, which is exactly the bug the cap exists to
/// make impossible.
fn user_markets_add<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<(), PerpError> {
    let mut markets = load_user_markets(context, user)?;
    let Err(i) = markets.binary_search(&market_id) else {
        return Ok(()); // already a member — no write, so no spurious delta key
    };
    if markets.len() >= MAX_USER_MARKETS {
        return Err(perp_invariant_err(format!(
            "user market index: {user} exceeded {MAX_USER_MARKETS} markets entering market \
             {market_id} (admission gate bypassed?)"
        )));
    }
    markets.insert(i, market_id);
    typed_store_mut(context).set_user_markets(user, markets);
    Ok(())
}

/// Removes `market_id` from `user`'s set; deletes the key when the set empties.
fn user_markets_remove<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<(), PerpError> {
    let mut markets = load_user_markets(context, user)?;
    let Ok(i) = markets.binary_search(&market_id) else {
        return Ok(()); // not a member — no write
    };
    markets.remove(i);
    if markets.is_empty() {
        typed_store_mut(context).remove_user_markets(user);
    } else {
        typed_store_mut(context).set_user_markets(user, markets);
    }
    Ok(())
}

/// Ground truth of the membership predicate, read back from storage: does `user` hold a non-zero
/// position OR any resting order in `market_id`?
fn user_market_is_active<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<bool, PerpError> {
    if load_position_ref(context, user, market_id)?.amount != 0 {
        return Ok(true);
    }
    if !load_buy_orders_ref(context, user, market_id)?.is_empty() {
        return Ok(true);
    }
    Ok(!load_sell_orders_ref(context, user, market_id)?.is_empty())
}

/// Per-write index maintenance, called AFTER one leg of the activity predicate (the position, the
/// buy list or the sell list) has been written, with that leg's own before/after emptiness.
///
/// Only a leg TRANSITION can change membership, so the common case (`was == now`) costs a single
/// bool compare and touches no storage:
/// * leg became non-empty → the user is active here → insert (idempotent: a user who was already
///   in the market through another leg writes nothing).
/// * leg became empty → the user MIGHT have left; consult the other two legs and remove only if
///   all three are now inactive.
///
/// Evaluating this after the leg's own write is what makes it order-independent: whichever leg
/// moves last sees the other two in their final state, so any interleaving of the three writes
/// converges on the same set.
fn sync_user_market_membership<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    leg_was_active: bool,
    leg_now_active: bool,
) -> Result<(), PerpError> {
    if leg_was_active == leg_now_active {
        return Ok(());
    }
    if leg_now_active {
        user_markets_add(context, user, market_id)
    } else if user_market_is_active(context, user, market_id)? {
        Ok(())
    } else {
        user_markets_remove(context, user, market_id)
    }
}

// ── Order entry lists (per-user per-market) ───────────────────────────────────

pub fn load_buy_orders<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<std::collections::VecDeque<OrderEntry>, PerpError> {
    use crate::typed_store::Resident;
    match typed_store_mut(context).buy_orders(user, market_id) {
        Resident::Hit(v) => return Ok(v.clone()),
        Resident::Deleted => return Ok(std::collections::VecDeque::new()),
        Resident::Miss => {}
    }
    let arc = cold_load::<_, std::collections::VecDeque<OrderEntry>>(context, user_buy_orders_key(user, market_id))?;
    typed_store_mut(context).fill_buy_orders(user, market_id, arc.clone());
    Ok(arc.map(|v| (*v).clone()).unwrap_or_default())
}

/// Zero-copy user buy-order-entry list (点1): `Arc<std::collections::VecDeque<OrderEntry>>`, no per-read clone. PURE reads
/// only (opposite-side snapshot in reservation calc / `.last()` / iteration). List edits use
/// [`mutate_buy_orders`].
pub fn load_buy_orders_ref<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<std::sync::Arc<std::collections::VecDeque<OrderEntry>>, PerpError> {
    use crate::typed_store::Resident;
    if let Some(arc) = typed_store_mut(context).buy_orders_arc(user, market_id) {
        return Ok(arc);
    }
    if matches!(
        typed_store_mut(context).buy_orders(user, market_id),
        Resident::Deleted
    ) {
        return Ok(std::sync::Arc::new(std::collections::VecDeque::new()));
    }
    let arc = cold_load::<_, std::collections::VecDeque<OrderEntry>>(context, user_buy_orders_key(user, market_id))?;
    typed_store_mut(context).fill_buy_orders(user, market_id, arc.clone());
    Ok(arc.unwrap_or_else(|| std::sync::Arc::new(std::collections::VecDeque::new())))
}

pub fn save_buy_orders<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    entries: &std::collections::VecDeque<OrderEntry>,
) -> Result<(), PerpError> {
    let was_active = !load_buy_orders_ref(context, user, market_id)?.is_empty();
    // An EMPTY list is a stored value (msgpack `0x90`, key present) — never a delete.
    typed_store_mut(context).set_buy_orders(user, market_id, entries.iter().copied().collect());
    sync_user_market_membership(context, user, market_id, was_active, !entries.is_empty())?;
    Ok(())
}

pub fn load_sell_orders<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<std::collections::VecDeque<OrderEntry>, PerpError> {
    use crate::typed_store::Resident;
    match typed_store_mut(context).sell_orders(user, market_id) {
        Resident::Hit(v) => return Ok(v.clone()),
        Resident::Deleted => return Ok(std::collections::VecDeque::new()),
        Resident::Miss => {}
    }
    let arc = cold_load::<_, std::collections::VecDeque<OrderEntry>>(context, user_sell_orders_key(user, market_id))?;
    typed_store_mut(context).fill_sell_orders(user, market_id, arc.clone());
    Ok(arc.map(|v| (*v).clone()).unwrap_or_default())
}

/// Zero-copy user sell-order-entry list (点1): `Arc<std::collections::VecDeque<OrderEntry>>`, no per-read clone. PURE reads
/// only. List edits use [`mutate_sell_orders`].
pub fn load_sell_orders_ref<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
) -> Result<std::sync::Arc<std::collections::VecDeque<OrderEntry>>, PerpError> {
    use crate::typed_store::Resident;
    if let Some(arc) = typed_store_mut(context).sell_orders_arc(user, market_id) {
        return Ok(arc);
    }
    if matches!(
        typed_store_mut(context).sell_orders(user, market_id),
        Resident::Deleted
    ) {
        return Ok(std::sync::Arc::new(std::collections::VecDeque::new()));
    }
    let arc = cold_load::<_, std::collections::VecDeque<OrderEntry>>(context, user_sell_orders_key(user, market_id))?;
    typed_store_mut(context).fill_sell_orders(user, market_id, arc.clone());
    Ok(arc.unwrap_or_else(|| std::sync::Arc::new(std::collections::VecDeque::new())))
}

pub fn save_sell_orders<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    entries: &std::collections::VecDeque<OrderEntry>,
) -> Result<(), PerpError> {
    let was_active = !load_sell_orders_ref(context, user, market_id)?.is_empty();
    typed_store_mut(context).set_sell_orders(user, market_id, entries.iter().copied().collect());
    sync_user_market_membership(context, user, market_id, was_active, !entries.is_empty())?;
    Ok(())
}

/// In-place mutate the user's buy-order list (#21 靶子2): if it's resident in the typed store, run
/// `f` on the live `&mut Vec` (CoW lift on first write, no load/store clone round-trip); otherwise
/// load it (cache/cold) → run `f` → store. `f`'s return value passes through (e.g. the recomputed
/// reservation, computed inside the borrow so it sees the post-mutation list). The result is
/// byte-identical to load→modify→`save_buy_orders` since the block-end ser is the same msgpack.
pub fn mutate_buy_orders<H: PerpHost, R>(
    context: &mut H,
    user: Address,
    market_id: u64,
    f: impl FnOnce(&mut std::collections::VecDeque<OrderEntry>) -> R,
) -> Result<R, PerpError> {
    // Emptiness before/after the edit drives the per-user market index (free here: the list is
    // already borrowed, so neither probe costs a storage access).
    let (r, was_active, now_active) =
        if let Some(entries) = typed_store_mut(context).buy_orders_mut(user, market_id) {
            let was = !entries.is_empty();
            let r = f(entries);
            let now = !entries.is_empty();
            (r, was, now)
        } else {
            let mut entries = load_buy_orders(context, user, market_id)?;
            let was = !entries.is_empty();
            let r = f(&mut entries);
            let now = !entries.is_empty();
            typed_store_mut(context).set_buy_orders(user, market_id, entries);
            (r, was, now)
        };
    sync_user_market_membership(context, user, market_id, was_active, now_active)?;
    Ok(r)
}

/// In-place mutate the user's sell-order list (#21 靶子2). See [`mutate_buy_orders`].
pub fn mutate_sell_orders<H: PerpHost, R>(
    context: &mut H,
    user: Address,
    market_id: u64,
    f: impl FnOnce(&mut std::collections::VecDeque<OrderEntry>) -> R,
) -> Result<R, PerpError> {
    // See [`mutate_buy_orders`] for the emptiness before/after that drives the market index.
    let (r, was_active, now_active) =
        if let Some(entries) = typed_store_mut(context).sell_orders_mut(user, market_id) {
            let was = !entries.is_empty();
            let r = f(entries);
            let now = !entries.is_empty();
            (r, was, now)
        } else {
            let mut entries = load_sell_orders(context, user, market_id)?;
            let was = !entries.is_empty();
            let r = f(&mut entries);
            let now = !entries.is_empty();
            typed_store_mut(context).set_sell_orders(user, market_id, entries);
            (r, was, now)
        };
    sync_user_market_membership(context, user, market_id, was_active, now_active)?;
    Ok(r)
}

// ── Full Order struct ─────────────────────────────────────────────────────────

pub fn load_order<H: PerpHost>(
    context: &mut H,
    order_id: &[u8; 32],
) -> Result<Option<Order>, PerpError> {
    use crate::typed_store::Resident;
    match typed_store_mut(context).order(order_id) {
        Resident::Hit(o) => return Ok(Some(o.clone())),
        // Deleted-this-block (delete-on-terminal) reads as not-found — never the stale committed row.
        Resident::Deleted => return Ok(None),
        Resident::Miss => {}
    }
    let arc = cold_load::<_, Order>(context, order_key(order_id))?;
    typed_store_mut(context).fill_order(order_id, arc.clone());
    Ok(arc.map(|o| (*o).clone()))
}

/// Zero-copy order read (点1): `Arc<Order>`, no per-read clone. PURE reads only (dup check / FOK
/// feasibility / field reads). Mutating an order keeps [`load_order`] + [`save_order`].
pub fn load_order_ref<H: PerpHost>(
    context: &mut H,
    order_id: &[u8; 32],
) -> Result<Option<std::sync::Arc<Order>>, PerpError> {
    use crate::typed_store::Resident;
    if let Some(arc) = typed_store_mut(context).order_arc(order_id) {
        return Ok(Some(arc));
    }
    if matches!(typed_store_mut(context).order(order_id), Resident::Deleted) {
        return Ok(None);
    }
    let arc = cold_load::<_, Order>(context, order_key(order_id))?;
    typed_store_mut(context).fill_order(order_id, arc.clone());
    Ok(arc)
}

pub fn save_order<H: PerpHost>(
    context: &mut H,
    order_id: &[u8; 32],
    order: &Order,
) -> Result<(), PerpError> {
    typed_store_mut(context).set_order(order_id, order.clone());
    Ok(())
}

/// Deletes an order record (delete-on-terminal): a resident tombstone in the typed store —
/// `load_order` → `None` for the rest of the block, and the block delta emits empty bytes (the
/// store DELETE convention), byte-identical to the old empty-blob write. A save-then-delete in the
/// same block reads back absent. Callers use this the instant an order reaches a terminal status
/// so the order map only ever holds live (Open/PartiallyFilled) orders.
pub fn delete_order<H: PerpHost>(
    context: &mut H,
    order_id: &[u8; 32],
) -> Result<(), PerpError> {
    typed_store_mut(context).remove_order(order_id);
    Ok(())
}

// ── Global trade counter ──────────────────────────────────────────────────────

/// Atomically increment and return the *current* trade ID for a market, then store the
/// incremented value.  Returns 0 for the first trade in that market, 1 for the second, etc.
/// Trade IDs are per-market so that indexers can use them directly as `fromId` cursors.
pub fn next_trade_id<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<u64, PerpError> {
    // Byte tier (off the Struct tier); always writes the incremented counter.
    let buf = load_blob(context, trade_count_key(market_id))?;
    let current: u64 = if buf.is_empty() { 0 } else { decode(&buf)? };
    let nbuf = encode(&(current + 1))?;
    store_blob(context, trade_count_key(market_id), &nbuf)?;
    Ok(current)
}

// ── User nonce (folded into UserAccount) ────────────────────────────────────────

pub fn load_user_nonce<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<u64, PerpError> {
    Ok(load_account_ref(context, user)?.nonce)
}

pub fn save_user_nonce<H: PerpHost>(
    context: &mut H,
    user: Address,
    nonce: u64,
) -> Result<(), PerpError> {
    mutate_account(context, user, |a| a.nonce = nonce)
}

// ── Market ────────────────────────────────────────────────────────────────────

pub fn load_market<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<Option<Market>, PerpError> {
    use crate::typed_store::Resident;
    match typed_store_mut(context).market(market_id) {
        Resident::Hit(m) => return Ok(Some(m.clone())),
        Resident::Deleted => return Ok(None),
        Resident::Miss => {}
    }
    Ok(cold_fill_market(context, market_id)?.map(|m| (*m).clone()))
}

/// Zero-copy market read (点1): `Arc<Market>`, no per-read clone. PURE reads only (params / mark /
/// funding). RMW (funding/config writes) keep [`load_market`] + [`save_market`].
pub fn load_market_ref<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<Option<std::sync::Arc<Market>>, PerpError> {
    use crate::typed_store::Resident;
    // Hot path: resident Arc, refcount bump only.
    if let Some(arc) = typed_store_mut(context).market_arc(market_id) {
        return Ok(Some(arc));
    }
    // Deleted (definitively absent) must NOT fall through to the stale committed store.
    if matches!(
        typed_store_mut(context).market(market_id),
        Resident::Deleted
    ) {
        return Ok(None);
    }
    cold_fill_market(context, market_id)
}

pub fn save_market<H: PerpHost>(
    context: &mut H,
    market: &Market,
) -> Result<(), PerpError> {
    typed_store_mut(context).set_market(market.market_id, market.clone());
    Ok(())
}

/// In-place RMW of the Market blob (mirror of [`mutate_account`]): fast-path mutates the value
/// resident in the typed store (CoW lift on first write, in-place thereafter); slow-path loads
/// once → mutate → store. Used by [`save_mark_price`] to set the mark field without a full
/// load+save owned clone pair. Errors on a market that was never saved (rather than fabricate a
/// phantom zero Market) — every caller (`updateIndexPrice`, tests) writes the full Market first,
/// so this never fires in practice.
fn mutate_market<H: PerpHost, R>(
    context: &mut H,
    market_id: u64,
    f: impl FnOnce(&mut Market) -> R,
) -> Result<R, PerpError> {
    // Fast path: resident in the typed store → in-place &mut.
    if let Some(m) = typed_store_mut(context).market_mut(market_id) {
        return Ok(f(m));
    }
    // Cold/deleted: materialize once (fills the store), mutate, store the result.
    let mut m = load_market(context, market_id)?
        .ok_or_else(|| perp_err("mutate_market: unknown market"))?;
    let r = f(&mut m);
    typed_store_mut(context).set_market(market_id, m);
    Ok(r)
}

// ── Per-market hot scalars (grouped: MarketHot) ─────────────────────────────────
// best bid/ask + last traded + open interest (the PER-TRADE scalars) live in ONE blob → co-access
// = one probe/decode/Arc, one cache line, one coalesced write. (Mark price is NOT here — it is
// write-rare + co-read with config, so it lives in the Market blob; see [`load_mark_price`].)

pub fn load_market_hot<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<MarketHot, PerpError> {
    use crate::typed_store::Resident;
    match typed_store_mut(context).market_hot(market_id) {
        Resident::Hit(h) => return Ok(h.clone()),
        Resident::Deleted => return Ok(MarketHot::default()),
        Resident::Miss => {}
    }
    let arc = cold_load::<_, MarketHot>(context, market_hot_key(market_id))?;
    typed_store_mut(context).fill_market_hot(market_id, arc.clone());
    Ok(arc.map(|h| (*h).clone()).unwrap_or_default())
}

/// In-place RMW of a market's hot scalars (mirror of [`mutate_buy_orders`]): fast-path mutates the
/// value resident in the typed store (CoW lift on first write, one write coalesced across scalar
/// setters); slow-path loads once → mutate → store. Byte-identical final blob to a load→modify→save.
fn mutate_market_hot<H: PerpHost, R>(
    context: &mut H,
    market_id: u64,
    f: impl FnOnce(&mut MarketHot) -> R,
) -> Result<R, PerpError> {
    if let Some(h) = typed_store_mut(context).market_hot_mut(market_id) {
        return Ok(f(h));
    }
    let mut h = load_market_hot(context, market_id)?;
    let r = f(&mut h);
    typed_store_mut(context).set_market_hot(market_id, h);
    Ok(r)
}

// ── Mark price (a field of the Market blob) ─────────────────────────────────────
// Callers already holding `&Market` (validate band check, the match walk) should read
// `market.mark_price` directly — zero extra probe. These helpers are for callers WITHOUT the
// Market in hand (and for tests); they read via the Arc (no clone) / RMW the Market field.

pub fn load_mark_price<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<u64, PerpError> {
    Ok(load_market_ref(context, market_id)?
        .map(|m| m.mark_price)
        .unwrap_or(0))
}

pub fn save_mark_price<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<(), PerpError> {
    mutate_market(context, market_id, |m| m.mark_price = price)
}

// ── Open interest ─────────────────────────────────────────────────────────────

pub fn load_open_interest<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<u64, PerpError> {
    Ok(load_market_hot(context, market_id)?.open_interest)
}

pub fn save_open_interest<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    oi: u64,
) -> Result<(), PerpError> {
    mutate_market_hot(context, market_id, |h| h.open_interest = oi)
}

// ── Order book: price level lists ─────────────────────────────────────────────
// The active price levels per side are a SORTED `Vec<u64>` (ascending). Present-check /
// insert-position use `binary_search` = O(log n) (NOT a linear `contains`); insert/remove are
// O(n) memmove of contiguous u64 — trivially cheap at real book depth. This reverts catalog #22's
// `BTreeSet<u64>`, which was O(log n) insert/remove but paid node allocation, pointer-chasing
// iteration, and — worst — O(n log n)+n-alloc cross-block cold rebuild and slower per-block
// serialization. A 3-way A/B (2026-07-20) showed sorted-Vec+binary_search beats BTreeSet at every
// depth. The Vec is kept ascending so its msgpack bytes are byte-identical to the BTreeSet's (both
// serialize as an ascending array) → commitment UNCHANGED, no CHAIN change. Side order for BBO:
//   * asks — best = min = `.first()`; walk lowest-first = `.iter()`.
//   * bids — best = max = `.last()`;  walk highest-first = `.iter().rev()`.

/// Insert `x` into a sorted (ascending) `Vec` if absent; returns `true` if newly inserted.
/// O(log n) `binary_search` + O(n) memmove — the sorted-Vec analogue of `BTreeSet::insert`.
#[inline]
fn sorted_insert(v: &mut Vec<u64>, x: u64) -> bool {
    match v.binary_search(&x) {
        Ok(_) => false,
        Err(i) => {
            v.insert(i, x);
            true
        }
    }
}

/// Remove `x` from a sorted `Vec` if present. O(log n) search + O(n) memmove.
#[inline]
fn sorted_remove(v: &mut Vec<u64>, x: u64) {
    if let Ok(i) = v.binary_search(&x) {
        v.remove(i);
    }
}

/// Fills the typed store's bid-price index from the cold store if not resident this block.
fn ensure_bid_prices_resident<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<(), PerpError> {
    use crate::typed_store::Resident;
    if matches!(typed_store_mut(context).bid_prices(market_id), Resident::Miss) {
        let arc = cold_load::<_, Vec<u64>>(context, bid_prices_key(market_id))?;
        typed_store_mut(context).fill_bid_prices(market_id, arc);
    }
    Ok(())
}

fn ensure_ask_prices_resident<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<(), PerpError> {
    use crate::typed_store::Resident;
    if matches!(typed_store_mut(context).ask_prices(market_id), Resident::Miss) {
        let arc = cold_load::<_, Vec<u64>>(context, ask_prices_key(market_id))?;
        typed_store_mut(context).fill_ask_prices(market_id, arc);
    }
    Ok(())
}

/// Active bid price levels (best = max).
pub fn load_bid_prices<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<Vec<u64>, PerpError> {
    use crate::typed_store::Resident;
    match typed_store_mut(context).bid_prices(market_id) {
        Resident::Hit(v) => return Ok(v.clone()),
        Resident::Deleted => return Ok(Vec::new()),
        Resident::Miss => {}
    }
    let arc = cold_load::<_, Vec<u64>>(context, bid_prices_key(market_id))?;
    typed_store_mut(context).fill_bid_prices(market_id, arc.clone());
    Ok(arc.map(|v| (*v).clone()).unwrap_or_default())
}

/// Zero-copy active bid prices (点1): `Arc<Vec<u64>>`, no per-read clone. PURE reads only
/// (BBO / matching walk — bids walk `.iter().rev()`, best = `.last()`). Edits use
/// [`insert_bid_price`] / [`remove_bid_price`].
pub fn load_bid_prices_ref<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<std::sync::Arc<Vec<u64>>, PerpError> {
    use crate::typed_store::Resident;
    if let Some(arc) = typed_store_mut(context).bid_prices_arc(market_id) {
        return Ok(arc);
    }
    if matches!(typed_store_mut(context).bid_prices(market_id), Resident::Deleted) {
        return Ok(std::sync::Arc::new(Vec::new()));
    }
    let arc = cold_load::<_, Vec<u64>>(context, bid_prices_key(market_id))?;
    typed_store_mut(context).fill_bid_prices(market_id, arc.clone());
    Ok(arc.unwrap_or_else(|| std::sync::Arc::new(Vec::new())))
}

pub fn save_bid_prices<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    prices: &Vec<u64>,
) -> Result<(), PerpError> {
    typed_store_mut(context).set_bid_prices(market_id, prices.clone());
    Ok(())
}

/// Active ask price levels (best = min).
pub fn load_ask_prices<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<Vec<u64>, PerpError> {
    use crate::typed_store::Resident;
    match typed_store_mut(context).ask_prices(market_id) {
        Resident::Hit(v) => return Ok(v.clone()),
        Resident::Deleted => return Ok(Vec::new()),
        Resident::Miss => {}
    }
    let arc = cold_load::<_, Vec<u64>>(context, ask_prices_key(market_id))?;
    typed_store_mut(context).fill_ask_prices(market_id, arc.clone());
    Ok(arc.map(|v| (*v).clone()).unwrap_or_default())
}

/// Zero-copy active ask prices (点1): `Arc<Vec<u64>>`, no per-read clone. PURE reads only
/// (asks walk `.iter()`, best = `.first()`).
pub fn load_ask_prices_ref<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<std::sync::Arc<Vec<u64>>, PerpError> {
    use crate::typed_store::Resident;
    if let Some(arc) = typed_store_mut(context).ask_prices_arc(market_id) {
        return Ok(arc);
    }
    if matches!(typed_store_mut(context).ask_prices(market_id), Resident::Deleted) {
        return Ok(std::sync::Arc::new(Vec::new()));
    }
    let arc = cold_load::<_, Vec<u64>>(context, ask_prices_key(market_id))?;
    typed_store_mut(context).fill_ask_prices(market_id, arc.clone());
    Ok(arc.unwrap_or_else(|| std::sync::Arc::new(Vec::new())))
}

pub fn save_ask_prices<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    prices: &Vec<u64>,
) -> Result<(), PerpError> {
    typed_store_mut(context).set_ask_prices(market_id, prices.clone());
    Ok(())
}

// ── In-place orderbook mutation (catalog #21, generalized) ──────────────────────
// Mirror of `mutate_buy_orders`/`mutate_sell_orders`: fast-path mutates the deferred `Struct`
// already in the block overlay IN PLACE (zero clone, zero re-encode); slow-path (first touch this
// block) loads once (选项A: one Arc clone from the committed store, no re-deserialize) and stores
// the struct back into the overlay. Byte-identical final bytes → commitment unchanged.
// These variants ALWAYS write on the slow path — matching the callers whose pre-#21 form ended in an
// UNCONDITIONAL `save_*` (remove_*_price, level detach). Conditional-write callers (insert_*_price)
// are handled inline to preserve their "no write when unchanged" delta semantics (golden).

fn mutate_bid_prices<H: PerpHost, R>(
    context: &mut H,
    market_id: u64,
    f: impl FnOnce(&mut Vec<u64>) -> R,
) -> Result<R, PerpError> {
    // Unconditional write (matches the old load→f→save): materialize (cold-fill if first touch)
    // then mutate the resident Vec in place with an auto-mark.
    ensure_bid_prices_resident(context, market_id)?;
    Ok(f(typed_store_mut(context).bid_prices_mut(market_id)))
}

fn mutate_ask_prices<H: PerpHost, R>(
    context: &mut H,
    market_id: u64,
    f: impl FnOnce(&mut Vec<u64>) -> R,
) -> Result<R, PerpError> {
    ensure_ask_prices_resident(context, market_id)?;
    Ok(f(typed_store_mut(context).ask_prices_mut(market_id)))
}

// (Removed the key-based `mutate_level` — superseded by side-aware `mutate_bid_level` /
// `mutate_ask_level` on the typed store; the RAW-codec level path no longer flows through the
// type-erased overlay.)

// ── Order book: FIFO queue at a price level ───────────────────────────────────

/// Packs an order-id FIFO queue as raw concatenated 32-byte ids — the most compact form for a
/// homogeneous fixed-size-id list, with zero msgpack overhead (P4/#20). An empty queue packs to an
/// empty buf (which `store_blob` treats as a delete; `load_*_level` reads it back as `vec![]`).
fn pack_order_ids(queue: &[[u8; 32]]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(queue.len() * 32);
    for id in queue {
        buf.extend_from_slice(id);
    }
    buf
}

/// Inverse of [`pack_order_ids`]: chunks a raw blob into 32-byte ids.
/// Cold read for a level (raw codec): cross-block decoded store (Arc<LevelBlob>, zero clone) →
/// committed bytes → `unpack_level` once. Mirrors [`cold_load`] but for the non-msgpack level blob.
fn cold_load_level<H: PerpHost>(
    context: &mut H,
    key: B256,
) -> Result<Option<std::sync::Arc<LevelBlob>>, PerpError> {
    if let Some(arc) = context.perp_load_arc(key)? {
        if let Ok(b) = std::sync::Arc::downcast::<LevelBlob>(arc) {
            return Ok(Some(b));
        }
    }
    let buf = load_blob(context, key)?;
    if buf.is_empty() {
        return Ok(None);
    }
    Ok(Some(std::sync::Arc::new(unpack_level(&buf)?)))
}

// (Removed `ser_level` / `clone_level` — the overlay deferred-`Struct` level serializer + cloner.
// The typed store serializes levels directly via `pack_level` in its `take_delta`; the raw-codec
// level path no longer uses the type-erased overlay's fn-pointer machinery.)

/// Reads a resident bid level blob (typed store) or cold-loads + fills it. A cold-absent level is
/// the empty default (count 0, no ids) — same as `unpack_level("")`.
fn bid_level_blob<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<std::sync::Arc<LevelBlob>, PerpError> {
    if let Some(arc) = typed_store_mut(context).bid_level_arc(market_id, price) {
        return Ok(arc);
    }
    if typed_store_mut(context).bid_level_resident(market_id, price) {
        return Ok(std::sync::Arc::new(LevelBlob::default())); // resident cold-absent (fill None)
    }
    let arc = cold_load_level(context, bid_level_key(market_id, price))?;
    typed_store_mut(context).fill_bid_level(market_id, price, arc.clone());
    Ok(arc.unwrap_or_else(|| std::sync::Arc::new(LevelBlob::default())))
}

fn ask_level_blob<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<std::sync::Arc<LevelBlob>, PerpError> {
    if let Some(arc) = typed_store_mut(context).ask_level_arc(market_id, price) {
        return Ok(arc);
    }
    if typed_store_mut(context).ask_level_resident(market_id, price) {
        return Ok(std::sync::Arc::new(LevelBlob::default()));
    }
    let arc = cold_load_level(context, ask_level_key(market_id, price))?;
    typed_store_mut(context).fill_ask_level(market_id, price, arc.clone());
    Ok(arc.unwrap_or_else(|| std::sync::Arc::new(LevelBlob::default())))
}

/// In-place RMW of a bid level (count + ids), auto-marking dirty (matches the old unconditional
/// mutate_level→save_level). Materializes (cold-fill if first touch) then mutates in place.
fn mutate_bid_level<H: PerpHost, R>(
    context: &mut H,
    market_id: u64,
    price: u64,
    f: impl FnOnce(&mut LevelBlob) -> R,
) -> Result<R, PerpError> {
    if !typed_store_mut(context).bid_level_resident(market_id, price) {
        let arc = cold_load_level(context, bid_level_key(market_id, price))?;
        typed_store_mut(context).fill_bid_level(market_id, price, arc);
    }
    Ok(f(typed_store_mut(context).bid_level_mut(market_id, price)))
}

fn mutate_ask_level<H: PerpHost, R>(
    context: &mut H,
    market_id: u64,
    price: u64,
    f: impl FnOnce(&mut LevelBlob) -> R,
) -> Result<R, PerpError> {
    if !typed_store_mut(context).ask_level_resident(market_id, price) {
        let arc = cold_load_level(context, ask_level_key(market_id, price))?;
        typed_store_mut(context).fill_ask_level(market_id, price, arc);
    }
    Ok(f(typed_store_mut(context).ask_level_mut(market_id, price)))
}

/// Reads a bid level's FIFO ids (for tests / callers that only need the ids).
pub fn load_bid_level<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<Vec<[u8; 32]>, PerpError> {
    Ok(bid_level_blob(context, market_id, price)?.ids.clone())
}

/// Zero-copy bid level read (点1): `Arc<LevelBlob>` (ids + count).
pub fn load_bid_level_arc<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<std::sync::Arc<LevelBlob>, PerpError> {
    bid_level_blob(context, market_id, price)
}

/// Writes a bid level (live `count` + `ids`).
pub fn save_bid_level<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
    count: u64,
    ids: &[[u8; 32]],
) -> Result<(), PerpError> {
    typed_store_mut(context).set_bid_level(
        market_id,
        price,
        LevelBlob {
            count,
            ids: ids.to_vec(),
        },
    );
    Ok(())
}

/// Reads an ask level's FIFO ids.
pub fn load_ask_level<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<Vec<[u8; 32]>, PerpError> {
    Ok(ask_level_blob(context, market_id, price)?.ids.clone())
}

/// Zero-copy ask level read (点1): `Arc<LevelBlob>` (ids + count).
pub fn load_ask_level_arc<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<std::sync::Arc<LevelBlob>, PerpError> {
    ask_level_blob(context, market_id, price)
}

/// Writes an ask level (live `count` + `ids`).
pub fn save_ask_level<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
    count: u64,
    ids: &[[u8; 32]],
) -> Result<(), PerpError> {
    typed_store_mut(context).set_ask_level(
        market_id,
        price,
        LevelBlob {
            count,
            ids: ids.to_vec(),
        },
    );
    Ok(())
}

// ── Order book helpers ────────────────────────────────────────────────────────

/// Insert `price` into the active bid price set if not already present. O(log n) binary_search + O(n) memmove.
pub fn insert_bid_price<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<(), PerpError> {
    // Conditional write: dirty-mark (delta membership) fires ONLY when the price is newly inserted,
    // preserving the "no store when already present" commitment semantics. Materialize first, then
    // sorted_insert via the no-mark handle and mark iff it changed.
    ensure_bid_prices_resident(context, market_id)?;
    let changed = sorted_insert(typed_store_mut(context).bid_prices_mut_nomark(market_id), price);
    if changed {
        typed_store_mut(context).mark_bid_prices(market_id);
    }
    Ok(())
}

/// Insert `price` into the active ask price set if not already present. O(log n) binary_search + O(n) memmove.
pub fn insert_ask_price<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<(), PerpError> {
    ensure_ask_prices_resident(context, market_id)?;
    let changed = sorted_insert(typed_store_mut(context).ask_prices_mut_nomark(market_id), price);
    if changed {
        typed_store_mut(context).mark_ask_prices(market_id);
    }
    Ok(())
}

/// Remove `price` from the active bid price set (call when level becomes empty). O(log n).
pub fn remove_bid_price<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<(), PerpError> {
    mutate_bid_prices(context, market_id, |prices| {
        sorted_remove(prices, price);
    })
}

/// Remove `price` from the active ask price set (call when level becomes empty). O(log n).
pub fn remove_ask_price<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<(), PerpError> {
    mutate_ask_prices(context, market_id, |prices| {
        sorted_remove(prices, price);
    })
}

/// Rest an order at a bid level: push its id to the FIFO AND bump the live count, in ONE in-place
/// blob mutate (was push + a separate incr_level_count on a separate key).
pub fn push_bid_order<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
    order_id: [u8; 32],
) -> Result<(), PerpError> {
    mutate_bid_level(context, market_id, price, |b| {
        b.ids.push(order_id);
        b.count += 1;
    })
}

/// Rest an order at an ask level. See [`push_bid_order`].
pub fn push_ask_order<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
    order_id: [u8; 32],
) -> Result<(), PerpError> {
    mutate_ask_level(context, market_id, price, |b| {
        b.ids.push(order_id);
        b.count += 1;
    })
}

// ── Per-level live-order count (lazy-queue) ────────────────────────────────────
// The count of LIVE (Open/PartiallyFilled) orders at a level, stored IN the level blob next to the
// FIFO ids (Obs-1 merge: one key, not two). lazy-queue leaves cancelled/filled ids in the FIFO
// (swept by the next match walk), so `ids.len()` no longer tracks liveness — `count` is the
// authoritative "is the level empty?" signal. Maintained by: rest (push +1), cancel (decr −1),
// cancel-all (decr −live), and the match walk (`SaveLevel` carries the post-walk survivor count).
// count == 0 → the whole level blob is deleted (stale ids discarded).

pub fn load_bid_count<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<u64, PerpError> {
    Ok(bid_level_blob(context, market_id, price)?.count)
}

pub fn load_ask_count<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<u64, PerpError> {
    Ok(ask_level_blob(context, market_id, price)?.count)
}

/// Decrement a side's level count by `n` (orders removed: cancel / bulk-cancel). Returns the new
/// count. On reaching 0 the level is EMPTY → the FIFO ids are cleared so the blob packs to empty
/// (delete); the caller still removes the price from the index. Saturates at 0.
pub fn decr_level_count<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: crate::types::Side,
    price: u64,
    n: u64,
) -> Result<u64, PerpError> {
    use crate::types::Side;
    let f = |b: &mut LevelBlob| {
        b.count = b.count.saturating_sub(n);
        if b.count == 0 {
            b.ids.clear();
        }
        b.count
    };
    match side {
        Side::Buy => mutate_bid_level(context, market_id, price, f),
        Side::Sell => mutate_ask_level(context, market_id, price, f),
    }
}

// ── Best bid / ask cache ──────────────────────────────────────────────────────
// Field accessors on the grouped [`MarketHot`] blob (0 = "no orders on that side"). Kept in sync
// with the sorted price lists so callers can avoid loading the full list for a PostOnly / spread
// check.

pub fn load_best_bid<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<u64, PerpError> {
    Ok(load_market_hot(context, market_id)?.best_bid)
}

pub fn save_best_bid<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<(), PerpError> {
    mutate_market_hot(context, market_id, |h| h.best_bid = price)
}

pub fn load_best_ask<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<u64, PerpError> {
    Ok(load_market_hot(context, market_id)?.best_ask)
}

pub fn save_best_ask<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<(), PerpError> {
    mutate_market_hot(context, market_id, |h| h.best_ask = price)
}

/// Re-derive best_bid from the current bid price list (already in journal cache after matching).
/// Call this after any operation that may have removed the top bid level.
pub fn refresh_best_bid<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<u64, PerpError> {
    let prices = load_bid_prices(context, market_id)?;
    let best = prices.last().copied().unwrap_or(0); // bids: best = max
    save_best_bid(context, market_id, best)?;
    Ok(best)
}

/// Re-derive best_ask from the current ask price list (already in journal cache after matching).
/// Call this after any operation that may have removed the top ask level.
pub fn refresh_best_ask<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<u64, PerpError> {
    let prices = load_ask_prices(context, market_id)?;
    let best = prices.first().copied().unwrap_or(0);
    save_best_ask(context, market_id, best)?;
    Ok(best)
}
// ── API key (ed25519 signed orders) ──────────────────────────────────────────

pub fn load_api_key<H: PerpHost>(
    context: &mut H,
    user: Address,
    key_id: u8,
) -> Result<Option<ApiKey>, PerpError> {
    let buf = load_blob(context, api_key_key(user, key_id))?;
    if buf.is_empty() {
        return Ok(None);
    }
    Ok(Some(decode(&buf)?))
}

pub fn save_api_key<H: PerpHost>(
    context: &mut H,
    user: Address,
    key_id: u8,
    key: ApiKey,
) -> Result<(), PerpError> {
    let buf = encode(&key)?;
    store_blob(context, api_key_key(user, key_id), &buf)?;
    // Track this key_id in the user's id list (deduplicated).
    let mut ids = load_api_key_ids(context, user)?;
    if !ids.contains(&key_id) {
        ids.push(key_id);
        ids.sort_unstable();
        save_api_key_ids(context, user, &ids)?;
    }
    Ok(())
}

pub fn delete_api_key<H: PerpHost>(
    context: &mut H,
    user: Address,
    key_id: u8,
) -> Result<(), PerpError> {
    store_blob(context, api_key_key(user, key_id), &[])?;
    let mut ids = load_api_key_ids(context, user)?;
    ids.retain(|&id| id != key_id);
    save_api_key_ids(context, user, &ids)
}

pub fn load_api_key_ids<H: PerpHost>(
    context: &mut H,
    user: Address,
) -> Result<Vec<u8>, PerpError> {
    let buf = load_blob(context, api_key_ids_key(user))?;
    if buf.is_empty() {
        return Ok(vec![]);
    }
    decode(&buf)
}

fn save_api_key_ids<H: PerpHost>(
    context: &mut H,
    user: Address,
    ids: &[u8],
) -> Result<(), PerpError> {
    let buf = encode(&ids)?;
    store_blob(context, api_key_ids_key(user), &buf)
}

// ── Role addresses ────────────────────────────────────────────────────────────

pub fn load_oracle<H: PerpHost>(context: &mut H) -> Result<Address, PerpError> {
    let buf = load_blob(context, oracle_key())?;
    if buf.is_empty() {
        return Ok(Address::ZERO);
    }
    decode(&buf)
}

pub fn save_oracle<H: PerpHost>(
    context: &mut H,
    oracle: Address,
) -> Result<(), PerpError> {
    let buf = encode(&oracle)?;
    store_blob(context, oracle_key(), &buf)
}

pub fn load_market_manager<H: PerpHost>(context: &mut H) -> Result<Address, PerpError> {
    let buf = load_blob(context, market_manager_key())?;
    if buf.is_empty() {
        return Ok(Address::ZERO);
    }
    decode(&buf)
}

pub fn save_market_manager<H: PerpHost>(
    context: &mut H,
    manager: Address,
) -> Result<(), PerpError> {
    let buf = encode(&manager)?;
    store_blob(context, market_manager_key(), &buf)
}

// ── Index price state ─────────────────────────────────────────────────────────

pub fn load_index_price_state<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<IndexPriceState, PerpError> {
    let buf = load_blob(context, index_price_state_key(market_id))?;
    if buf.is_empty() {
        return Ok(IndexPriceState::default());
    }
    decode(&buf)
}

pub fn save_index_price_state<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    state: &IndexPriceState,
) -> Result<(), PerpError> {
    let buf = encode(state)?;
    store_blob(context, index_price_state_key(market_id), &buf)
}

// ── Price mid window (30s MA basis input) ─────────────────────────────────────

pub fn load_index_price_history<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<IndexPriceHistory, PerpError> {
    let buf = load_blob(context, index_price_history_key(market_id))?;
    if buf.is_empty() {
        return Ok(IndexPriceHistory::default());
    }
    decode(&buf)
}

pub fn save_index_price_history<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    history: &IndexPriceHistory,
) -> Result<(), PerpError> {
    let buf = encode(history)?;
    store_blob(context, index_price_history_key(market_id), &buf)
}

pub fn load_price_basis_window<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<PriceBasisWindow, PerpError> {
    let buf = load_blob(context, price_basis_window_key(market_id))?;
    if buf.is_empty() {
        return Ok(PriceBasisWindow::default());
    }
    decode(&buf)
}

pub fn save_price_basis_window<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    window: &PriceBasisWindow,
) -> Result<(), PerpError> {
    let buf = encode(window)?;
    store_blob(context, price_basis_window_key(market_id), &buf)
}

// ── Last traded price (contract price) ───────────────────────────────────────
// Field accessor on the grouped [`MarketHot`] blob.

pub fn load_last_traded_price<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<u64, PerpError> {
    Ok(load_market_hot(context, market_id)?.last_traded)
}

pub fn save_last_traded_price<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    price: u64,
) -> Result<(), PerpError> {
    mutate_market_hot(context, market_id, |h| h.last_traded = price)
}

// ── Funding state ─────────────────────────────────────────────────────────────

pub fn load_funding_state<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<FundingState, PerpError> {
    let buf = load_blob(context, funding_state_key(market_id))?;
    if buf.is_empty() {
        return Ok(FundingState::default());
    }
    decode(&buf)
}

pub fn save_funding_state<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    state: &FundingState,
) -> Result<(), PerpError> {
    let buf = encode(state)?;
    store_blob(context, funding_state_key(market_id), &buf)
}

// ── Insurance Fund ────────────────────────────────────────────────────────────

pub fn load_insurance_fund<H: PerpHost>(context: &mut H) -> Result<u64, PerpError> {
    let buf = load_blob(context, insurance_fund_key())?;
    if buf.is_empty() {
        return Ok(0);
    }
    decode(&buf)
}

pub fn save_insurance_fund<H: PerpHost>(
    context: &mut H,
    balance: u64,
) -> Result<(), PerpError> {
    let buf = encode(&balance)?;
    store_blob(context, insurance_fund_key(), &buf)
}

/// Absorb up to `deficit` from the insurance fund.
/// Returns `(absorbed, remaining_deficit)`.
/// If the fund covers everything, `remaining_deficit` is 0.
/// If the fund is insufficient, it is drained to zero and `remaining_deficit` > 0.
pub fn absorb_from_insurance_fund<H: PerpHost>(
    context: &mut H,
    deficit: u64,
) -> Result<(u64, u64), PerpError> {
    let balance = load_insurance_fund(context)?;
    let absorbed = deficit.min(balance);
    let remaining = deficit - absorbed;
    save_insurance_fund(context, balance - absorbed)?;
    Ok((absorbed, remaining))
}

// ── Signed-order replay guard (seen-signature set) ─────────────────────────────
// Replaces the order-map presence check as the replay witness for `placeOrderSigned`. Under
// delete-on-terminal a filled/cancelled signed order is DELETED, so its order-id no longer proves
// "this signature was already submitted" — this set does, in its own key namespace. The hot-path
// check is a single O(1) overlay lookup that REUSES the `keccak256(signature)` already computed for
// the order id (no extra hash). A seen marker is tiny; time-bucketing bounds the set to the recv
// window. Crucially, `check_recv_window` rejects any signature older than the window BEFORE the
// seen check runs, so a stale marker can never cause a false reject — GC exists purely to bound
// storage, and losing a marker late is harmless.

const SEEN_BUCKET_WIDTH_SECS: u64 = 15;
/// Buckets retained behind the current one before GC drops them. `RETENTION * WIDTH` must exceed
/// the max recv window (+ clock skew) so a bucket is dropped only once EVERY signature it could
/// hold is already recv-window-expired (hence its marker unreachable). 6 * 15 = 90s > 60s + 5s.
const SEEN_RETENTION_BUCKETS: u64 = 6;

/// Replay check: has this signature (by `keccak256(signature)`) already been submitted?
pub fn is_signature_seen<H: PerpHost>(
    context: &mut H,
    sig_hash: &[u8; 32],
) -> Result<bool, PerpError> {
    Ok(!load_blob(context, seen_sig_key(sig_hash))?.is_empty())
}

/// Records a signature as seen and indexes its seen-key in the GC bucket for the signature's OWN
/// timestamp (`sig_ts`, already validated inside the recv window) — so the marker is reclaimed a
/// fixed time after the signature was signed, independent of when it was submitted.
pub fn mark_signature_seen<H: PerpHost>(
    context: &mut H,
    sig_hash: &[u8; 32],
    sig_ts: u64,
) -> Result<(), PerpError> {
    let key = seen_sig_key(sig_hash);
    store_blob(context, key, &[1u8])?; // non-empty marker ('empty' == absent)
    let bucket = sig_ts / SEEN_BUCKET_WIDTH_SECS;
    let bkey = seen_bucket_key(bucket);
    let mut ids = unpack_order_ids(&load_blob(context, bkey)?)?;
    ids.push(key.0);
    store_blob(context, bkey, &pack_order_ids(&ids))
}

/// Lazy GC: drop the one bucket now `RETENTION` behind the block's current bucket, deleting every
/// seen marker it indexed and then the bucket itself. Bounded (O(bucket size)) per call; the
/// retention margin guarantees every signature in the dropped bucket is already recv-window-
/// expired. Under continuous signed traffic each bucket is visited exactly once; a gap longer than
/// one bucket width can skip a bucket, leaving harmless stale markers until a wipe (acceptable
/// pre-production — they never cause false rejects). Call after each accepted signed order.
pub fn gc_seen_buckets<H: PerpHost>(
    context: &mut H,
    block_ts: u64,
) -> Result<(), PerpError> {
    let cur = block_ts / SEEN_BUCKET_WIDTH_SECS;
    if cur < SEEN_RETENTION_BUCKETS {
        return Ok(());
    }
    let bkey = seen_bucket_key(cur - SEEN_RETENTION_BUCKETS);
    let ids = unpack_order_ids(&load_blob(context, bkey)?)?;
    if ids.is_empty() {
        return Ok(());
    }
    for id in &ids {
        store_blob(context, B256::new(*id), &[])?;
    }
    store_blob(context, bkey, &[])
}

// ── Premium index accumulator ─────────────────────────────────────────────────

pub fn load_premium_accumulator<H: PerpHost>(
    context: &mut H,
    market_id: u64,
) -> Result<PremiumIndexAccumulator, PerpError> {
    let buf = load_blob(context, premium_accumulator_key(market_id))?;
    if buf.is_empty() {
        return Ok(PremiumIndexAccumulator::default());
    }
    decode(&buf)
}

pub fn save_premium_accumulator<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    acc: &PremiumIndexAccumulator,
) -> Result<(), PerpError> {
    let buf = encode(acc)?;
    store_blob(context, premium_accumulator_key(market_id), &buf)
}

#[cfg(test)]
mod commitment_tests {
    use super::*;
    use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
    use database::InMemoryDB;
    use primitives::hardfork::SpecId;

    use context::ContextTr;

    type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

    /// Mirror of `perp_dex::trading::tests::make_ctx`: build a journal-backed context and warm the
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
}

#[cfg(test)]
mod size_probe_tests {
    use super::*;
    use crate::types::{
        MarginTiers, OrderStatus, OrderType, Side, TimeInForce, PRICE_BASIS_WINDOW_SIZE,
    };

    #[test]
    fn probe_encoded_sizes() {
        // Order — realistic BTC-ish values
        let order = Order {
            owner: [0xAB; 20],
            market_id: 1,
            side: Side::Buy,
            price: 65_432_10,      // 7 digits
            quantity: 150_000_000, // 1.5 BTC @ 8 decimals
            filled: 50_000_000,
            order_type: OrderType::Limit,
            tif: TimeInForce::Gtc,
            status: OrderStatus::PartiallyFilled,
        };
        let buf = encode(&order).unwrap();
        println!(
            "Order: {} bytes; hex={}",
            buf.len(),
            primitives::hex::encode(&buf)
        );

        let entry = OrderEntry {
            order_id: [0xCD; 32],
            price: 65_432_10,
            amount: 150_000_000,
            maker_fee_bps: 2,
            assuming_price: 6_550_000,
        };
        println!(
            "OrderEntry x1 (in vec): {} bytes",
            encode(&vec![entry]).unwrap().len()
        );
        println!(
            "OrderEntry x5: {} bytes",
            encode(&vec![entry; 5]).unwrap().len()
        );
        println!(
            "OrderEntry x20: {} bytes",
            encode(&vec![entry; 20]).unwrap().len()
        );
        println!(
            "OrderEntry single hex={}",
            primitives::hex::encode(encode(&entry).unwrap())
        );

        let prices: Vec<u64> = (0..1u64).map(|i| 65_000_00 + i * 10).collect();
        println!("bid_prices x1: {} bytes", encode(&prices).unwrap().len());
        let prices: Vec<u64> = (0..10u64).map(|i| 65_000_00 + i * 10).collect();
        println!("bid_prices x10: {} bytes", encode(&prices).unwrap().len());
        let prices: Vec<u64> = (0..100u64).map(|i| 65_000_00 + i * 10).collect();
        println!("bid_prices x100: {} bytes", encode(&prices).unwrap().len());

        let q: Vec<[u8; 32]> = vec![[0xEF; 32]; 1];
        println!("level queue x1: {} bytes", encode(&q).unwrap().len());
        let q: Vec<[u8; 32]> = vec![[0xEF; 32]; 5];
        println!("level queue x5: {} bytes", encode(&q).unwrap().len());
        println!(
            "level queue single elem hex={}",
            primitives::hex::encode(encode(&[0xEFu8; 32]).unwrap())
        );

        let pos = PerpPosition {
            amount: 150_000_000,
            v_quote_balance: -98_148_315,
            margin: 9_814_831,
            leverage: 10,
            last_funding_index: 123_456_789_012_345i128,
            total_buy_qty: 60_000_000,
            total_buy_notional: 50_000_000,
            total_sell_qty: 12_000_000,
            total_sell_notional: 10_000_000,
        };
        let buf = encode(&pos).unwrap();
        println!(
            "PerpPosition: {} bytes; hex={}",
            buf.len(),
            primitives::hex::encode(&buf)
        );
        println!(
            "PerpPosition default: {} bytes",
            encode(&PerpPosition::default()).unwrap().len()
        );

        let acct = UserAccount {
            usdc_balance: "123456789000000000000".into(), // 21-digit decimal string
            perp_wallet_balance: 1_234_567_890,
            maker_fee_bps: 2,
            taker_fee_bps: 5,
            nonce: 7,
            total_position_margin: 4_200_000,
        };
        let buf = encode(&acct).unwrap();
        println!(
            "UserAccount: {} bytes; hex={}",
            buf.len(),
            primitives::hex::encode(&buf)
        );

        let market = Market {
            market_id: 1,
            base_decimals: 8,
            price_decimals: 2,
            tick_size: 10,
            step_size: 1000,
            min_quantity: 1000,
            max_quantity: 10_000_000_000,
            max_price: 100_000_000,
            price_update_interval: 1,
            active: true,
            funding_interval: 28_800,
            interest_rate: 100,
            liquidation_fee_rate_bps: 50,
            price_band_bps: 0,
            mark_price: 0,
            tiers: MarginTiers::default(),
        };
        let buf = encode(&market).unwrap();
        println!("Market: {} bytes", buf.len());

        let fs = FundingState {
            last_funding_rate: 125,
            next_funding_ts: 1_750_000_000,
            cumulative_funding_index: 9_876_543_210_123i128,
        };
        println!("FundingState: {} bytes", encode(&fs).unwrap().len());

        let acc = PremiumIndexAccumulator {
            weighted_sum: 123_456_789_012i128,
            sample_count: 28_000,
            epoch_start_ts: 1_750_000_000,
            last_pi: -1234,
            last_sample_ts: 1_750_000_123,
        };
        println!(
            "PremiumIndexAccumulator: {} bytes",
            encode(&acc).unwrap().len()
        );

        let mut window = PriceBasisWindow::default();
        for i in 0..PRICE_BASIS_WINDOW_SIZE as u64 {
            let _ = window.record_observation(1_750_000_000 + i, 65_000_00 + i);
        }
        println!(
            "PriceBasisWindow full: {} bytes",
            encode(&window).unwrap().len()
        );
        println!(
            "PriceBasisWindow empty: {} bytes",
            encode(&PriceBasisWindow::default()).unwrap().len()
        );

        let mut hist = IndexPriceHistory::default();
        for i in 0..32u64 {
            hist.push(
                IndexPriceState {
                    index_price: 65_000_00 + i,
                    timestamp: 1_750_000_000 + i,
                },
                32,
            );
        }
        println!(
            "IndexPriceHistory x32: {} bytes",
            encode(&hist).unwrap().len()
        );

        println!(
            "UserFeeRates: {} bytes",
            encode(&UserFeeRates {
                maker_fee_bps: 2,
                taker_fee_bps: 5
            })
            .unwrap()
            .len()
        );
        println!(
            "u64 scalar (mark price 6_543_210): {} bytes",
            encode(&6_543_210u64).unwrap().len()
        );
        println!(
            "u64 scalar small (nonce 7): {} bytes",
            encode(&7u64).unwrap().len()
        );
        println!("Address: {} bytes", encode(&Address::ZERO).unwrap().len());
        println!(
            "ApiKey: {} bytes",
            encode(&ApiKey {
                pubkey: [9; 32],
                expiry: 1_750_000_000
            })
            .unwrap()
            .len()
        );
    }
}

/// Permanent guard for the P4/#20 blob encoding (positional msgpack + serde_bytes byte-arrays +
/// serde_repr integer enums + raw-packed order-id queues). The golden commitment test only detects
/// DRIFT (any byte change re-pins it); these `decode(encode(v)) == v` checks catch a SELF-CONSISTENT
/// mis-round-trip on a field the golden scenario never stresses — negative i128s, bytes > 0x7F in
/// fixed arrays, every enum discriminant, defaulted middle fields at extremes. Value-equality is
/// required: for positional encoding `encode∘decode` is a byte-identity, so a byte round-trip would
/// be vacuously true.
#[cfg(test)]
mod encoding_roundtrip_tests {
    use super::*;
    use crate::types::{MarginTiers, OrderStatus, OrderType, Side, TimeInForce};

    fn rt<T>(label: &str, v: T)
    where
        T: serde::Serialize + for<'de> Deserialize<'de> + PartialEq + core::fmt::Debug,
    {
        let buf = encode(&v).unwrap();
        let back: T = decode(&buf).unwrap();
        assert_eq!(v, back, "round-trip mismatch for {label}");
    }

    #[test]
    fn order_every_enum_combo_and_high_byte_owner() {
        for side in [Side::Buy, Side::Sell] {
            for ot in [OrderType::Limit, OrderType::Market] {
                for tif in [
                    TimeInForce::Gtc,
                    TimeInForce::Ioc,
                    TimeInForce::Fok,
                    TimeInForce::PostOnly,
                ] {
                    for status in [
                        OrderStatus::Open,
                        OrderStatus::PartiallyFilled,
                        OrderStatus::Filled,
                        OrderStatus::Cancelled,
                        OrderStatus::Expired,
                    ] {
                        // owner spans the full byte domain incl. > 0x7F.
                        let mut owner = [0u8; 20];
                        for (i, b) in owner.iter_mut().enumerate() {
                            *b = (0x80 + i as u8) ^ 0xA5;
                        }
                        rt(
                            "Order",
                            Order {
                                owner,
                                market_id: u64::MAX,
                                side,
                                price: u64::MAX,
                                quantity: u64::MAX - 1,
                                filled: 1,
                                order_type: ot,
                                tif,
                                status,
                            },
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn order_entry_and_api_key_high_byte_arrays() {
        let id: [u8; 32] = core::array::from_fn(|i| (0xFF - i as u8) ^ 0x3C);
        rt(
            "OrderEntry",
            OrderEntry {
                order_id: id,
                price: u64::MAX,
                amount: u64::MAX,
                maker_fee_bps: 12_345,
                assuming_price: u64::MAX,
            },
        );
        rt(
            "ApiKey",
            ApiKey {
                pubkey: id,
                expiry: u64::MAX,
            },
        );
        rt(
            "ApiKey-never-expires",
            ApiKey {
                pubkey: [0xFF; 32],
                expiry: 0,
            },
        );
    }

    #[test]
    fn perp_position_signed_extremes() {
        rt(
            "PerpPosition",
            PerpPosition {
                amount: i64::MIN,
                v_quote_balance: i64::MAX,
                margin: -1,
                leverage: 20,
                last_funding_index: i128::MIN,
                total_buy_qty: u64::MAX,
                total_buy_notional: u64::MAX,
                total_sell_qty: 0,
                total_sell_notional: u64::MAX,
            },
        );
        rt(
            "PerpPosition-pos-i128",
            PerpPosition {
                last_funding_index: i128::MAX,
                ..PerpPosition::default()
            },
        );
    }

    #[test]
    fn market_all_fields_incl_defaulted() {
        rt(
            "Market",
            Market {
                market_id: u64::MAX,
                base_decimals: u32::MAX,
                price_decimals: 18,
                tick_size: u64::MAX,
                step_size: 1,
                min_quantity: 1,
                max_quantity: u64::MAX,
                max_price: u64::MAX,
                price_update_interval: 15,
                active: true,
                funding_interval: 3600,
                interest_rate: i64::MIN,
                liquidation_fee_rate_bps: u32::MAX,
                price_band_bps: 0,
                mark_price: u64::MAX,
                tiers: MarginTiers::default(),
            },
        );
    }

    #[test]
    fn funding_and_premium_signed_i128() {
        rt(
            "FundingState",
            FundingState {
                last_funding_rate: i64::MIN,
                next_funding_ts: u64::MAX,
                cumulative_funding_index: i128::MIN,
            },
        );
        rt(
            "PremiumIndexAccumulator",
            PremiumIndexAccumulator {
                weighted_sum: i128::MIN,
                sample_count: u64::MAX,
                epoch_start_ts: 1,
                last_pi: i64::MIN,
                last_sample_ts: u64::MAX,
            },
        );
    }

    #[test]
    fn enum_out_of_range_discriminant_errors_cleanly() {
        // Each enum round-trips its real discriminants...
        for s in [OrderStatus::Open, OrderStatus::Filled, OrderStatus::Expired] {
            let buf = encode(&s).unwrap();
            assert_eq!(decode::<OrderStatus>(&buf).unwrap(), s);
        }
        // ...and an out-of-range discriminant decodes to a clean Err, never UB/panic.
        // 99 encodes as a 1-byte msgpack positive fixint (0x63); no OrderStatus variant == 99.
        assert!(
            decode::<OrderStatus>(&[0x63]).is_err(),
            "out-of-range enum discriminant must error, not panic/UB"
        );
        assert!(decode::<Side>(&[0x05]).is_err());
    }

    #[test]
    fn order_id_queue_pack_unpack_roundtrip_and_rejects_misaligned() {
        for q in [
            vec![],
            vec![[0xFFu8; 32]],
            vec![
                [0x00u8; 32],
                [0x80u8; 32],
                core::array::from_fn(|i| i as u8),
            ],
        ] {
            let packed = pack_order_ids(&q);
            assert_eq!(packed.len(), q.len() * 32);
            assert_eq!(
                unpack_order_ids(&packed).unwrap(),
                q,
                "pack/unpack must round-trip"
            );
        }
        // A blob whose length is not a multiple of 32 is rejected, never silently truncated.
        for bad_len in [1usize, 31, 33, 63] {
            assert!(
                unpack_order_ids(&vec![0xABu8; bad_len]).is_err(),
                "len {bad_len} must error"
            );
        }
    }
}
