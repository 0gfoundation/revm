use super::*;
use crate::math::calc_trading_fee;
use crate::interface::IPerpDex::{
    batchPlaceOrdersCall, setLeverageCall, OrderCancelled, PlaceItem, Trade,
};
use crate::types::CancelReason;

/// Every `AccountBalanceChanged` in the journal, in emission order.
fn take_balance_events(ctx: &mut TestCtx) -> Vec<AccountBalanceChanged> {
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
        .map(|log| {
            AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
        })
        .collect()
}

// `assert_account_update_groups` and `stream_shape` live in `crate::events::stream_test_support`
// — the module that owns the invariant — because `risk::tests` needs them too (the liquidation
// and ADL streams are asserted there, against the fixtures that already exist there).
use crate::events::stream_test_support::{
    account_update_reasons, assert_account_update_groups, stream_shape,
};

// ── ORDERED-STREAM SHAPE: the assembled `ACCOUNT_UPDATE` pushes, not just counts ──────────
//
// The tests above assert WHO gets a snapshot and how many. These assert the exact ORDERED
// `(event, subject)` sequence, which is the only form that can catch the failure this design
// exists to prevent: a `PositionChanged` landing one slot too early (before its own header) or
// one slot too late (after a foreign header) is invisible to a count and fatal to an indexer.

/// **The two-fill sweep.** One taker crossing two DIFFERENT makers: each fill is its own
/// `ACCOUNT_UPDATE` for that maker (`Trade` outside the group, header then row inside), and the
/// taker gets one group for the whole order, last.
///
/// Two distinct makers rather than one maker twice, so a header/row mismatch — the row from
/// fill 2 sitting inside fill 1's group — is a DIFFERENT ADDRESS and not merely a duplicate.
#[test]
fn a_two_fill_sweep_publishes_one_group_per_fill_then_the_takers() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);
    // One ask each, same level → one sweep consumes both from one queue, BOB first (FIFO).
    place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell BOB");
    place_in(&mut ctx, CAROL, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell CAROL");

    let logs = crossing_order(&mut ctx, ALICE, 0, PRICE, 2 * QTY);
    assert_account_update_groups(&logs);
    assert_eq!(
        stream_shape(&logs),
        vec![
            ("OrderPlaced", Some(ALICE)),
            // fill 1: BOB's group, with the `Trade` OUTSIDE it
            ("Trade", Some(ALICE)),
            ("AccountBalanceChanged", Some(BOB)),
            ("PositionChanged", Some(BOB)),
            // fill 2: CAROL's group
            ("Trade", Some(ALICE)),
            ("AccountBalanceChanged", Some(CAROL)),
            ("PositionChanged", Some(CAROL)),
            // the taker: ONE group for the whole order
            ("AccountBalanceChanged", Some(ALICE)),
            ("PositionChanged", Some(ALICE)),
        ],
        "each fill is one ACCOUNT_UPDATE for its maker; the taker gets one for the order"
    );
    // …and every one of them reports `ORDER`, Binance's reason for a matched order moving money.
    // Measured R14: a market fill pushes exactly one `ACCOUNT_UPDATE(m=ORDER)`.
    assert_eq!(
        account_update_reasons(&logs),
        vec![
            (BOB, AccountUpdateReason::Order),
            (CAROL, AccountUpdateReason::Order),
            (ALICE, AccountUpdateReason::Order),
        ],
    );
}

/// **The funding-epoch rollover, non-registry path.** A rolled-over funding index plus an
/// `addPositionMargin` is TWO economic events for one user in one call, so it is two pushes:
/// the funding group, then the margin group. `FundingSettled` sits before the first header and
/// `PositionMarginAdjusted` between the two, and both are group terminators — which is exactly
/// why the funding header cannot be hoisted to the top of `apply_funding_settlement`.
#[test]
fn a_funding_rollover_publishes_its_own_group_before_the_margin_add_group() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    set_mark(&mut ctx, PRICE);

    // ALICE: QTY long at $100.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    // The epoch rolled over: a funding index a LONG owes against.
    storage::save_funding_state(
        &mut ctx,
        MARKET_ID,
        &FundingState {
            last_funding_rate: 1_000,
            next_funding_ts: u64::MAX,
            cumulative_funding_index: PRICE as i128 * 1_000,
        },
    )
    .unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());

    start_call(&mut ctx);
    crate::risk::run_add_position_margin(
        &crate::interface::IPerpDex::addPositionMarginCall {
            marketId: MARKET_ID,
            amount: 1,
        }
        .abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap();
    end_call(&mut ctx);

    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_account_update_groups(&logs);
    assert_eq!(
        stream_shape(&logs),
        vec![
            ("FundingSettled", Some(ALICE)),
            ("AccountBalanceChanged", Some(ALICE)),
            ("PositionChanged", Some(ALICE)),
            ("PositionMarginAdjusted", Some(ALICE)),
            ("AccountBalanceChanged", Some(ALICE)),
            ("PositionChanged", Some(ALICE)),
        ],
        "funding and the margin add are two economic events → two ACCOUNT_UPDATE pushes; the \
         drain adds nothing (the inline emit cleared the mark)"
    );
    // Two economic events, two DIFFERENT reasons — and this is the pair that shows the field
    // earning its keep: both rows are for the same user, both carry the same three balances'
    // shape, and only `reason` distinguishes "funding took money out of my silo" from "I moved
    // money into it myself". `MarginTransfer` is Binance's value for their *Modify Isolated
    // Position Margin*, which is what `addPositionMargin` is.
    assert_eq!(
        account_update_reasons(&logs),
        vec![
            (ALICE, AccountUpdateReason::FundingFee),
            (ALICE, AccountUpdateReason::MarginTransfer),
        ],
    );
}

