use super::*;
use crate::interface::IPerpDex::{Adl, Liquidation, Trade};

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

// ── Q6: a maker-capped liquidation rest goes to the drain, not to ADL ──────────────────────────
//
// The distinct-maker cap (`MAX_LIQUIDATION_MAKER_ACCOUNTS` = 128) bounds one liquidation PASS.
// When it stops the book leg the next maker in band is still live, so the rest is not ADL'd: the
// pass ends, `deferred_work` is set, and the next pass continues against the book. ADL still takes
// what the book leg leaves for any other reason (here: the book running out).

/// ALICE isolated long `qty` @ $100 at 5x (`vQuote = −qty·1e8`, `margin = qty·2e7`, wallet 0),
/// WHALE the opposite short at 1x (wallet 0, healthy at any mark below $100 — the only ADL
/// counterparty), and `makers` distinct accounts each bidding 1 @ $90. Mark $100, band 0, funding
/// off (`setup_market`).
fn maker_capped_fixture(ctx: &mut TestCtx, qty: u64, makers: usize) {
    setup_market(ctx);
    let entry_value = calc_value(ENTRY_PRICE, qty, 0, PRICE_DECIMALS).unwrap();
    assert_eq!(entry_value, qty * 100_000_000);
    let (q, v) = (qty as i64, entry_value as i64);
    seed_position_account(ctx, ALICE, q, -v, v / 5, 5, 0);
    seed_position_account(ctx, WHALE, -q, v, v, 1, 0);
    for index in 0..makers {
        let maker = indexed_maker(index);
        storage::save_account(
            ctx,
            maker,
            UserAccount { perp_wallet_balance: 200_000_000, ..UserAccount::default() },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
        place_order(ctx, maker, Side::Buy as u8, LONG_LIQ_PRICE, 1);
    }
    let _ = JournalTr::take_logs(ctx.journal_mut());
}

/// The oracle tick to $90 (the mark is exactly the index) that runs the liquidation sweep.
fn tick_to_ninety(ctx: &mut TestCtx) -> Vec<primitives::Log> {
    advance_block_to(ctx, 31);
    oracle_update_at(ctx, LONG_LIQ_PRICE, 31);
    assert_eq!(storage::load_mark_price(ctx, MARKET_ID).unwrap(), LONG_LIQ_PRICE);
    JournalTr::take_logs(ctx.journal_mut())
}

/// `drainDeferredWork(MARKET_ID)` → `(moreRemaining, logs)`.
fn drain(ctx: &mut TestCtx) -> (bool, Vec<primitives::Log>) {
    let out = crate::risk::funding::run_drain_deferred_work(
        &drainDeferredWorkCall { marketId: MARKET_ID }.abi_encode(),
        ctx,
    )
    .unwrap();
    let more = drainDeferredWorkCall::abi_decode_returns(&out).unwrap();
    (more, JournalTr::take_logs(ctx.journal_mut()))
}

fn deferred_work(ctx: &mut TestCtx) -> u8 {
    storage::load_market(ctx, MARKET_ID).unwrap().unwrap().deferred_work
}

fn bids_at_ninety(ctx: &mut TestCtx) -> u64 {
    storage::load_bid_count(ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap()
}

/// `(liquidatedUser, adlUser, qty, price)` of every ADL fill.
fn adl_fills(logs: &[primitives::Log]) -> Vec<(Address, Address, u64, u64)> {
    decode_events::<Adl>(logs)
        .into_iter()
        .map(|a| (a.liquidatedUser, a.adlUser, a.qty, a.price))
        .collect()
}

/// `(amount, reward)` of every `Liquidation` row.
fn liquidation_rows(logs: &[primitives::Log]) -> Vec<(i64, u64)> {
    decode_events::<Liquidation>(logs).into_iter().map(|l| (l.amount, l.reward)).collect()
}

fn wallet_of(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_account(ctx, user).unwrap().perp_wallet_balance
}

/// **The sweep stops at the cap, the drain finishes against the book, and the totals are the
/// one-shot liquidation's.** 133 single-lot bids for a long of 133, fee 125 bps.
///
/// At $90: equity `2_660_000_000 − 1_330_000_000 = 1_330_000_000` < MM `133·90e6/6 =
/// 1_995_000_000`. The tick's pass fills 128 makers and stops on the cap with 5 live bids left:
/// no ADL, the rest (5) stays open, `deferred_work` set. Close credit `2_560_000_000 −
/// 1_280_000_000`, fee `11_520_000_000 · 125 / 1e4 = 144_000_000`. The drain's pass closes the 5
/// on the book (fee `450_000_000 · 125 / 1e4 = 5_625_000`). Wallet `1_180_375_000` and IF
/// `149_625_000` are what the one-shot liquidation before Q6 ended with (128 book + 5 ADL).
#[test]
fn a_maker_capped_isolated_liquidation_finishes_against_the_book_on_the_drain() {
    let mut ctx = make_ctx();
    maker_capped_fixture(&mut ctx, 133, 133);
    let mut market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    market.liquidation_fee_rate_bps = 125;
    storage::save_market(&mut ctx, &market).unwrap();
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0);

    let logs = tick_to_ninety(&mut ctx);
    let alice = position(&mut ctx, ALICE);
    assert_eq!(
        (alice.amount, alice.v_quote_balance, alice.margin),
        (5, -500_000_000, 100_000_000),
        "128 closed on the book, the capped rest of 5 left open"
    );
    assert_eq!(wallet_of(&mut ctx, ALICE), 1_136_000_000);
    assert_eq!(decode_events::<Trade>(&logs).len(), MAX_LIQUIDATION_MAKER_ACCOUNTS);
    assert!(adl_fills(&logs).is_empty(), "the capped rest must not be ADL'd: {:?}", adl_fills(&logs));
    assert_eq!(bids_at_ninety(&mut ctx), 5, "the book still holds the rest's liquidity");
    assert_eq!(position(&mut ctx, WHALE).amount, -133, "the ADL counterparty is untouched");
    assert_eq!(deferred_work(&mut ctx), 1, "the drain must see the capped rest");
    assert_eq!(liquidation_rows(&logs), [(133, 144_000_000)]);
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 144_000_000);

    let (more, logs) = drain(&mut ctx);
    assert!(!more, "the drain finished the rest");
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    assert_eq!(wallet_of(&mut ctx, ALICE), 1_180_375_000);
    assert_eq!(decode_events::<Trade>(&logs).len(), 5);
    assert!(adl_fills(&logs).is_empty());
    assert_eq!(bids_at_ninety(&mut ctx), 0);
    assert_eq!(position(&mut ctx, WHALE).amount, -133);
    assert_eq!(deferred_work(&mut ctx), 0);
    assert_eq!(liquidation_rows(&logs), [(5, 5_625_000)], "one row per pass, fee on what it closed");
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 149_625_000);

    let (more, _) = drain(&mut ctx);
    assert!(!more, "nothing left: a no-op drain");
}

