use super::*;

/// 55 identical 5x longs, each its own address so each is its own registry entry. Seeded in a
/// deterministic order so the test can also speak about WHICH ones get done.
/// `losers` underwater longs of `QTY`, against `QTY * losers` counterparties holding ONE unit
/// each.
///
/// The shape matters: one fill per counterparty means a single loser costs `QTY` fills, so the
/// shared 128-fill ADL budget runs out after ~12 liquidations — long before the 50-slot cap.
/// A shared whale would absorb each loser in ONE fill and the slot cap would bind first, which
/// is a different test.
pub(super) fn seed_underwater_longs_fragmented(ctx: &mut TestCtx, losers: u8) -> Vec<Address> {
    let users: Vec<Address> = (1..=losers)
        .map(|i| {
            let user = Address::from([i; 20]);
            seed_position_account(ctx, user, QTY, -ENTRY_VALUE, MARGIN, 5, 0);
            user
        })
        .collect();
    for k in 0..(losers as u16) * (QTY as u16) {
        let mut b = [0xC0u8; 20];
        b[18] = (k >> 8) as u8;
        b[19] = k as u8;
        // One unit, same $100 entry and same 5x as the longs, so it is deeply in profit at any
        // mark below $100 and stays an eligible candidate throughout.
        seed_position_account(ctx, Address::from(b), -1, ENTRY_VALUE / QTY, MARGIN / QTY, 5, 0);
    }
    users
}

pub(super) fn seed_underwater_longs(ctx: &mut TestCtx, n: u8) -> Vec<Address> {
    let users: Vec<Address> = (1..=n)
        .map(|i| {
            let user = Address::from([i; 20]);
            seed_position_account(ctx, user, QTY, -ENTRY_VALUE, MARGIN, 5, 0);
            user
        })
        .collect();
    // LAST, so the losers keep registry indices 0..n and the order-sensitive test below still
    // reads straightforwardly. Sized to absorb every one of them: without a counterparty each
    // liquidation would defer instead of completing, and these tests would be measuring
    // deferral rather than the cap.
    seed_absorbing_short(ctx, n as i64);
    users
}

pub(super) fn still_open(ctx: &mut TestCtx, users: &[Address]) -> usize {
    users
        .iter()
        .filter(|u| storage::load_position(ctx, **u, MARKET_ID).unwrap().amount != 0)
        .count()
}

/// ONE update liquidates exactly 50 and leaves the rest untouched; a SECOND update finishes
/// them. The deferral is the documented behaviour and it had no coverage.
#[test]
fn the_cap_defers_the_overflow_to_the_next_update() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let users = seed_underwater_longs(&mut ctx, 55);
    assert_eq!(still_open(&mut ctx, &users), 55);

    // $100 -> $85: every one of them is below maintenance but still solvent.
    oracle_update_at(&mut ctx, 8_500, 31);
    assert_eq!(
        still_open(&mut ctx, &users),
        5,
        "exactly MAX_SWEEP_ACTIONS_PER_UPDATE (50) liquidated in one update"
    );

    // A second update at the SAME price — the mark does not have to move again. This is the
    // part worth pinning: nothing was stored as pending, the registry is simply re-scanned.
    oracle_update_at(&mut ctx, 8_500, 61);
    assert_eq!(still_open(&mut ctx, &users), 0, "the overflow is drained, not lost");
}

/// ⚠️ WHICH 50 is POSITIONAL, not severity-ranked: the sweep snapshots the registry and walks
/// it in INSERTION order from index 0 every update, with no cursor and no rotation. Pinned
/// because it is a fairness property nothing else states, and because any future rotation /
/// severity ordering must consciously break this test rather than silently change behaviour.
#[test]
fn the_first_fifty_by_registry_order_are_the_ones_liquidated() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let users = seed_underwater_longs(&mut ctx, 55);
    let mut expected = users.clone();
    expected.push(WHALE); // seeded last by `seed_underwater_longs`
    assert_eq!(
        storage::load_position_registry(&mut ctx, MARKET_ID).unwrap(),
        expected,
        "registry is insertion-ordered — the premise of this test"
    );

    oracle_update_at(&mut ctx, 8_500, 31);

    for (i, u) in users.iter().enumerate() {
        let open = storage::load_position(&mut ctx, *u, MARKET_ID).unwrap().amount != 0;
        assert_eq!(
            open,
            i >= 50,
            "registry index {i} should {} be open",
            if i >= 50 { "" } else { "NOT" }
        );
    }
}

/// Under the cap, nothing is deferred — guards against a future change that defers work it did
/// not need to, which the test above could not distinguish from correct behaviour.
#[test]
fn under_the_cap_one_update_clears_everything() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let users = seed_underwater_longs(&mut ctx, 49);
    oracle_update_at(&mut ctx, 8_500, 31);
    assert_eq!(still_open(&mut ctx, &users), 0);
}

pub(super) fn oracle_update_at(ctx: &mut TestCtx, index: u64, ts: u64) {
    run_update_index_price(
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: ts }
            .abi_encode(),
        ADMIN,
        ctx,
    )
    .unwrap();
}