/// **The funding-epoch rollover, REGISTRY path — the subtle one.** A maker's funding is settled
/// at `MatchRegistry::get_or_load` and replayed by the flush BEFORE the flush writes the
/// position, so a header folded from STORAGE at that point would report the pre-match silo. The
/// payload is therefore captured from the working copy; this pins that it lands as its own
/// group, ahead of the fill's `Trade` and the fill's own group.
#[test]
fn a_funding_rollover_on_the_match_path_groups_the_makers_funding_before_its_fill() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    set_mark(&mut ctx, PRICE);
    fund(&mut ctx, CAROL, WALLET);

    // BOB ends up SHORT QTY (he sold to ALICE), so funding can bite his silo.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert_eq!(pos(&mut ctx, BOB).amount, -(QTY as i64));
    // …and rests a bid, which CAROL will cross: BOB is the MAKER of the next fill.
    place_in(&mut ctx, BOB, MARKET_ID, 0, PRICE, QTY, 3).expect("maker buy BOB");

    // The epoch rolled over with a NEGATIVE index, so the SHORT is the side that pays.
    storage::save_funding_state(
        &mut ctx,
        MARKET_ID,
        &FundingState {
            last_funding_rate: -1_000,
            next_funding_ts: u64::MAX,
            cumulative_funding_index: -(PRICE as i128) * 1_000,
        },
    )
    .unwrap();

    let logs = crossing_order(&mut ctx, CAROL, 1, PRICE, QTY);
    assert_account_update_groups(&logs);
    assert_eq!(
        stream_shape(&logs),
        vec![
            ("OrderPlaced", Some(CAROL)),
            // BOB's funding group, replayed from the registry at his first touch
            ("FundingSettled", Some(BOB)),
            ("AccountBalanceChanged", Some(BOB)),
            ("PositionChanged", Some(BOB)),
            // then the fill: `Trade` outside, BOB's fill group, then CAROL's order group
            ("Trade", Some(CAROL)),
            ("AccountBalanceChanged", Some(BOB)),
            ("PositionChanged", Some(BOB)),
            ("AccountBalanceChanged", Some(CAROL)),
            ("PositionChanged", Some(CAROL)),
        ],
        "the maker's funding is its own push, ahead of the fill it was settled for"
    );
    // The maker's funding push is `FUNDING_FEE`; his fill and the taker's order are `ORDER`.
    assert_eq!(
        account_update_reasons(&logs),
        vec![
            (BOB, AccountUpdateReason::FundingFee),
            (BOB, AccountUpdateReason::Order),
            (CAROL, AccountUpdateReason::Order),
        ],
    );
}

/// A second market, so a batch can touch one user across two of them.
const MARKET_2: u64 = 2;

fn add_market_2(ctx: &mut TestCtx) {
    let mut m = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    m.market_id = MARKET_2;
    storage::save_market(ctx, &m).unwrap();
    storage::save_mark_price(ctx, MARKET_2, MARK).unwrap();
}

fn place_in(
    ctx: &mut TestCtx,
    caller: Address,
    market: u64,
    side: u8,
    price: u64,
    qty: u64,
    tif: u8,
) -> Result<Bytes, PerpError> {
    let input = placeOrderCall {
        marketId: market,
        side,
        price,
        quantity: qty,
        orderType: 0,
        tif,
        clientOrderId: FixedBytes::default(),
                flags: 0,
    }
    .abi_encode();
    run_place_order(&input, caller, ctx)
}

/// A LONG whose mark has moved off its entry, so `Σ uPnL`, `Σ isolatedWallet`, `Σ PIM` and
/// `Σ maintMargin` are all non-zero — otherwise a field-for-field equality against `getAccount`
/// would be an equality between rows of zeros.
fn seed_long(ctx: &mut TestCtx, who: Address, market: u64, lots: i64) {
    let notional = lots * FILL_VALUE as i64;
    storage::save_position(
        ctx,
        who,
        market,
        &PerpPosition {
            amount: lots * QTY as i64,
            v_quote_balance: -notional,
            margin: notional.abs(),
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

/// Mark $110 against a $100 entry: `N` and therefore PIM / MM / uPnL are all live.
const MARK: u64 = PRICE + 10 * TICK;

/// ALICE with a live long in the test market; journal and marks both drained.
fn fixture(ctx: &mut TestCtx) {
    setup(ctx);
    storage::save_mark_price(ctx, MARKET_ID, MARK).unwrap();
    seed_long(ctx, ALICE, MARKET_ID, 3);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    start_call(ctx);
}

// ── The trigger filter: placement and cancel are SILENT ──────────────────────────────────

/// **The headline.** A placement that only RESTS publishes no account snapshot, even though it
/// moves `availableBalance`.
///
/// The `start_call` / `end_call` pair matters: without the drain actually running, "no event"
/// would hold for the trivial reason that `place_in` bypasses the shell. Here the drain runs and
/// finds an EMPTY set, which is the property under test — `rest_in_book` writes the position
/// through `save_position_reservation_only`, and that entry point marks nobody.
#[test]
fn a_resting_placement_publishes_no_account_snapshot() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);

    let available_before = get_account(&mut ctx, ALICE).availableBalance;
    let _ = JournalTr::take_logs(ctx.journal_mut());

    start_call(&mut ctx);
    place_in(&mut ctx, ALICE, MARKET_ID, 0, PRICE, QTY, 0).expect("resting buy");
    end_call(&mut ctx);

    assert!(
        take_balance_events(&mut ctx).is_empty(),
        "a pure placement must publish nothing — R14 + the official trigger sentence"
    );
    // And it really did move the number the event would have carried, so the silence is a
    // DECISION and not an absence of change. This is the stale-between-fills cost, pinned.
    let after = get_account(&mut ctx, ALICE);
    assert!(
        after.totalOpenOrderInitialMargin > 0,
        "the resting order must contribute a real ooIM term"
    );
    assert_eq!(
        after.availableBalance,
        available_before - after.totalOpenOrderInitialMargin as i64,
        "availableBalance fell by exactly the new order's requirement — unannounced"
    );
}

/// PostOnly never calls `match_order`, so `rest_in_book` is the only write path: still silent.
#[test]
fn a_post_only_placement_publishes_no_account_snapshot() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);

    start_call(&mut ctx);
    place_in(&mut ctx, ALICE, MARKET_ID, 0, PRICE, QTY, 3).expect("post-only buy");
    end_call(&mut ctx);

    assert!(take_balance_events(&mut ctx).is_empty());
}

/// A pure CANCEL publishes nothing either. `release_open_order_margin` shrinks `Bid`/`Ask` and
/// nothing else, so it too takes the reservation-only write path — "nothing to release", the
/// same Summary-Matrix row and the same official sentence as the placement.
#[test]
fn a_pure_cancel_publishes_no_account_snapshot() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let oo_im_before = get_account(&mut ctx, ALICE).totalOpenOrderInitialMargin;
    assert!(oo_im_before > 0, "the fixture must leave a real ooIM term");
    let _ = JournalTr::take_logs(ctx.journal_mut());

    start_call(&mut ctx);
    let out = run_perp_dex_call(
        &cancelOrderCall {
            orderId: id.into(),
            marketId: MARKET_ID,
        }
        .abi_encode(),
        10_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!out.reverted);

    assert!(
        take_balance_events(&mut ctx).is_empty(),
        "a pure cancel must publish nothing"
    );
    assert_eq!(
        get_account(&mut ctx, ALICE).totalOpenOrderInitialMargin,
        0,
        "…while the requirement it released really is gone"
    );
}