/// **The reviewer's repro through manual `liquidate()`.** The manual pass is not a sweep, but the
/// deferral it leaves is no longer discarded: it sets `deferred_work`, so the drain — not the next
/// tick — closes the capped rest against the book.
#[test]
fn liquidate_leaves_a_maker_capped_residual_to_the_drain() {
    let mut ctx = make_ctx();
    maker_capped_fixture(&mut ctx, 133, 133);
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());

    liquidate(&mut ctx, ALICE).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(position(&mut ctx, ALICE).amount, 5);
    assert!(adl_fills(&logs).is_empty(), "the capped rest must not be ADL'd: {:?}", adl_fills(&logs));
    assert_eq!(bids_at_ninety(&mut ctx), 5);
    assert_eq!(position(&mut ctx, WHALE).amount, -133);
    assert_eq!(deferred_work(&mut ctx), 1, "manual liquidate() records its deferral");

    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    assert!(adl_fills(&logs).is_empty());
    assert_eq!(bids_at_ninety(&mut ctx), 0);
    assert_eq!(position(&mut ctx, WHALE).amount, -133);
    assert_eq!(deferred_work(&mut ctx), 0);
}

/// **Once the book runs out, the rest still goes to ADL.** 130 bids for a long of 135: the tick
/// stops on the cap with 2 bids left and 7 open (equity `70e6` < MM `105e6`, still liquidatable);
/// the drain fills the 2 bids, the book is empty, and the last 5 are ADL'd at the mark (equity
/// `50e6` ≥ 0, solvent) against WHALE.
#[test]
fn a_maker_capped_residual_still_adls_once_the_book_runs_out() {
    let mut ctx = make_ctx();
    maker_capped_fixture(&mut ctx, 135, 130);

    let logs = tick_to_ninety(&mut ctx);
    let alice = position(&mut ctx, ALICE);
    assert_eq!(
        (alice.amount, alice.v_quote_balance, alice.margin),
        (7, -700_000_000, 140_000_000)
    );
    assert!(adl_fills(&logs).is_empty());
    assert_eq!(bids_at_ninety(&mut ctx), 2);
    assert_eq!(deferred_work(&mut ctx), 1);

    let (more, logs) = drain(&mut ctx);
    assert!(!more);
    assert_eq!(decode_events::<Trade>(&logs).len(), 2);
    assert_eq!(adl_fills(&logs), [(ALICE, WHALE, 5, LONG_LIQ_PRICE)], "ADL at the mark");
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    assert_eq!(position(&mut ctx, WHALE).amount, -130);
    assert_eq!(bids_at_ninety(&mut ctx), 0);
    assert_eq!(deferred_work(&mut ctx), 0);
}

