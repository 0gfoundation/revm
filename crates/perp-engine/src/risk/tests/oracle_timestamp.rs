//! Q2: the `updateIndexPrice` timestamp is bounded from above by the block clock
//! (`MAX_ORACLE_FUTURE_SKEW_SECS`), on the RAW value as sent, before the staleness no-op; there is
//! no lower bound.

use super::*;
use crate::risk::{
    MAX_MARKET_INTERVAL_SECS, MAX_ORACLE_FUTURE_SKEW_SECS, ORACLE_FUTURE_TIMESTAMP_REJECT,
};
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

/// T6. A year of oracle silence on one-second epochs, then a stamp at the millisecond scale (the
/// block clock with it, so the bound admits it): each push settles exactly ONE epoch at its own
/// mark and lands `next_funding_ts` on the first boundary after it, in one step. The catch-up loop
/// this replaced iterated once per missed epoch — 3.15e7 times for the year, ~1.7e12 for the
/// second push, which would not finish; the closed form answers both at once (a structural guard:
/// no timing assert).
///
/// Mark = index = $100 throughout (no book, no trade), so every epoch's average premium is 0 and
/// its rate `calc_funding_rate(0, 100) = 100`, folding `10_000 × 100 = 1_000_000` into the index.
#[test]
fn a_year_of_oracle_silence_settles_one_epoch_and_jumps_to_the_next_boundary() {
    use crate::interface::IPerpDex::FundingRateComputed;

    let mut ctx = ctx_at(1);
    let mut market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    market.price_update_interval = 1;
    market.funding_interval = 1;
    market.interest_rate = 100;
    storage::save_market(&mut ctx, &market).unwrap();
    assert_eq!(crate::math::calc_funding_rate(0, 100), 100);

    let push = |ctx: &mut TestCtx, ts: u64| {
        ctx.block.timestamp = U256::from(ts);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        run_update_index_price(&push_input(ts), ADMIN, ctx).unwrap();
        JournalTr::take_logs(ctx.journal_mut())
    };

    push(&mut ctx, 1_000);
    assert_eq!(storage::load_funding_state(&mut ctx, MARKET_ID).unwrap().next_funding_ts, 1_001);

    let t1 = 1_000 + 31_536_000;
    let logs = push(&mut ctx, t1);
    let f = storage::load_funding_state(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(
        (f.next_funding_ts, f.last_funding_rate, f.cumulative_funding_index),
        (t1 + 1, 100, 1_000_000),
        "one epoch settled at mark × rate, the next boundary the first after the push"
    );
    let acc = storage::load_premium_accumulator(&mut ctx, MARKET_ID).unwrap();
    assert_eq!((acc.sample_count, acc.epoch_start_ts, acc.last_sample_ts), (1, t1, t1));
    let computed = decode_events::<FundingRateComputed>(&logs);
    assert_eq!(computed.len(), 1, "exactly one epoch computed");
    assert_eq!(
        (computed[0].fundingRate, computed[0].avgPremiumIndex, computed[0].sampleCount),
        (100, 0, 1)
    );
    assert_eq!(computed[0].timestamp, t1);
    let mark = mark_price_events(&logs);
    assert_eq!(mark.len(), 1);
    assert_eq!(mark[0].nextFundingTime, t1 + 1);

    let t2 = 1_700_000_000_000;
    let logs = push(&mut ctx, t2);
    let f = storage::load_funding_state(&mut ctx, MARKET_ID).unwrap();
    assert_eq!((f.next_funding_ts, f.cumulative_funding_index), (t2 + 1, 2_000_000));
    assert_eq!(decode_events::<FundingRateComputed>(&logs).len(), 1);
    assert_eq!(mark_price_events(&logs)[0].nextFundingTime, t2 + 1);
}

// ── Write-clean funding failures and interval bounds ─────────────────────────────────────────────

/// The largest multiple of 15: a `fundingInterval` that overflows the funding clock on any push.
const HUGE_FUNDING_INTERVAL: u64 = u64::MAX - u64::MAX % 15;

/// The funding step's refusals are write-clean: every fallible step of the premium sample and
/// the epoch rollover runs before the push's first write (commit-only #23). Driven by the funding
/// clock overflowing on a huge interval, with the market saved directly — `updateMarket` now
/// refuses such an interval ([`MAX_MARKET_INTERVAL_SECS`]): one set BEFORE the first push
/// overflows `window + interval`; one set AFTER funding started overflows the rollover's next
/// boundary. Both used to write the index, mode and mark and then revert, which
/// the call shell's tripwire turns into a panic in a debug build.
#[test]
fn a_push_whose_funding_step_fails_is_refused_with_nothing_written() {
    const OVERFLOW: &str = "index price update: next funding timestamp overflow";
    let set_funding_interval = |ctx: &mut TestCtx, fi: u64| {
        let mut market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
        market.funding_interval = fi;
        storage::save_market(ctx, &market).unwrap();
    };
    let mark = |ctx: &mut TestCtx| storage::load_mark_price(ctx, MARKET_ID).unwrap();

    // Before the first push: `effective_timestamp + interval`.
    let mut ctx = ctx_at(1_000_000);
    set_funding_interval(&mut ctx, HUGE_FUNDING_INTERVAL);
    let mark_before = mark(&mut ctx);
    refused_as(&mut ctx, ADMIN, 1_000_000, OVERFLOW);
    assert_eq!(mark(&mut ctx), mark_before);
    assert_eq!(storage::load_premium_accumulator(&mut ctx, MARKET_ID).unwrap().epoch_start_ts, 0);

    // After funding started: the rollover's next boundary.
    let mut ctx = ctx_at(1_000_000);
    set_funding_interval(&mut ctx, 15);
    accepted(&mut ctx, 1_000_000);
    assert_eq!(storage::load_funding_state(&mut ctx, MARKET_ID).unwrap().next_funding_ts, 1_000_005);
    set_funding_interval(&mut ctx, HUGE_FUNDING_INTERVAL);
    ctx.block.timestamp = U256::from(1_000_020);
    let mark_before = mark(&mut ctx);
    let acc_before = storage::load_premium_accumulator(&mut ctx, MARKET_ID).unwrap();
    refused_as(&mut ctx, ADMIN, 1_000_020, OVERFLOW);
    assert_eq!(mark(&mut ctx), mark_before);
    assert_eq!(storage::load_premium_accumulator(&mut ctx, MARKET_ID).unwrap(), acc_before);

    // The config repaired, the same push goes through.
    set_funding_interval(&mut ctx, 15);
    accepted(&mut ctx, 1_000_020);
    assert_eq!(storage::load_funding_state(&mut ctx, MARKET_ID).unwrap().next_funding_ts, 1_000_035);
}

/// `updateMarket` on [`MARKET_ID`] through the call shell, with every field as `setup_market`
/// left it except the two intervals. The raw output, so a test can assert either way.
fn update_intervals(
    ctx: &mut TestCtx,
    price_update_interval: u64,
    funding_interval: u64,
) -> crate::PerpOutput {
    let input = crate::interface::IPerpDex::updateMarketCall {
        marketId: MARKET_ID,
        tickSize: 1,
        stepSize: 1,
        minQuantity: 1,
        maxQuantity: 1_000_000,
        maxPrice: 1_000_000,
        priceUpdateInterval: price_update_interval,
        active: true,
        fundingInterval: funding_interval,
        interestRate: 0,
        liquidationFeeRateBps: 0,
        priceBandBps: 0,
    }
    .abi_encode();
    run_perp_dex_call(&input, GAS, ADMIN, U256::ZERO, false, ctx).unwrap()
}

/// A year is the longest interval a market takes, on both fields (`addMarket` shares the check),
/// so `updateMarket` refuses the interval the test above needs. It used to accept it. At the cap
/// the funding clock still runs: the first boundary is the window plus a year.
#[test]
fn a_market_interval_beyond_a_year_is_refused() {
    let mut ctx = ctx_at(1_000_000);
    assert_eq!(MAX_MARKET_INTERVAL_SECS % 15, 0, "the cap is itself a valid 15 s funding interval");

    for (pui, fi, field) in [
        (15, MAX_MARKET_INTERVAL_SECS + 15, "fundingInterval"),
        (15, HUGE_FUNDING_INTERVAL, "fundingInterval"),
        (MAX_MARKET_INTERVAL_SECS + 1, 0, "priceUpdateInterval"),
        (u64::MAX, 0, "priceUpdateInterval"),
    ] {
        let writes = ctx.perp_write_count();
        let out = update_intervals(&mut ctx, pui, fi);
        assert!(out.reverted, "({pui}, {fi}) accepted");
        assert_eq!(
            alloy_sol_types::Revert::abi_decode(&out.bytes).map(|r| r.reason).unwrap(),
            format!("updateMarket: {field} must be <= {MAX_MARKET_INTERVAL_SECS}")
        );
        assert_eq!(ctx.perp_write_count(), writes, "({pui}, {fi}): a refused update wrote");
    }
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    assert_eq!((market.price_update_interval, market.funding_interval), (15, 0));

    assert!(!update_intervals(&mut ctx, MAX_MARKET_INTERVAL_SECS, 0).reverted);
    assert!(!update_intervals(&mut ctx, 15, MAX_MARKET_INTERVAL_SECS).reverted);
    accepted(&mut ctx, 1_000_000);
    assert_eq!(
        storage::load_funding_state(&mut ctx, MARKET_ID).unwrap().next_funding_ts,
        999_990 + MAX_MARKET_INTERVAL_SECS
    );
}