/// **THE PAYLOAD/TRIGGER AGREEMENT.** A placement and a cancel cannot move ANY field the event
/// publishes — so the two tests above ("they publish nothing") are not merely Binance mimicry,
/// they are *consistent*: there is nothing to publish.
///
/// This is the property the payload was narrowed for. The old eleven-field payload did NOT have
/// it: `totalOpenOrderInitialMargin` and `availableBalance` move on every placement and every
/// cancel, so the event shipped fields whose freshness its own trigger did not guarantee. The
/// assertions below pin both halves — the three published balances are byte-identical across a
/// placement and a cancel, and the numbers that DID move are `getAccount`-only.
///
/// # Why this survives the per-event granularity intact
///
/// It never depended on "the snapshot is emitted last" as a mechanism, only on the DRAIN existing
/// for a pure account write — and it still does (rule 3: non-trading paths coalesce). The
/// measurement is a probe in its own separate call, `mutate_account_balance(credit_perp(0))`, so
/// what it reads is the state at the end of that probe call, never a row emitted by the placement
/// or the cancel under test. The two calls under test remain PURE placement and PURE cancel — the
/// order rests in `MARKET_2`, which has no liquidity, so neither becomes a taker and neither can
/// reach the new per-fill or per-order emit points. That is what keeps "they publish nothing"
/// checkable: it is still a statement about a stream with no money rows in it.
///
/// The order is placed in a market ALICE holds NO position in, which is the subtle leg: an
/// order-list transition flips per-user-market-index membership
/// (`storage::sync_user_market_membership`) with no snapshot mark, so the placement ADDS a market
/// to the set `Σ pos.margin` is summed over and the cancel DROPS it. That is only harmless
/// because a flat position carries no margin. If a flat position could hold margin,
/// `totalWalletBalance` would move here and this test is what would catch it.
#[test]
fn placement_and_cancel_cannot_move_any_published_field() {
    // Force one snapshot for `user` and return its whole payload. `credit_perp(0)` is a write
    // that MOVES NOTHING, so the payload is the current state verbatim — the same isolation
    // trick `margin_view_tests::the_event_and_get_account_agree_field_for_field_on_the_same_state`
    // uses.
    fn published(ctx: &mut TestCtx, user: Address) -> (U256, i64, i64) {
        let _ = JournalTr::take_logs(ctx.journal_mut());
        start_call(ctx);
        storage::mutate_account_balance(ctx, user, AccountUpdateReason::Adjustment, |a| {
            a.credit_perp(0)
        })
            .unwrap()
            .unwrap();
        end_call(ctx);
        let events = take_balance_events(ctx);
        assert_eq!(
            events.len(),
            1,
            "the probe must publish exactly one snapshot"
        );
        let e = &events[0];
        assert_eq!(e.user, user);
        (
            e.usdcBalance,
            e.totalWalletBalance,
            e.totalCrossWalletBalance,
        )
    }

    let mut ctx = make_ctx();
    fixture(&mut ctx);
    add_market_2(&mut ctx);
    // The fixture's long is in MARKET_ID; MARKET_2 is untouched, so the order below moves ALICE's
    // index membership. Both non-trivial fields are live: she has a wallet AND a funded silo.
    let before = published(&mut ctx, ALICE);
    assert!(
        before.1 != before.2 && before.2 != 0,
        "fixture must make both published balances non-trivial, got {before:?}"
    );
    assert!(
        !storage::load_user_markets(&mut ctx, ALICE)
            .unwrap()
            .contains(&MARKET_2),
        "MARKET_2 must start OUTSIDE the index for the membership leg to be exercised"
    );
    let available_before = get_account(&mut ctx, ALICE).availableBalance;

    // ── A placement ───────────────────────────────────────────────────────────────────────
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let out = run_perp_dex_call(
        &placeOrderCall {
            marketId: MARKET_2,
            side: 0,
            price: PRICE,
            quantity: QTY,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
                        flags: 0,
        }
        .abi_encode(),
        10_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!out.reverted, "placement reverted: {:?}", out.bytes);
    assert!(
        take_balance_events(&mut ctx).is_empty(),
        "placement is silent"
    );
    let order_id: [u8; 32] =
        crate::interface::IPerpDex::placeOrderCall::abi_decode_returns(&out.bytes)
            .unwrap()
            .0;
    assert!(
        storage::load_user_markets(&mut ctx, ALICE)
            .unwrap()
            .contains(&MARKET_2),
        "the resting order really did add MARKET_2 to the index — the membership leg is live"
    );
    assert_eq!(
        published(&mut ctx, ALICE),
        before,
        "a placement cannot move any published field — not the two balances, and not through \
         the index-membership change it causes"
    );
    // …while the numbers the OLD payload carried really did move. Without this the equality
    // above would hold for the trivial reason that nothing happened.
    let after_place = get_account(&mut ctx, ALICE);
    assert!(
        after_place.totalOpenOrderInitialMargin > 0
            && after_place.availableBalance < available_before,
        "the resting order must move `Σ ooIM` and `availableBalance` — the two fields the \
         narrowing removed, which is why they are REST-only: {:?}",
        (
            after_place.totalOpenOrderInitialMargin,
            after_place.availableBalance,
            available_before
        )
    );

    // ── …and a cancel ─────────────────────────────────────────────────────────────────────
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let out = run_perp_dex_call(
        &cancelOrderCall {
            orderId: order_id.into(),
            marketId: MARKET_2,
        }
        .abi_encode(),
        10_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!out.reverted, "cancel reverted: {:?}", out.bytes);
    assert!(take_balance_events(&mut ctx).is_empty(), "cancel is silent");
    assert!(
        !storage::load_user_markets(&mut ctx, ALICE)
            .unwrap()
            .contains(&MARKET_2),
        "the cancel dropped MARKET_2 back out of the index — the other direction of the same leg"
    );
    assert_eq!(
        published(&mut ctx, ALICE),
        before,
        "a cancel cannot move any published field either"
    );
    assert_eq!(
        get_account(&mut ctx, ALICE).availableBalance,
        available_before,
        "…while `availableBalance` came back, again unannounced"
    );
}

/// **The emit filter is not a revert filter.** A placement refused by the margin gate emits
/// NOTHING at all — not the (already absent) snapshot, and not `OrderPlaced`/`OrderRested`
/// either. Validate-then-apply: under commit-only there is no undo, so a log emitted before a
/// reject would survive in a batch (the batch selectors catch a per-item error and return Ok).
#[test]
fn a_rejected_placement_emits_nothing() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    // Leave ALICE with no headroom at all, then ask for an order whose ooIM is strictly
    // positive: `available(after) < 0` and `Δ ooIM > 0`, so the gate refuses.
    set_available(&mut ctx, ALICE, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    start_call(&mut ctx);
    let err = place_in(&mut ctx, ALICE, MARKET_ID, 0, PRICE, QTY, 0).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "{err}"
    );
    assert!(
        JournalTr::take_logs(ctx.journal_mut()).is_empty(),
        "a rejected placement must emit no logs at all"
    );
}

