//! Margin type — Binance's `marginType` (CROSSED / ISOLATED), cross-margin series P1, live since
//! the P3 flip.
//!
//! # The gate
//!
//! Every position record carries the owner's STORED choice ([`PerpPosition::margin_type`]); this
//! module owns the rule that turns it into the type IN FORCE ([`effective_margin_type`]). The rule
//! is gated by [`CROSS_MARGIN_ENABLED`]: it was `false` through P1, P2 and P3's build-up — every
//! position ISOLATED in force, every margin rule, view and event as before the field existed —
//! and flipped to `true` as the last step of P3, once cross accounting (P2) and cross liquidation
//! (P3) existed. Since then the stored choice is in force, and a position whose owner never chose
//! is CROSSED, Binance's default ([`never_set_position`]).
//!
//! Every reader of the type goes through [`effective_margin_type`] — the `AccountPosition` row,
//! `PositionChanged` and `getSymbolConfig` today — so the gate lives in one place and the three
//! surfaces cannot disagree.
//!
//! # The switch
//!
//! `setMarginType` / `setMarginTypeSigned` ([`run_set_margin_type`],
//! [`run_set_margin_type_signed`]) are Binance's `POST /fapi/v1/marginType`: validate-then-apply
//! (commit-only #23), every refusal before the one write, the write through
//! [`storage::save_position_margin_type_only`] (no hooks, no snapshot), then `MarginTypeChanged`.
//! Only a flat, order-free market can switch, so nothing a balance row carries can move. The gate
//! is the switch's last refusal: with it off (a test's override; the build before the flip) the
//! switch changes nothing — ISOLATED is already in force (`-4046`) and CROSSED is refused.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use primitives::{keccak256, Address, Bytes, Log};

use crate::{
    errors::perp_err,
    host::PerpHost,
    interface::IPerpDex::{self, setMarginTypeCall, setMarginTypeSignedCall},
    storage,
    trading::{check_api_key_expiry, check_recv_window, gc_seen_buckets_best_effort, verify_ed25519},
    types::{MarginType, PerpPosition, CROSS_MARGIN_ENABLED},
    PerpError, PERP_DEX_ADDRESS,
};

/// Whether cross margin is live: [`CROSS_MARGIN_ENABLED`], a plain `const` in every non-test
/// build. Under `cfg(test)` a test may force it on or off for its own thread
/// ([`test_gate::enable`] / [`test_gate::disable`]); that hook does not exist outside test builds.
#[inline]
pub fn cross_margin_enabled() -> bool {
    #[cfg(test)]
    if let Some(forced) = test_gate::forced() {
        return forced;
    }
    CROSS_MARGIN_ENABLED
}

/// What a `(user, market)` that was never written reads as: [`PerpPosition::default`], whose
/// margin type is CROSSED — Binance's default ("All contracts and positions are defaulted to the
/// Cross Margin mode"). The position loaders (`storage::load_position` / `load_position_ref`)
/// return it for an absent record, so it is also what that position's first save stores.
///
/// Under `cfg(test)` a fixture may make the never-set type ISOLATED for its thread
/// ([`test_gate::isolated_by_default`]); outside test builds this is exactly
/// [`PerpPosition::default`].
#[inline]
pub(crate) fn never_set_position() -> PerpPosition {
    #[cfg(test)]
    if let Some(t) = test_gate::never_set_type() {
        return PerpPosition { margin_type: t.code(), ..PerpPosition::default() };
    }
    PerpPosition::default()
}

/// The margin type IN FORCE for `pos` — the ONE implementation of the gate
/// ([`MarginType::effective`] under [`cross_margin_enabled`]). Total: no error path, so it is safe
/// under views, events and the liquidation sweep.
#[inline]
pub fn effective_margin_type(pos: &PerpPosition) -> MarginType {
    MarginType::effective(pos.margin_type, cross_margin_enabled())
}

/// `#[cfg(test)]`-only hooks on the margin-type rule, per THREAD (the test harness runs each test
/// on its own thread, so nothing one test sets can leak into another). Two independent hooks:
///
/// * **The gate** — [`enable`] / [`disable`] force [`cross_margin_enabled`] on or off until the
///   returned guard drops. A guard restores whatever was in force before it, so they nest: a test
///   can run the isolated engine with the gate forced OFF and compare it, inside a nested scope,
///   against the same scenario with the gate forced ON.
/// * **The never-set default** — [`isolated_by_default`]: from then on, on this thread, a position
///   that was never written reads, and is therefore first stored, as ISOLATED instead of Binance's
///   CROSSED ([`never_set_position`]). It changes the STORED byte, not the gate: the position is
///   ISOLATED exactly as if its owner had called `setMarginType(ISOLATED)` on the flat market, and
///   every rule runs under the real gate. The fixtures of the suites written for isolated margin
///   call it, so their accounts stay isolated by an explicit choice rather than by the gate.
#[cfg(test)]
pub(crate) mod test_gate {
    use core::cell::Cell;

