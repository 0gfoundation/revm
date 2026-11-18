//! Q2: the `updateIndexPrice` timestamp is bounded from above by the block clock
//! (`MAX_ORACLE_FUTURE_SKEW_SECS`), on the RAW value as sent, before the staleness no-op; there is
//! no lower bound.

use super::*;
use crate::risk::{MAX_ORACLE_FUTURE_SKEW_SECS, ORACLE_FUTURE_TIMESTAMP_REJECT};
use alloy_sol_types::SolError;

const GAS: u64 = 30_000_000;

fn push_input(ts: u64) -> Vec<u8> {
    updateIndexPriceCall { marketId: MARKET_ID, indexPrice: ENTRY_PRICE, timestamp: ts }
        .abi_encode()
}

fn stored_index_ts(ctx: &mut TestCtx) -> u64 {
    storage::load_index_price_state(ctx, MARKET_ID).unwrap().timestamp
}

fn index_state(ctx: &mut TestCtx) -> (u64, u64) {
    let s = storage::load_index_price_state(ctx, MARKET_ID).unwrap();
    (s.index_price, s.timestamp)
}

/// `updateIndexPrice(ts)` by `caller` through the call shell, asserted REFUSED with exactly
/// `reason`: no log, no perp write (so the commit-only tripwire stays quiet), the index state and
/// the funding state as they were.
fn refused_as(ctx: &mut TestCtx, caller: Address, ts: u64, reason: &str) {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let index_before = index_state(ctx);
    let funding_before = storage::load_funding_state(ctx, MARKET_ID).unwrap();
    let writes = ctx.perp_write_count();
    let out = run_perp_dex_call(&push_input(ts), GAS, caller, U256::ZERO, false, ctx).unwrap();
    assert!(out.reverted, "ts {ts}: expected a revert");
    assert_eq!(
        alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason).unwrap(),
        reason,
        "ts {ts}"
    );
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty(), "ts {ts}: a refused push logged");
    assert_eq!(ctx.perp_write_count(), writes, "ts {ts}: a refused push wrote");
    assert_eq!(index_state(ctx), index_before, "ts {ts}: the index state moved");
    assert_eq!(storage::load_funding_state(ctx, MARKET_ID).unwrap(), funding_before);
}

fn refused(ctx: &mut TestCtx, ts: u64) {
    refused_as(ctx, ADMIN, ts, ORACLE_FUTURE_TIMESTAMP_REJECT);
}

/// `updateIndexPrice(ts)` through the call shell, asserted ACCEPTED; its `MarkPriceUpdated`.
fn accepted(ctx: &mut TestCtx, ts: u64) -> crate::interface::IPerpDex::MarkPriceUpdated {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let out = run_perp_dex_call(&push_input(ts), GAS, ADMIN, U256::ZERO, false, ctx).unwrap();
    assert!(
        !out.reverted,
        "ts {ts}: reverted with {:?}",
        alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason)
    );
    let mut events = mark_price_events(&JournalTr::take_logs(ctx.journal_mut()));
    assert_eq!(events.len(), 1, "ts {ts}: one price event");
    events.pop().unwrap()
}

fn ctx_at(block_ts: u64) -> TestCtx {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    ctx.block.timestamp = U256::from(block_ts);
    ctx
}

/// T1. The boundary, exact: `block + 61` is refused, `block + 60` accepted. It is the RAW stamp
/// that is bounded: `1_000_061` floors to `1_000_050` — the very window `1_000_060` then publishes
/// — so a bound on the floored window would have admitted it.
#[test]
fn a_stamp_more_than_the_skew_ahead_of_the_block_is_refused_and_one_at_it_accepted() {
    let mut ctx = ctx_at(1_000_000);
    assert_eq!(MAX_ORACLE_FUTURE_SKEW_SECS, 60);

    refused(&mut ctx, 1_000_061);
    assert_eq!(stored_index_ts(&mut ctx), 0, "nothing was published");
    assert_eq!(1_000_061 / 15 * 15, 1_000_050, "the refused stamp's window is the one below");

    let e = accepted(&mut ctx, 1_000_060);
    assert_eq!(stored_index_ts(&mut ctx), 1_000_050);
    assert_eq!(e.priceWindowTs, 1_000_050);
}

/// T2. The mistake the bound exists for: a feed that stamps MILLISECONDS. Refused, nothing moves;
/// the same feed in seconds is then accepted as usual.
#[test]
fn a_millisecond_stamp_is_refused() {
    let mut ctx = ctx_at(1_700_000_000);
    refused(&mut ctx, 1_700_000_000_000);
    assert_eq!(stored_index_ts(&mut ctx), 0);

    accepted(&mut ctx, 1_700_000_000);
    assert_eq!(stored_index_ts(&mut ctx), 1_699_999_995);
}

/// T3. After a real push, `u64::MAX` is refused loudly — the bound runs before the staleness
/// no-op, and the block-side `saturating_add` cannot wrap it into range.
#[test]
fn the_largest_stamp_is_refused_after_a_real_push() {
    let mut ctx = ctx_at(1_000_000);
    accepted(&mut ctx, 1_000_000);
    assert_eq!(stored_index_ts(&mut ctx), 999_990);
    refused(&mut ctx, u64::MAX);
    assert_eq!(stored_index_ts(&mut ctx), 999_990, "the published window stands");

    // At the very top of the block clock too: `block + 60` saturates instead of wrapping.
    ctx.block.timestamp = U256::from(u64::MAX - 30);
    accepted(&mut ctx, u64::MAX);
}

/// T4. No lower bound: a stamp far BEHIND the block is accepted (here the first one, so it
/// publishes); an older one after it is the usual silent stale no-op.
#[test]
fn an_old_stamp_is_accepted_and_a_stale_window_still_ignored() {
    let mut ctx = ctx_at(1_000_000);
    accepted(&mut ctx, 15);
    assert_eq!(stored_index_ts(&mut ctx), 15);

    let _ = JournalTr::take_logs(ctx.journal_mut());
    let writes = ctx.perp_write_count();
    let out = run_perp_dex_call(&push_input(29), GAS, ADMIN, U256::ZERO, false, &mut ctx).unwrap();
    assert!(!out.reverted, "a stale window is ignored, not refused");
    assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
    assert_eq!(ctx.perp_write_count(), writes);
    assert_eq!(stored_index_ts(&mut ctx), 15);
}

/// The caller check still runs first: a non-oracle caller with a far-future stamp is told it is
/// not authorised, not that its stamp is early.
#[test]
fn authorisation_is_checked_before_the_stamp() {
    let mut ctx = ctx_at(1_000_000);
    refused_as(&mut ctx, ALICE, u64::MAX, "not authorised: caller is not admin or oracle");
}

/// T5. The reject string states the constant it enforces.
#[test]
fn the_reject_reason_names_the_skew() {
    assert!(ORACLE_FUTURE_TIMESTAMP_REJECT.contains(&MAX_ORACLE_FUTURE_SKEW_SECS.to_string()));
    assert!(ORACLE_FUTURE_TIMESTAMP_REJECT.starts_with("updateIndexPrice: "));
}