/// A call that REVERTS through the shell publishes nothing: the drain is on the success arm.
#[test]
fn a_reverted_call_publishes_no_account_snapshot() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    set_available(&mut ctx, ALICE, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let out = run_perp_dex_call(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: 0,
            price: PRICE,
            quantity: QTY,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
                        flags: 0,
        }
        .abi_encode(),
        10_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(out.reverted, "the margin gate must refuse this");
    assert!(take_balance_events(&mut ctx).is_empty());

    // …and the marks a reverted call left behind do not leak into the NEXT call. This is the
    // scope property: the set lives in `TypedPerpStore`, which the journal keeps for the whole
    // BLOCK, so it is `begin_perp_call` at the top of the shell — not any tx boundary — that
    // makes it call-scoped. Without that reset the deposit below would publish for ALICE too.
    let out = run_perp_dex_call(
        &crate::interface::IPerpDex::getAdminCall {}.abi_encode(),
        1_000_000,
        BOB,
        U256::ZERO,
        true,
        &mut ctx,
    )
    .unwrap();
    assert!(!out.reverted);
    assert!(
        take_balance_events(&mut ctx).is_empty(),
        "a later call must not inherit a reverted call's marks"
    );
}

// ── The coalescing: one settled snapshot per user, last, in address order ─────────────────

/// A crossing fill publishes one snapshot per affected user — the maker's at his fill, the
/// taker's after her order settles — and each equals `getAccount`.
///
/// The counts happen to be unchanged here (one fill, one order, so one each), which is exactly
/// why this is the right place to pin the STREAM SHAPE rather than the counts: what moved is
/// WHERE the maker's row sits. It used to be in the drain at the very end; it is now inside the
/// match flush, immediately after the `Trade` row it closes, with the taker's row after her own
/// `PositionChanged` and before the `OrderRested` of the remainder she rests. So the snapshots
/// are no longer a contiguous suffix, and
/// [`assert_account_update_groups`] is the property that replaces that one.
#[test]
fn a_crossing_fill_publishes_a_snapshot_closing_each_party_s_rows() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    // One lot of liquidity: ALICE's 2-lot GTC buy fills one and rests one, so the same call both
    // moves money (the fill) and rests an order (which on its own would publish nothing).
    place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell");
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let out = run_perp_dex_call(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: 0,
            price: PRICE,
            quantity: 2 * QTY,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
                        flags: 0,
        }
        .abi_encode(),
        10_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!out.reverted);

    let logs = JournalTr::take_logs(ctx.journal_mut());
    // The snapshots are NOT a suffix any more: BOB's sits mid-stream, inside the flush.
    let names = logs
        .iter()
        .map(|log| log.data.topics().first().copied())
        .collect::<Vec<_>>();
    let snapshot_at = |i: usize| names[i] == Some(AccountBalanceChanged::SIGNATURE_HASH);
    let first_snapshot = (0..names.len())
        .find(|i| snapshot_at(*i))
        .expect("snapshots");
    assert!(
        !(first_snapshot..names.len()).all(snapshot_at),
        "the suffix shape is GONE by design — if it came back, the maker's snapshot has drifted \
         out of the flush and back into the drain"
    );
    assert_account_update_groups(&logs);

    let events = logs
        .into_iter()
        .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
        .map(|log| {
            AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
        })
        .collect::<Vec<_>>();
    // BOB first (his fill, inside the flush), then ALICE (her order, after `finalize_apply`).
    // ALICE is written twice inside the call and also rests a remainder, and still gets exactly
    // one: her mark is cleared at her emit, and the resting leg does not re-mark her. The fee
    // rates are 0 here so ADMIN is never credited and never appears — only AFFECTED users do.
    assert_eq!(
        events.iter().map(|e| e.user).collect::<Vec<_>>(),
        vec![BOB, ALICE],
    );
    for e in &events {
        assert_event_matches_get_account(&mut ctx, e);
    }
}

/// A 2-item batch, each item a crossing order into the same maker: **the initiator gets TWO and
/// the maker gets TWO** — one per order and one per fill respectively.
///
/// This is the test whose expectation the granularity change is FOR, so it is worth being
/// explicit about what it used to say and why that was wrong. It used to assert ONE snapshot for
/// the initiator, citing "one final snapshot per affected user per transaction": a K-item batch
/// (K ≤ `MAX_BATCH_PLACE` = 64) published a single event for the initiator. The initiator's side
/// of that was defensible — it is her transaction. The MAKER's was not: BOB also got one event,
/// at the end of a transaction he did not participate in, for two fills that were two separate
/// economic events for him, and the cadence of his notifications was set by how ALICE chose to
/// batch. Both sides are now per-event, and BOB's two rows sit at his two fills.
///
/// The two items are in DIFFERENT markets, which is the leg that makes the maker count
/// non-trivial: each market runs its own `MatchRegistry`, so BOB's two snapshots come from two
/// different working copies, and each has to reconstruct `Σ pos.margin` over the market the other
/// one moved (`UserWork::other_market_position_margin`). A single-market version of this test
/// would leave that term at zero and prove nothing about it.
#[test]
fn a_two_item_batch_publishes_per_order_for_the_initiator_and_per_fill_for_the_maker() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    add_market_2(&mut ctx);
    // Liquidity on both markets, from the same maker.
    place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell m1");
    place_in(&mut ctx, BOB, MARKET_2, 1, PRICE, QTY, 3).expect("maker sell m2");
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let item = |market: u64| PlaceItem {
        marketId: market,
        side: 0,
        price: PRICE,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
                flags: 0,
    };
    let out = run_perp_dex_call(
        &batchPlaceOrdersCall {
            orders: vec![item(MARKET_ID), item(MARKET_2)],
        }
        .abi_encode(),
        30_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(
        !out.reverted,
        "batch reverted: {:?}",
        String::from_utf8_lossy(&out.bytes)
    );

    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_account_update_groups(&logs);
    let events = logs
        .into_iter()
        .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
        .map(|log| {
            AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        events.iter().filter(|e| e.user == ALICE).count(),
        2,
        "the initiator gets one snapshot per ORDER — two items, two orders, got {:?}",
        events.iter().map(|e| e.user).collect::<Vec<_>>()
    );
    assert_eq!(
        events.iter().filter(|e| e.user == BOB).count(),
        2,
        "the maker gets one snapshot per FILL — his two orders are hit once each, and his \
         cadence no longer depends on how the taker batched, got {:?}",
        events.iter().map(|e| e.user).collect::<Vec<_>>()
    );
    assert_eq!(
        events.iter().map(|e| e.user).collect::<Vec<_>>(),
        vec![BOB, ALICE, BOB, ALICE],
        "item by item: maker at the fill, then initiator at the end of that order (fee rates \
         are 0, so no ADMIN credit)"
    );
    // The LAST event per user is the settled state an indexer keeps under last-per-user-wins.
    // Earlier rows for the same user are earlier truths, not wrong ones, so only the last is
    // compared against `getAccount` here.
    for user in [ALICE, BOB] {
        let last = events.iter().rfind(|e| e.user == user).unwrap();
        assert_event_matches_get_account(&mut ctx, last);
    }
}

/// The drain order is ASCENDING ADDRESS order, not write order and not hash-map order — pinned
/// with two users whose writes happen in the opposite order.
///
/// A hash-map iteration order here would make the receipts differ between nodes running the same
/// code, so this is a consensus property rather than a cosmetic one.
#[test]
fn the_drain_order_is_ascending_address_order() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    start_call(&mut ctx);
    // BOB (0x22..) written FIRST, ALICE (0x11..) second.
    storage::mutate_account_balance(&mut ctx, BOB, AccountUpdateReason::Adjustment, |a| {
        a.credit_perp(1)
    })
        .unwrap()
        .unwrap();
    storage::mutate_account_balance(&mut ctx, ALICE, AccountUpdateReason::Adjustment, |a| {
        a.credit_perp(1)
    })
        .unwrap()
        .unwrap();
    end_call(&mut ctx);

    assert_eq!(
        take_balance_events(&mut ctx)
            .iter()
            .map(|e| e.user)
            .collect::<Vec<_>>(),
        vec![ALICE, BOB],
        "drained in address order, which here is the REVERSE of the write order"
    );
}

