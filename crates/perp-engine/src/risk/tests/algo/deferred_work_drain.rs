use super::liquidation_cap::*;
use super::*;

/// A stranger with no role. Goes through the real dispatcher so the gas entry and the
/// non-static classification are exercised too, not just the handler. Returns `moreRemaining`.
const STRANGER: Address = Address::new([0xDD; 20]);

fn drain(ctx: &mut TestCtx) -> bool {
    let out = run_perp_dex_call(
        &drainDeferredWorkCall { marketId: MARKET_ID }.abi_encode(),
        1_000_000,
        STRANGER,
        U256::ZERO,
        false,
        ctx,
    )
    .unwrap();
    assert!(!out.reverted, "drainDeferredWork reverted: {:?}", out.bytes);
    drainDeferredWorkCall::abi_decode_returns(&out.bytes).unwrap()
}

fn flag(ctx: &mut TestCtx) -> u8 {
    storage::load_market(ctx, MARKET_ID).unwrap().unwrap().deferred_work
}

/// The whole point, end to end: the cap defers, the bit records it, a stranger drains it in the
/// SAME block, and the bit clears. Without this the backlog waits for the next oracle tick.
#[test]
fn a_stranger_drains_the_overflow_in_the_same_block() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let users = seed_underwater_longs(&mut ctx, 55);

    oracle_update_at(&mut ctx, 8_500, 31);
    assert_eq!(still_open(&mut ctx, &users), 5, "the cap deferred 5");
    assert_eq!(flag(&mut ctx), 1, "and recorded that it did");

    // No role, no oracle, no waiting for `priceUpdateInterval`.
    assert!(!drain(&mut ctx), "the backlog is gone, so nothing more remains");
    assert_eq!(still_open(&mut ctx, &users), 0);
    assert_eq!(flag(&mut ctx), 0, "the bit clears on the way out");
}

/// `moreRemaining` is a usable loop condition: 105 underwater positions need the oracle tick
/// plus two drains, and the return value says exactly that without the caller reading state.
#[test]
fn more_remaining_drives_the_keeper_loop() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let users = seed_underwater_longs(&mut ctx, 105);

    oracle_update_at(&mut ctx, 8_500, 31);
    assert_eq!(still_open(&mut ctx, &users), 55);

    assert!(drain(&mut ctx), "50 more done, 5 still queued");
    assert_eq!(still_open(&mut ctx, &users), 5);
    assert_eq!(flag(&mut ctx), 1);

    assert!(!drain(&mut ctx));
    assert_eq!(still_open(&mut ctx, &users), 0);
    assert_eq!(flag(&mut ctx), 0);
}

/// A poll on a market whose bit is `0` is READ-ONLY and does no sweep — it returns on the bit
/// alone, before the registry walk and before the book walk. That is what makes "call it every
/// block" affordable, and it is also what bounds the permissionless surface: the drain finishes
/// work a cap deferred, it is NOT a second liquidation sweep anyone can trigger off-tick.
///
/// The positions here are genuinely below maintenance at the stored mark with the bit still
/// `0` — the documented false-negative. A drain that skipped the early return would liquidate
/// them and this would fail on both counts.
#[test]
fn a_poll_with_nothing_deferred_is_read_only_and_sweeps_nothing() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let users = seed_underwater_longs(&mut ctx, 3);
    // Move the mark WITHOUT an oracle tick, so nothing clears them and the bit stays 0.
    storage::save_mark_price(&mut ctx, MARKET_ID, 8_500).unwrap();
    assert_eq!(flag(&mut ctx), 0, "no sweep has run, so nothing has been deferred");

    let before = ctx.perp_write_count();
    assert!(!drain(&mut ctx));
    assert_eq!(ctx.perp_write_count(), before, "a no-op poll is read-only");
    assert_eq!(still_open(&mut ctx, &users), 3, "and it swept nothing");

    // Again, to catch a first-call-only memoisation.
    assert!(!drain(&mut ctx));
    assert_eq!(ctx.perp_write_count(), before);

    // The oracle tick is what clears them — the drain never substitutes for it.
    oracle_update_at(&mut ctx, 8_500, 31);
    assert_eq!(still_open(&mut ctx, &users), 0);
}

/// The drain does the sweeps and NOTHING else. Re-running the oracle tail between ticks would
/// fold a second premium sample into the epoch and move `nextFundingTime` — i.e. a
/// permissionless call would be able to skew the funding rate. Pinned against exactly that.
#[test]
fn the_drain_does_not_touch_the_price_or_the_funding_epoch() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let users = seed_underwater_longs(&mut ctx, 55);
    oracle_update_at(&mut ctx, 8_500, 31);

    let mark = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap().mark_price;
    let index = storage::load_index_price_state(&mut ctx, MARKET_ID).unwrap().index_price;
    let funding = storage::load_funding_state(&mut ctx, MARKET_ID).unwrap();
    let acc = storage::load_premium_accumulator(&mut ctx, MARKET_ID).unwrap();

    drain(&mut ctx);
    assert_eq!(still_open(&mut ctx, &users), 0, "it did do the work");

    assert_eq!(
        storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap().mark_price,
        mark,
        "no mark recompute"
    );
    assert_eq!(
        storage::load_index_price_state(&mut ctx, MARKET_ID).unwrap().index_price,
        index,
        "no index write"
    );
    assert_eq!(
        storage::load_funding_state(&mut ctx, MARKET_ID).unwrap(),
        funding,
        "no funding settlement, no epoch roll"
    );
    assert_eq!(
        storage::load_premium_accumulator(&mut ctx, MARKET_ID).unwrap(),
        acc,
        "no extra premium sample — a permissionless rate skew"
    );
}

/// **The ADL fill budget reaches the drain.** The fixture is built so the SLOT cap provably
/// cannot be the thing that fires: 20 losers is well under 50, but each needs `QTY` separate
/// counterparties, so the shared 128-fill budget runs out around the 13th liquidation.
///
/// Before this signal existed the sweep returned `Ok(false)` whenever it completed its walk,
/// so an ADL-starved backlog left the bit at `0` and `drainDeferredWork` returned on the fast
/// path without sweeping anything — the whole backlog waited for the next oracle tick. This
/// pins that it no longer does.
#[test]
fn the_adl_fill_budget_also_sets_the_bit() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // 20 losers x QTY fills each = 200 fills needed against only 20 slots: the 128-fill ADL
    // budget is provably what stops this, not the 50-slot cap.
    let users = seed_underwater_longs_fragmented(&mut ctx, 20);

    oracle_update_at(&mut ctx, 8_500, 31);
    let open_after_tick = still_open(&mut ctx, &users);
    assert!(open_after_tick > 0, "the fixture must leave a backlog");
    assert!(
        20 - open_after_tick < MAX_SWEEP_ACTIONS_PER_UPDATE as usize,
        "fixture: the slot cap must NOT be what stopped this, or the test proves nothing"
    );
    assert_eq!(flag(&mut ctx), 1, "a budget, not the market, stopped it");

    // The drain gets fresh budgets and makes real progress — the thing the bit buys.
    drain(&mut ctx);
    assert!(
        still_open(&mut ctx, &users) < open_after_tick,
        "the drain must actually reduce the backlog"
    );
}

#[test]
fn an_unknown_market_reverts() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let out = run_perp_dex_call(
        &drainDeferredWorkCall { marketId: MARKET_ID + 7 }.abi_encode(),
        1_000_000,
        STRANGER,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(out.reverted, "an unknown market must revert, not silently return false");
}