/// **Exactly 128 makers is the book running out, not the cap**: the walk fills all 128 and finds
/// no 129th, so the rest is ADL'd in the same pass and nothing is deferred — as before Q6.
#[test]
fn exactly_128_makers_is_the_book_running_out_not_the_cap() {
    let mut ctx = make_ctx();
    maker_capped_fixture(&mut ctx, 133, MAX_LIQUIDATION_MAKER_ACCOUNTS);

    let logs = tick_to_ninety(&mut ctx);
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    assert_eq!(adl_fills(&logs), [(ALICE, WHALE, 5, LONG_LIQ_PRICE)]);
    assert_eq!(position(&mut ctx, WHALE).amount, -128);
    assert_eq!(bids_at_ninety(&mut ctx), 0);
    assert_eq!(deferred_work(&mut ctx), 0);
}

/// **A maker-capped pass that closed NOTHING goes to ADL** — orders refused at every fill cannot
/// stretch a liquidation over passes (Q6 review). 130 accounts each rest two 1-lot bids at $90 at a
/// stored 50x while tier 0 admits it; `setMarginTiers` then lowers tier 0 to 20x, so the per-open
/// tier guard refuses every one of those bids at its fill (cancelled `MakerInsolvent`). An honest
/// bid of 133 rests behind them, and ALICE's 133 long is liquidatable at $90.
///
/// The tick's book leg spends the 128-maker cap cancelling phantoms and fills nothing. It used to
/// defer the rest anyway, and each further pass did the same until the phantoms ran out — three
/// passes here, indefinitely for an attacker who restuffs them. Now the rest goes to ADL in the
/// same pass, against WHALE at the mark (ALICE's equity there is exactly 0: solvent), and nothing is
/// deferred. A capped pass that DID fill something still defers (the tests above).
#[test]
fn a_maker_capped_pass_that_closed_nothing_goes_to_adl() {
    use crate::interface::IPerpDex::OrderCancelled;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0], &[50]).unwrap();
    let qty = 133u64;
    let v = calc_value(ENTRY_PRICE, qty, 0, PRICE_DECIMALS).unwrap() as i64;
    seed_position_account(&mut ctx, ALICE, qty as i64, -v, v / 10, 10, 0);
    seed_position_account(&mut ctx, WHALE, -(qty as i64), v, v, 1, 0);
    let phantoms: Vec<Address> = (0..130).map(indexed_maker).collect();
    for &m in &phantoms {
        storage::save_account(
            &mut ctx,
            m,
            UserAccount { perp_wallet_balance: 200_000_000, ..UserAccount::default() },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
        run_set_leverage(&setLeverageCall { marketId: MARKET_ID, leverage: 50 }.abi_encode(), m, &mut ctx)
            .unwrap();
    }
    for _ in 0..2 {
        for &m in &phantoms {
            place_order(&mut ctx, m, Side::Buy as u8, LONG_LIQ_PRICE, 1);
        }
    }
    let honest = indexed_maker(10_000);
    storage::save_account(
        &mut ctx,
        honest,
        UserAccount { perp_wallet_balance: 20_000_000_000, ..UserAccount::default() },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    place_order(&mut ctx, honest, Side::Buy as u8, LONG_LIQ_PRICE, qty);
    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0], &[20]).unwrap();
    assert_eq!(bids_at_ninety(&mut ctx), 261);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let logs = tick_to_ninety(&mut ctx);
    assert!(decode_events::<Trade>(&logs).is_empty(), "every maker the walk reached was refused");
    let cancels: Vec<(Address, u8)> =
        decode_events::<OrderCancelled>(&logs).iter().map(|c| (c.user, c.reason)).collect();
    assert_eq!(cancels.len(), MAX_LIQUIDATION_MAKER_ACCOUNTS);
    assert!(cancels.iter().all(|&(u, r)| u != honest && r == CancelReason::MakerInsolvent as u8));
    assert_eq!(adl_fills(&logs), [(ALICE, WHALE, qty, LONG_LIQ_PRICE)], "ADL'd in the same pass");
    assert_eq!(liquidation_rows(&logs), [(qty as i64, 0)]);
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    assert_eq!(position(&mut ctx, WHALE).amount, 0);
    assert_eq!(deferred_work(&mut ctx), 0, "nothing deferred: the pass closed nothing on the book");
    assert_eq!(bids_at_ninety(&mut ctx), 261 - MAX_LIQUIDATION_MAKER_ACCOUNTS as u64);
}