/// **`setLeverage` publishes NOTHING — and this is the third position that decision has held.**
///
/// It used to publish nothing (the trigger was the ACCOUNT write, and `setLeverage` writes only
/// the position), then one snapshot (the trigger became the position write too), and now nothing
/// again — but for a reason neither of the first two had: **a leverage change cannot move any
/// field this event carries.** The assertions below pin both halves, so "publishes nothing" is
/// not mimicry, it is *consistent* — exactly the shape of
/// [`placement_and_cancel_cannot_move_any_published_field`]:
///
/// * no account blob is written, so `usdcBalance` / `totalCrossWalletBalance` cannot move;
/// * `risk::rebalance_order_margin_for_leverage` is a pure GATE (it takes `&PerpPosition`), so
///   changing leverage on a LIVE position does not reallocate `pos.margin` and the
///   `Σ pos.margin` half of `totalWalletBalance` is unchanged too;
/// * `leverage` is not a field of `AccountBalanceChanged` (nor of Binance's `a.P[]`); it has its
///   own `LeverageChanged`, which IS emitted.
///
/// What forced the third flip is `reason`: a row with no changed field has no truthful reason,
/// and every available label (`ADJUSTMENT`, `ORDER`, `Multiple`) is one a consumer would act on.
/// So the write routes through `storage::save_position_leverage_only`, which marks nobody.
///
/// The GAS is deliberately left at 30_000. The surcharge was originally justified by the
/// snapshot fold, but `rebalance_order_margin_for_leverage` walks the per-user market index on
/// its own (`derived_available_balance_with`), which is the same "walks a per-user list" work the
/// surcharge prices — and lowering a DoS brake is not a side effect this change should have.
#[test]
fn set_leverage_publishes_no_snapshot() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    // The seeded long is leverage 1; raising it is allowed (only REDUCING with an open position
    // is refused).
    let margin_before = pos(&mut ctx, ALICE).margin;
    let account_before = storage::load_account(&mut ctx, ALICE).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let out = run_perp_dex_call(
        &setLeverageCall {
            marketId: MARKET_ID,
            leverage: 2,
        }
        .abi_encode(),
        10_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(
        !out.reverted,
        "setLeverage reverted: {:?}",
        String::from_utf8_lossy(&out.bytes)
    );
    assert_eq!(out.gas_used, 30_000);

    assert!(
        take_balance_events(&mut ctx).is_empty(),
        "a leverage change moves no published field, so it must publish no snapshot"
    );
    // …and the call really did something: the leverage moved, and `LeverageChanged` announced it.
    let after = pos(&mut ctx, ALICE);
    assert_eq!(after.leverage, 2, "the leverage really did change");
    // The three legs of "no published field moved", read off state rather than off the event.
    assert_eq!(
        after.margin, margin_before,
        "changing leverage on a LIVE position must not reallocate `pos.margin` — if it ever \
         does, `totalWalletBalance` moves and this selector owes a snapshot again"
    );
    let account_after = storage::load_account(&mut ctx, ALICE).unwrap();
    assert_eq!(
        (
            account_after.usdc_balance.clone(),
            account_after.perp_wallet_balance,
            account_after.total_position_margin
        ),
        (
            account_before.usdc_balance.clone(),
            account_before.perp_wallet_balance,
            account_before.total_position_margin
        ),
        "no account field behind the published payload may move"
    );
}

// ── Per-fill for makers, per-order for takers ────────────────────────────────────────────

/// Drive one crossing GTC `placeOrder` through the shell and return its whole log stream.
fn crossing_order(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8,
    price: u64,
    qty: u64,
) -> Vec<primitives::Log> {
    crossing_order_tif(ctx, caller, side, price, qty, 0)
}

/// [`crossing_order`] with an explicit TIF — a self-trade needs FOK, the one taker kind
/// self-trade prevention does not apply to.
fn crossing_order_tif(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8,
    price: u64,
    qty: u64,
    tif: u8,
) -> Vec<primitives::Log> {
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let out = run_perp_dex_call(
        &placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: 0,
            tif,
            clientOrderId: FixedBytes::default(),
            flags: 0,
        }
        .abi_encode(),
        10_000_000,
        caller,
        U256::ZERO,
        false,
        ctx,
    )
    .unwrap();
    assert!(
        !out.reverted,
        "crossing order reverted: {}",
        String::from_utf8_lossy(&out.bytes)
    );
    JournalTr::take_logs(ctx.journal_mut())
}

fn snapshots_of(logs: &[primitives::Log]) -> Vec<AccountBalanceChanged> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
        .map(|log| {
            AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
        })
        .collect()
}