    use crate::types::MarginType;

    std::thread_local! {
        static FORCED: Cell<Option<bool>> = const { Cell::new(None) };
        static NEVER_SET: Cell<Option<MarginType>> = const { Cell::new(None) };
    }

    /// The gate this thread forces, if any; `None` = the real [`super::CROSS_MARGIN_ENABLED`].
    pub(crate) fn forced() -> Option<bool> {
        FORCED.with(Cell::get)
    }

    /// Forces cross margin ON on this thread until the returned guard drops.
    #[must_use = "the gate is forced only while the guard is alive"]
    pub(crate) fn enable() -> Guard {
        force(true)
    }

    /// Forces cross margin OFF on this thread until the returned guard drops: every position is
    /// ISOLATED in force whatever it stores, and `setMarginType(CROSSED)` is refused.
    #[must_use = "the gate is forced only while the guard is alive"]
    pub(crate) fn disable() -> Guard {
        force(false)
    }

    fn force(on: bool) -> Guard {
        Guard(FORCED.with(|c| c.replace(Some(on))))
    }

    /// Restores the gate that was in force when it was created.
    pub(crate) struct Guard(Option<bool>);

    impl Drop for Guard {
        fn drop(&mut self) {
            FORCED.with(|c| c.set(self.0));
        }
    }

    /// Makes every never-written position on this thread ISOLATED (see the module doc). For the
    /// rest of the thread — i.e. of the test — so a fixture can call it without handing back a
    /// guard; calling it again is a no-op.
    pub(crate) fn isolated_by_default() {
        NEVER_SET.with(|c| c.set(Some(MarginType::Isolated)));
    }

    /// The never-set margin type this thread overrides, if any.
    pub(crate) fn never_set_type() -> Option<MarginType> {
        NEVER_SET.with(Cell::get)
    }
}

// ── setMarginType / setMarginTypeSigned ───────────────────────────────────────

/// Refusal 0: the requested code must name a type. Calldata-only, so it precedes every load (and,
/// on the signed path, the signature burn).
fn requested_type(code: u8) -> Result<MarginType, PerpError> {
    MarginType::from_code(code)
        .ok_or_else(|| perp_err("setMarginType: invalid marginType (0 = CROSSED, 1 = ISOLATED)"))
}

/// `setMarginType(uint64 marketId, uint8 marginType)`
pub fn run_set_margin_type<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setMarginTypeCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setMarginType: invalid calldata"))?;
    let requested = requested_type(args.marginType)?;
    set_margin_type_core(context, caller, args.marketId, requested)
}

/// `setMarginTypeSigned(address account, uint64 marketId, uint8 marginType, uint64 timestamp,
/// uint64 recvWindow, uint8 keyId, bytes signature)`
///
/// # Where the signature burn sits — and why there
///
/// Same convention as `placeAlgoOrderSigned`: the refusals that are PERMANENT for this message run
/// before the burn, so a malformed request costs no write; everything that reads the owner's state
/// (or the gate) runs after it, in [`set_margin_type_core`], because it can lift later — a refused
/// signature left unspent would be replayable for the rest of its recv window, the moment the
/// owner's last order or position went away.
///
/// * BEFORE: decode, refusal 0 (an invalid type code), key / recvWindow / expiry / signature, the
///   duplicate check, refusal 1 (unknown market — markets are never deleted).
/// * AFTER, inside the shared core, in exactly the direct path's order and with its strings:
///   `-4046`, `-4047`, `-4048`, `-2023`, the gate. The gate is constant for a given build
///   (it lifted at the P3 flip) and it is the LAST check of the shared core, so keeping it there
///   keeps the two paths' refusal precedence identical; a signature burned on it is one the owner
///   signed for a mode the build does not offer.
pub fn run_set_margin_type_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setMarginTypeSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setMarginTypeSigned: invalid calldata"))?;
    let requested = requested_type(args.marginType)?;

    let api_key = storage::load_api_key(context, args.account, args.keyId)?
        .ok_or_else(|| perp_err("setMarginTypeSigned: no api key registered for account"))?;
    check_recv_window(context, args.timestamp, args.recvWindow)
        .map_err(|e| perp_err(&format!("setMarginTypeSigned: {e}")))?;
    check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("setMarginTypeSigned: {e}")))?;

    // Canonical fixed-layout message; the array literal and the closing assert carry the length.
    //   "perpdex_v1_margintype" || chainId(8) || account(20) || marketId(8) || marginType(1)
    //   || timestamp(8) || recvWindow(8) || keyId(1)
    let mut msg = [0u8; 75];
    let mut c = crate::trading::write_signed_header(
        &mut msg,
        b"perpdex_v1_margintype",
        context.chain_id(),
    );
    c = crate::trading::put(&mut msg, c, args.account.as_slice());
    c = crate::trading::put(&mut msg, c, &args.marketId.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[args.marginType]);
    c = crate::trading::put(&mut msg, c, &args.timestamp.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &args.recvWindow.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[args.keyId]);
    debug_assert_eq!(c, msg.len(), "setMarginTypeSigned message layout");

    verify_ed25519(&api_key.pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(&format!("setMarginTypeSigned: {e}")))?;

    let sig_hash: [u8; 32] = keccak256(args.signature.as_ref()).0;
    if storage::is_signature_seen(context, &sig_hash)? {
        return Err(perp_err(
            "setMarginTypeSigned: duplicate signature (already submitted)",
        ));
    }
    // Refusal 1, pre-burn: permanent for this message (see the function doc).
    if storage::load_market_ref(context, args.marketId)?.is_none() {
        return Err(perp_err("setMarginType: unknown market"));
    }

    // ── last pre-write fault has passed; the first write happens here ──
    let block_ts: u64 = context.timestamp();
    let writes_before_burn = context.perp_write_count();
    storage::mark_signature_seen(context, &sig_hash, args.timestamp)?;
    gc_seen_buckets_best_effort(context, block_ts)?;
    let burn_writes = context.perp_write_count().saturating_sub(writes_before_burn);

    set_margin_type_core(context, args.account, args.marketId, requested)
        .map_err(|e| e.after_retained_writes(burn_writes))
}