/// **The headline of the granularity change.** BOB rests TWO sells in the same level; ALICE
/// sweeps both in ONE order. BOB gets **two** snapshots — one per fill — and ALICE gets **one**,
/// for her one order.
///
/// This is precisely the asymmetry the coalescing hid. Under "one per user per transaction" BOB
/// got a single event, at the end of ALICE's transaction, for two distinct economic events of his
/// own; batch the taker side harder and BOB's two fills, or forty, still collapsed into one
/// notification whose timing he had no part in choosing. A maker order is consumed at most once
/// per taker sweep, so per fill is also exactly per maker ORDER, which is the unit a maker
/// actually reasons about.
///
/// The two rows are DIFFERENT — each is BOB's state at its own fill — which is what makes the
/// count meaningful rather than a duplicated log. The second is his settled state and equals
/// `getAccount`; the first is his state after one lot, and the difference between them is exactly
/// the second lot's effect on `wb`.
#[test]
fn a_maker_filled_twice_in_one_sweep_gets_two_snapshots_and_the_taker_one() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    // Two separate maker orders in the SAME level, so one sweep consumes both from one queue.
    place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell 1");
    place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell 2");

    let logs = crossing_order(&mut ctx, ALICE, 0, PRICE, 2 * QTY);
    assert_account_update_groups(&logs);
    let events = snapshots_of(&logs);

    assert_eq!(
        events.iter().map(|e| e.user).collect::<Vec<_>>(),
        vec![BOB, BOB, ALICE],
        "two fills for BOB → two snapshots, interleaved with his fills; ONE order for ALICE → \
         one snapshot, after it settles"
    );
    // Two fills against the same maker really did happen — the counts above are not one fill
    // reported twice.
    assert_eq!(
        logs.iter()
            .filter(|l| l.data.topics().first() == Some(&Trade::SIGNATURE_HASH))
            .count(),
        2,
        "fixture: the sweep must consume BOTH maker orders"
    );
    // BOB's two rows are two different states, and the second is the settled one.
    assert_ne!(
        (
            events[0].totalWalletBalance,
            events[0].totalCrossWalletBalance
        ),
        (
            events[1].totalWalletBalance,
            events[1].totalCrossWalletBalance
        ),
        "each maker row is that maker's state at ITS OWN fill, not the same row twice"
    );
    assert_event_matches_get_account(&mut ctx, &events[1]);
    assert_event_matches_get_account(&mut ctx, &events[2]);
}

/// A SELF-TRADE publishes **both legs**: the maker-leg snapshot at the fill and the taker-leg
/// snapshot at the end of the order, both for the same address. (A FOK taker: it is the one
/// kind that can still self-trade — STP cancels or stops every other.)
///
/// The owner chose both over suppressing one. The alternative — recognise `maker == taker` and
/// emit once — would make the maker leg conditional on who the taker happens to be, i.e. a
/// maker's own notification would depend on a property of the counterparty. Emitting both keeps
/// each leg's rule unconditional and pushes the reconciliation to where it is trivial:
/// **consumers take the last row per user**, which is already how every after-image event on this
/// ABI has to be read.
///
/// The maker-leg row is the genuinely interesting one: it is derived from the registry working
/// copy that the TAKER leg then goes on to reuse and evolve (`match_order`'s "self-match reuses
/// the evolved copies"), so it is a real intermediate — and the taker row that follows is the
/// settled state.
#[test]
fn a_self_trade_publishes_both_the_maker_leg_and_the_taker_leg() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    place_in(&mut ctx, ALICE, MARKET_ID, 1, PRICE, QTY, 3).expect("ALICE rests a maker sell");

    let logs = crossing_order_tif(&mut ctx, ALICE, 0, PRICE, QTY, 2);
    assert_account_update_groups(&logs);
    let events = snapshots_of(&logs);

    assert_eq!(
        events.iter().map(|e| e.user).collect::<Vec<_>>(),
        vec![ALICE, ALICE],
        "both legs: the maker-leg snapshot at the fill, then the taker-leg one for the order"
    );
    // Last-per-user-wins: the second row is the settled account, the first is not required to be.
    assert_event_matches_get_account(&mut ctx, &events[1]);
}

// ── Cancel-only match participants are SILENT (D9) ─────────────────────────────────────────
//
// A user whose only part in a match is LOSING an order (K9 today; STP's maker cancel next)
// moved no money, so — Binance: "Unfilled orders or cancelled orders will not make the event
// ACCOUNT_UPDATE pushed" — it gets no `AccountBalanceChanged`. The flush used to give it an
// empty drain row, because its unconditional `save_position` / `save_account` marked it.

/// Mark $200 with a band wide enough that $100 fills still execute: a maker SELL at $100 is then
/// an opening short whose equity (0) is below maintenance, so K9 cancels it.
fn k9_mark(ctx: &mut TestCtx) {
    let mut market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    market.mark_price = 200 * TICK;
    market.price_band_bps = 1_000_000;
    storage::save_market(ctx, &market).unwrap();
}

/// CAROL long 1 lot at $100, so her sell is a pure CLOSE that K9 does not gate.
fn carol_long(ctx: &mut TestCtx) {
    fund(ctx, CAROL, WALLET);
    storage::save_position(
        ctx,
        CAROL,
        MARKET_ID,
        &PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -(FILL_VALUE as i64),
            margin: INIT_MARGIN as i64,
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

/// The K9-cancelled maker (BOB, no pending funding) publishes NOTHING: its `OrderCancelled` is
/// the whole story. CAROL, filled behind it, and ALICE, the taker, keep their groups.
#[test]
fn a_k9_cancelled_maker_without_funding_publishes_no_account_update() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    carol_long(&mut ctx);
    let bob_sell = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, CAROL, 1, PRICE, QTY, 0, 0);
    k9_mark(&mut ctx);
    let bob_wallet = wallet(&mut ctx, BOB);

    let logs = crossing_order(&mut ctx, ALICE, 0, PRICE, QTY);
    assert_account_update_groups(&logs);
    assert_eq!(
        stream_shape(&logs),
        vec![
            ("OrderPlaced", Some(ALICE)),
            ("OrderCancelled", Some(BOB)),
            ("Trade", Some(ALICE)),
            ("AccountBalanceChanged", Some(CAROL)),
            ("PositionChanged", Some(CAROL)),
            ("AccountBalanceChanged", Some(ALICE)),
            ("PositionChanged", Some(ALICE)),
        ],
        "no row for BOB — before the fix he got an empty drain row at the end"
    );
    // …while his state really did change in the one way a cancel changes it.
    assert_terminal(&mut ctx, bob_sell);
    assert_eq!(pos(&mut ctx, BOB).total_sell_qty, 0, "his Ask shrank");
    assert_eq!(wallet(&mut ctx, BOB), bob_wallet, "and no money moved");
}

/// The K9-cancelled maker WITH pending funding publishes exactly its funding group — the money
/// that did move — and no second, empty row for the cancel.
#[test]
fn a_k9_cancelled_maker_with_funding_publishes_only_its_funding_group() {
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 1_000_000);
    set_mark(&mut ctx, PRICE);
    fund(&mut ctx, CAROL, WALLET);
    // CAROL short 1 lot, then a stranded ask at $1 that K9 refuses to fill (see
    // `an_insolvency_rejected_maker_still_reports_its_funding_adjusted_margin`).
    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0);
    place(&mut ctx, CAROL, 1, PRICE, QTY, 0, 0);
    try_place_limit(&mut ctx, CAROL, 1, TICK).unwrap();
    storage::save_funding_state(
        &mut ctx,
        MARKET_ID,
        &FundingState {
            last_funding_rate: -1_000,
            next_funding_ts: u64::MAX,
            cumulative_funding_index: -(PRICE as i128) * 1_000,
        },
    )
    .unwrap();
    let margin_before = pos(&mut ctx, CAROL).margin;

    let logs = crossing_order(&mut ctx, ALICE, 0, TICK, QTY);
    assert_account_update_groups(&logs);
    assert_eq!(pos(&mut ctx, CAROL).margin, margin_before - 1_000, "fixture: funding paid");
    let carol: Vec<_> = account_update_reasons(&logs)
        .into_iter()
        .filter(|(u, _)| *u == CAROL)
        .collect();
    assert_eq!(
        carol,
        vec![(CAROL, AccountUpdateReason::FundingFee)],
        "the funding group and nothing else"
    );
}

/// A taker whose ONLY interaction is cancelling its own resting order (a zero-fill self-match)
/// joins the registry through that cancel — and must then be as silent as any pure placement:
/// `OrderPlaced`, the cancel, `OrderRested`, and no account row.
///
/// This used to be reached through K9 (the K9 mark is kept, so the K9 path would refuse this
/// maker too). Self-trade prevention now runs BEFORE the maker settle, so the own maker never
/// reaches K9: the cancel is STP's (reason 9). A K9 cancel of the taker's own order is only
/// reachable on a FOK self-match, which cannot be zero-fill.
#[test]
fn a_zero_fill_self_match_publishes_no_account_update() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let own_sell = place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);
    k9_mark(&mut ctx);

    let logs = crossing_order(&mut ctx, ALICE, 0, PRICE, QTY);
    assert_eq!(
        stream_shape(&logs),
        vec![
            ("OrderPlaced", Some(ALICE)),
            ("OrderCancelled", Some(ALICE)),
            ("OrderRested", Some(ALICE)),
        ],
    );
    let reason = logs
        .iter()
        .find(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
        .map(|l| OrderCancelled::decode_raw_log(l.data.topics(), &l.data.data).unwrap().reason)
        .unwrap();
    assert_eq!(reason, CancelReason::SelfTradePrevention as u8);
    assert_terminal(&mut ctx, own_sell);
    assert_eq!(pos(&mut ctx, ALICE).amount, 0);
}

/// The account-bytes leg of the predicate is not a formality. The fee recipient whose own maker
/// order is K9-cancelled joins the registry through that cancel; a LATER maker's fee is then
/// credited onto its working copy (`MatchRegistry::credit_admin`'s read-through). It published
/// nothing and its position is untouched — but its WALLET moved, so it must keep the old route:
/// skipping its `save_account` would not just drop the row, it would drop the fee.
#[test]
fn a_cancel_only_fee_recipient_still_banks_the_fee_and_gets_its_row() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, ADMIN, WALLET);
    carol_long(&mut ctx);
    storage::save_user_fee_rates(
        &mut ctx,
        CAROL,
        UserFeeRates {
            maker_fee_ppm: 1_000,
            taker_fee_ppm: 0,
        },
    )
    .unwrap();
    // ADMIN first in the FIFO (K9 cancels it: an opening short), CAROL behind (a close: filled).
    place(&mut ctx, ADMIN, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, CAROL, 1, PRICE, QTY, 0, 0);
    k9_mark(&mut ctx);
    let admin_before = wallet(&mut ctx, ADMIN);

    let logs = crossing_order(&mut ctx, ALICE, 0, PRICE, QTY);
    let fee = calc_trading_fee(FILL_VALUE, 1_000).unwrap();
    assert!(fee > 0, "fixture: a real maker fee");
    assert_eq!(wallet(&mut ctx, ADMIN), admin_before + fee, "the fee is banked");
    assert!(
        account_update_reasons(&logs).contains(&(ADMIN, AccountUpdateReason::Order)),
        "and announced: {:?}",
        account_update_reasons(&logs)
    );
}

/// The trap in the silence predicate: a self-trade that ROUND-TRIPS (sell 1 as maker, buy it
/// back as taker, one price, no fees) leaves `amount`, `margin` and the account blob exactly as
/// stored — yet it TRADED and published a maker-leg row. "Unchanged ⇒ published nothing" is
/// false here, which is why `published.is_none()` is part of the predicate rather than an
/// assertion about it: this test panics on the assertion form. Both legs still publish.
#[test]
fn a_round_trip_self_trade_still_publishes_both_legs() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let fees = storage::load_user_fee_rates(&mut ctx, ALICE).unwrap();
    assert_eq!((fees.maker_fee_ppm, fees.taker_fee_ppm), (0, 0), "fixture: no fees");
    let account_before = storage::load_account(&mut ctx, ALICE).unwrap();
    place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);

    let _ = JournalTr::take_logs(ctx.journal_mut());
    let out = run_perp_dex_call(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: 0,
            price: PRICE,
            quantity: QTY,
            orderType: 0,
            tif: 2, // FOK
            clientOrderId: FixedBytes::default(),
            flags: 0,
        }
        .abi_encode(),
        10_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!out.reverted, "{:?}", String::from_utf8_lossy(&out.bytes));
    let logs = JournalTr::take_logs(ctx.journal_mut());

    let p = pos(&mut ctx, ALICE);
    assert_eq!((p.amount, p.margin), (0, 0), "fixture: the position round-tripped to flat");
    let mut account_after = storage::load_account(&mut ctx, ALICE).unwrap();
    account_after.nonce = account_before.nonce;
    assert_eq!(account_after, account_before, "fixture: the account round-tripped too");
    assert_eq!(
        snapshots_of(&logs).iter().map(|e| e.user).collect::<Vec<_>>(),
        vec![ALICE, ALICE],
        "maker-leg row + taker-leg row, as for any self-trade"
    );
}