/// The switch, shared by both entry points: Binance's refusal order (cross report §3.6), every
/// refusal before the one write.
fn set_margin_type_core<H: PerpHost>(
    context: &mut H,
    account: Address,
    market_id: u64,
    requested: MarginType,
) -> Result<Bytes, PerpError> {
    // 1. The market. (The signed path checked it before its burn; this re-read is a resident hit.)
    if storage::load_market_ref(context, market_id)?.is_none() {
        return Err(perp_err("setMarginType: unknown market"));
    }
    let mut pos = storage::load_position(context, account, market_id)?;

    // 2. Already in force — the EFFECTIVE type, the one `getSymbolConfig` reports, not the stored
    //    byte: a record that never chose is CROSSED in force (Binance's default), so asking for
    //    CROSSED is Binance's "no need". (With the gate off everyone is ISOLATED in force, and
    //    ISOLATED is "no need" even for a record that stores the CROSSED default.)
    if effective_margin_type(&pos) == requested {
        return Err(perp_err("setMarginType: -4046 No need to change margin type."));
    }

    // 3. Any open order in this market: the resting book orders on both sides AND the untriggered
    //    conditional / trailing orders — the same set the per-market open-order cap counts
    //    (`storage::ensure_open_order_capacity`). Including conditionals is our conservative
    //    reading; Binance does not say. A position-bound conditional whose position has since
    //    closed is still in the list (its expiry is lazy) and still blocks.
    if storage::book_order_count(context, account, market_id)? != 0
        || !storage::load_user_algo_orders(context, account, market_id)?.is_empty()
    {
        return Err(perp_err(
            "setMarginType: -4047 Margin type cannot be changed if there exists open orders.",
        ));
    }

    // 4. A position.
    if pos.amount != 0 {
        return Err(perp_err(
            "setMarginType: -4048 Margin type cannot be changed if there exists position.",
        ));
    }

    // 5. The account is in liquidation (`UserAccount::in_liquidation`, the
    //    `LIQ_FLAG_IN_LIQUIDATION` bit of `liq_flags`): Binance's `-2023`. One account `_ref`
    //    load. Set and cleared by cross liquidation (`risk::cross_liquidation`).
    if storage::load_account_ref(context, account)?.in_liquidation() {
        return Err(perp_err("setMarginType: -2023 User in liquidation mode now."));
    }

    // 6. The gate: CROSSED cannot be chosen while cross margin is off. Live since the P3 flip, so
    //    production never reaches it; kept as the gate's last refusal for a build (or a test's
    //    override) that turns it off.
    if requested == MarginType::Crossed && !cross_margin_enabled() {
        return Err(perp_err("setMarginType: CROSSED margin is not enabled yet"));
    }

    // ── every refusal has passed; the one write ──
    pos.margin_type = requested.code();
    storage::save_position_margin_type_only(context, account, market_id, pos)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::MarginTypeChanged {
            user: account,
            marketId: market_id,
            marginType: requested.code(),
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

#[cfg(test)]
#[path = "margin_type_tests.rs"]
mod tests;