/// **The one branch where a bug LOSES an event: the fee recipient is also a maker.**
///
/// ADMIN rests a sell at `PRICE`; BOB rests one a tick worse. ALICE sweeps both, so ADMIN's own
/// fill comes FIRST and BOB's second — and BOB's maker fee is credited to ADMIN through
/// `MatchRegistry::credit_admin`'s read-through, landing on ADMIN's working copy **after** ADMIN's
/// per-fill snapshot was already derived from it. ADMIN's state therefore moves once more, with no
/// further event of ADMIN's own to hang a snapshot on.
///
/// So ADMIN gets **two** rows: the per-fill one, and a drain one carrying the later fee. That
/// second row exists only because `MatchRegistry::flush` gates the mark-clear on the settled
/// payload equalling what was published, rather than on "I emitted something for this user".
/// An unconditional clear passes every other test in this module and silently drops this update —
/// which is why this test exists, and why it asserts the DELTA rather than just the count.
///
/// ⚠️ ALICE's taker fee is deliberately left at 0. A non-zero one would make `finalize_apply`'s
/// `credit_fee_recipient` mark ADMIN *after* the flush as well, so the drain row would be
/// explained by either cause and the branch under test would no longer be isolated.
#[test]
fn a_fee_recipient_who_is_also_a_maker_gets_a_drain_row_for_the_later_fee() {
    let mut ctx = make_ctx();
    fixture(&mut ctx);
    fund(&mut ctx, ADMIN, WALLET);
    // BOB pays a maker fee; ADMIN's own fill pays none, so the only fee in this call is the one
    // credited to ADMIN AFTER ADMIN's snapshot.
    storage::save_user_fee_rates(
        &mut ctx,
        BOB,
        UserFeeRates {
            maker_fee_ppm: 1_000,
            taker_fee_ppm: 0,
        },
    )
    .unwrap();
    assert_eq!(
        storage::load_user_fee_rates(&mut ctx, ALICE)
            .unwrap()
            .taker_fee_ppm,
        0,
        "fixture: the taker must pay no fee, or the drain row is not attributable"
    );

    // ADMIN at the better price is hit FIRST; BOB's fill (and therefore BOB's fee) follows.
    place_in(&mut ctx, ADMIN, MARKET_ID, 1, PRICE, QTY, 3).expect("ADMIN maker sell");
    place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE + TICK, QTY, 3).expect("BOB maker sell");

    let logs = crossing_order(&mut ctx, ALICE, 0, PRICE + TICK, 2 * QTY);
    assert_account_update_groups(&logs);
    let events = snapshots_of(&logs);

    assert_eq!(
        events.iter().map(|e| e.user).collect::<Vec<_>>(),
        vec![ADMIN, BOB, ALICE, ADMIN],
        "ADMIN twice: once at ADMIN's own fill, once from the drain for BOB's fee — which \
         arrived after that fill's snapshot had already been taken"
    );
    // ── The KNOWN live drain conflict, and why it is NOT a `Multiple` ────────────────────────
    //
    // ADMIN is marked twice here by two different economic events — their own maker fill (the
    // flush's `save_position` / `save_account`) and BOB's fee credit (`credit_admin`'s
    // read-through, materialised at the flush) — and the drain publishes ONE row for both. Both
    // marks carry `Order`, because a trading fee IS money moved by a matched order, so
    // `AccountUpdateReason::merge` is a no-op and the row keeps a truthful label rather than
    // degrading to `Multiple`. That is the case the merge rule was checked against first.
    assert_eq!(
        account_update_reasons(&logs),
        vec![
            (ADMIN, AccountUpdateReason::Order),
            (BOB, AccountUpdateReason::Order),
            (ALICE, AccountUpdateReason::Order),
            (ADMIN, AccountUpdateReason::Order),
        ],
        "the fee recipient's drain row reports ORDER — a fee is order-driven money"
    );

    // The two ADMIN rows differ by exactly BOB's maker fee, which is the whole point: the second
    // row carries information the first could not have.
    //
    //   calc_value(PRICE + TICK, QTY, 8, 9) = 101 * 1e9 * 1e6 * 1e6 / (1e9 * 1e8) = 1_010_000
    //   fee = 1_010_000 * 10 bps / 10_000                                        =     1_010
    const BOB_MAKER_FEE: i64 = 1_010;
    let (first, second) = (&events[0], &events[3]);
    assert_eq!(
        second.totalCrossWalletBalance - first.totalCrossWalletBalance,
        BOB_MAKER_FEE,
        "the drain row must carry the later fee credit, not repeat the fill row"
    );
    assert_eq!(
        second.totalWalletBalance - first.totalWalletBalance,
        BOB_MAKER_FEE,
        "…and it lands on the wallet, so `wb` moves with `cw`"
    );
    // The LAST row is the settled account. (The first is not, and must not be asserted to be.)
    assert_event_matches_get_account(&mut ctx, second);
}

/// The whole snapshot stream — order, users and payloads — is **byte-for-byte reproducible**.
///
/// Two identical scenarios built in two independent contexts must produce the same sequence.
/// Emission is no longer a single `BTreeSet` drain, so determinism now rests on three ordered
/// things instead of one: the match walk's fill order, `MatchRegistry`'s event replay (a `Vec`,
/// in push order), and the drain's `BTreeSet`. A `HashMap` anywhere in that chain would make
/// receipts differ between nodes running the same code, which is a consensus bug rather than a
/// cosmetic one — so this is pinned across a scenario that exercises all three: two makers, a
/// multi-fill sweep, a resting remainder and a fee-earning ADMIN.
#[test]
fn the_snapshot_stream_is_deterministic() {
    fn run() -> Vec<(Address, i64, i64)> {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        // Non-zero maker fee → ADMIN is credited and joins the stream through the drain.
        storage::save_user_fee_rates(
            &mut ctx,
            BOB,
            UserFeeRates {
                maker_fee_ppm: 100,
                taker_fee_ppm: 0,
            },
        )
        .unwrap();
        fund(&mut ctx, CAROL, WALLET);
        place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("BOB sell");
        place_in(&mut ctx, CAROL, MARKET_ID, 1, PRICE, QTY, 3).expect("CAROL sell");
        // 3 lots: two fills, one lot rests.
        let logs = crossing_order(&mut ctx, ALICE, 0, PRICE, 3 * QTY);
        assert_account_update_groups(&logs);
        snapshots_of(&logs)
            .into_iter()
            .map(|e| (e.user, e.totalWalletBalance, e.totalCrossWalletBalance))
            .collect()
    }

    let first = run();
    assert_eq!(first, run(), "the snapshot stream must be reproducible");
    // …and it is the stream this change is about, not an empty one: both makers report at their
    // own fills (FIFO: BOB's order rested first), the taker once, ADMIN last from the drain.
    assert_eq!(
        first.iter().map(|(u, ..)| *u).collect::<Vec<_>>(),
        vec![BOB, CAROL, ALICE, ADMIN]
    );
}
