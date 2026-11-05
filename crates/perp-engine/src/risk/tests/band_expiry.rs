use super::*;
use crate::interface::IPerpDex::OrderCancelled;
use crate::types::CancelReason;

fn oracle_update(ctx: &mut TestCtx, index: u64, ts: u64) {
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: index,
            timestamp: ts,
        }
        .abi_encode(),
        ADMIN,
        ctx,
    )
    .unwrap();
}

/// The band tests rest dozens of orders, and one unit of this market is $100 of notional, so
/// `setup_market`'s $50 / $2 000 wallets do not stretch. Fund generously — none of these tests
/// is about the affordability gate.
fn fund_big(ctx: &mut TestCtx, user: Address) {
    storage::save_account(
        ctx,
        user,
        UserAccount {
            perp_wallet_balance: 100_000_000_000_000, // $100M
            ..UserAccount::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

fn asks(ctx: &mut TestCtx) -> Vec<u64> {
    storage::load_ask_prices(ctx, MARKET_ID).unwrap()
}

fn bids(ctx: &mut TestCtx) -> Vec<u64> {
    storage::load_bid_prices(ctx, MARKET_ID).unwrap()
}

fn cancel_reasons(ctx: &mut TestCtx) -> Vec<u8> {
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
        .map(|l| {
            OrderCancelled::decode_raw_log(l.data.topics(), &l.data.data)
                .unwrap()
                .reason
        })
        .collect()
}

fn post_only(
    ctx: &mut TestCtx,
    user: Address,
    side: u8,
    price: u64,
) -> Result<Bytes, PerpError> {
    run_place_order(
        &placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: 1,
            orderType: 0, // Limit
            tif: 3,       // PostOnly
            clientOrderId: FixedBytes::default(),
                        flags: 0,
        }
        .abi_encode(),
        user,
        ctx,
    )
}

/// An **IOC** taker. Used where the fixture needs a crossing order that provably reaches an
/// out-of-band level but must not REST: crossing a level past the far band edge requires a limit
/// past that edge too, and a resting order that would become a too-good best is refused at
/// placement. IOC drops the remainder, so the only thing under test is the fill-time band.
fn ioc(ctx: &mut TestCtx, user: Address, side: u8, price: u64) -> Result<Bytes, PerpError> {
    run_place_order(
        &placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: 1,
            orderType: 0, // Limit
            tif: 1,       // IOC
            clientOrderId: FixedBytes::default(),
                        flags: 0,
        }
        .abi_encode(),
        user,
        ctx,
    )
}

/// PRECONDITION helper: `price` really is outside the band at the CURRENT mark, and really is
/// the cached best on `side` — the two things every fixture below silently depends on. Read from
/// stored state, never recomputed from a comment, so it cannot drift away from the market the
/// test actually runs against.
fn assert_out_of_band_best(ctx: &mut TestCtx, side: Side, price: u64) {
    let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    let (upper, lower) =
        crate::math::mark_band_bounds(market.mark_price, market.price_band_bps);
    let p = price as u128;
    assert!(
        p < lower || p > upper,
        "precondition: {price} must be OUTSIDE the band [{lower}, {upper}] at mark {}",
        market.mark_price
    );
    let best = match side {
        Side::Buy => storage::load_best_bid(ctx, MARKET_ID),
        Side::Sell => storage::load_best_ask(ctx, MARKET_ID),
    }
    .unwrap();
    assert_eq!(
        best, price,
        "precondition: the out-of-band level must be the cached best — that is the entire \
         reason it is harmful, and if it is not, every assertion below passes vacuously"
    );
}

/// Move the mark WITHOUT running `updateIndexPrice`, i.e. without the band-expiry sweep.
///
/// This is the only way to set up a stranded book now that placement refuses a too-good new
/// best: the level must be placed while the band contains it and then be stranded by a mark
/// move. Going through the oracle instead would run the very GC these fixtures are trying to
/// observe (or to prove has NOT yet run).
fn drift_mark(ctx: &mut TestCtx, mark: u64) {
    storage::save_mark_price(ctx, MARKET_ID, mark).unwrap();
}

/// A mark JUMP UP strands the whole cheap end of the ask book below the new lower edge. Those
/// levels are the touch and can never fill, so they go; the level that is still in band, and the
/// far-side level ABOVE the new upper edge, both survive.
#[test]
fn a_mark_jump_expires_the_stranded_ask_prefix_and_spares_the_far_side() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // In band at the OLD mark ($100 ⇒ [$90, $110]) …
    fund_big(&mut ctx, MAKER);
    place_order(&mut ctx, MAKER, 1, 9_500, 1);
    place_order(&mut ctx, MAKER, 1, 9_600, 1);
    // … and two that are out of band already, ABOVE the upper edge. Placement allows these: an
    // ask above the band is a FAR-side quote, which is exactly the deep passive liquidity the
    // one-sided placement reject deliberately keeps
    // (`trading::tests::a_far_side_out_of_band_quote_still_rests_even_as_the_new_best`). Note
    // they never become the best ask either — 9_500 is already in front of them.
    place_order(&mut ctx, MAKER, 1, 14_000, 1);
    place_order(&mut ctx, MAKER, 1, 17_000, 1);
    assert_eq!(asks(&mut ctx), vec![9_500, 9_600, 14_000, 17_000]);

    // Mark $100 → $150 ⇒ band [$135, $165]. 9_500/9_600 are now the stranded near-side prefix;
    // 14_000 is in band; 17_000 is a deep passive order on the far side.
    oracle_update(&mut ctx, 15_000, 31);
    assert_eq!(
        storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(),
        15_000
    );

    assert_eq!(
        asks(&mut ctx),
        vec![14_000, 17_000],
        "the contiguous below-band prefix is expired; the in-band level and the far-side deep \
         ask both survive"
    );
    let best_ask = storage::load_best_ask(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(best_ask, 14_000, "the BBO cache follows the removals");
    let (upper, lower) = crate::math::mark_band_bounds(15_000, 0);
    assert!(
        (best_ask as u128) >= lower && (best_ask as u128) <= upper,
        "the post-GC best ask is IN band: {best_ask} vs [{lower}, {upper}]"
    );
}

/// The bid mirror: bids descend, so the stranded prefix is the levels ABOVE the upper edge, and
/// the deep bid BELOW the lower edge is the one that must be left alone.
#[test]
fn a_mark_drop_expires_the_stranded_bid_prefix_and_spares_the_far_side() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund_big(&mut ctx, MAKER);
    // The two TOP bids are the ones this test needs stranded, and a stranded bid is only
    // reachable by drift: a bid above the upper edge is too-good, and bids rank better-first, so
    // it is necessarily the best bid — which placement refuses. So the whole book is built at a
    // mark that CONTAINS the top bids ($120 ⇒ [$108, $132]) and the drop below strands them.
    drift_mark(&mut ctx, 12_000);
    place_order(&mut ctx, MAKER, 0, 5_000, 1); // far side after the drop
    place_order(&mut ctx, MAKER, 0, 6_000, 1); // in band after the drop
    place_order(&mut ctx, MAKER, 0, 11_500, 1);
    place_order(&mut ctx, MAKER, 0, 12_000, 1);
    assert_eq!(bids(&mut ctx), vec![5_000, 6_000, 11_500, 12_000]);
    let (upper_at_placement, _) = crate::math::mark_band_bounds(12_000, 0);
    assert!(
        12_000u128 <= upper_at_placement,
        "precondition: the top bid really was INSIDE the band when it was placed"
    );

    // Mark $120 → $60 ⇒ band [$54, $66].
    oracle_update(&mut ctx, 6_000, 31);
    assert_eq!(
        storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(),
        6_000
    );
    // The three roles this fixture asserts must actually be occupied, or "the far side is
    // spared" is a claim about nothing.
    let (upper_new, lower_new) = crate::math::mark_band_bounds(6_000, 0);
    for doomed in [11_500u128, 12_000] {
        assert!(
            doomed > upper_new,
            "precondition: removed bid {doomed} really is in the ABOVE-band prefix vs \
             [{lower_new}, {upper_new}]"
        );
    }
    assert!(
        5_000u128 < lower_new,
        "precondition: the spared bid really is on the FAR side (below the lower edge), not \
         merely in band"
    );
    assert!(
        6_000u128 >= lower_new && 6_000u128 <= upper_new,
        "precondition: and the survivor at the touch really is in band"
    );

    assert_eq!(
        bids(&mut ctx),
        vec![5_000, 6_000],
        "the contiguous above-band prefix is expired; the in-band bid and the deep passive bid \
         below the band both survive"
    );
    let best_bid = storage::load_best_bid(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(best_bid, 6_000);
    let (upper, lower) = crate::math::mark_band_bounds(6_000, 0);
    assert!(
        (best_bid as u128) >= lower && (best_bid as u128) <= upper,
        "the post-GC best bid is IN band: {best_bid} vs [{lower}, {upper}]"
    );
}

/// The cap is on ORDERS, not levels: 70 orders at ONE price take two updates to clear, 64 then
/// 6. Between them the level keeps resting — deferral, not loss.
#[test]
fn the_cap_is_on_orders_and_a_second_update_finishes_the_job() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund_big(&mut ctx, MAKER);
    let total = MAX_BAND_EXPIRIES_PER_UPDATE + 6;
    for _ in 0..total {
        place_order(&mut ctx, MAKER, 1, 9_500, 1);
    }
    assert_eq!(
        storage::load_ask_count(&mut ctx, MARKET_ID, 9_500).unwrap(),
        total as u64
    );

    oracle_update(&mut ctx, 15_000, 31);
    assert_eq!(
        storage::load_ask_count(&mut ctx, MARKET_ID, 9_500).unwrap(),
        6,
        "exactly MAX_BAND_EXPIRIES_PER_UPDATE orders expired in the first update"
    );
    assert_eq!(
        asks(&mut ctx),
        vec![9_500],
        "the level is not empty yet, so its price is still in the index"
    );

    oracle_update(&mut ctx, 15_000, 61);
    assert_eq!(
        storage::load_ask_count(&mut ctx, MARKET_ID, 9_500).unwrap(),
        0
    );
    assert!(
        asks(&mut ctx).is_empty(),
        "the second update drains the remainder and drops the price"
    );
}

/// The expiry is ATTRIBUTED: `OrderCancelled.reason` separates a protocol kill from the owner's
/// own cancel, which the event could not do when it was three indexed fields and no data.
#[test]
fn the_expiry_event_carries_the_gc_reason_and_a_user_cancel_carries_its_own() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund_big(&mut ctx, MAKER);
    place_order(&mut ctx, MAKER, 1, 9_500, 1);
    let keep = try_place_order(&mut ctx, MAKER, 1, 14_000, 1).unwrap();
    let keep: [u8; 32] = keep[..32].try_into().unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());

    oracle_update(&mut ctx, 15_000, 31);
    assert_eq!(
        cancel_reasons(&mut ctx),
        vec![CancelReason::PriceBandExpiry as u8],
        "the stranded ask is expired by the protocol, and says so"
    );

    crate::trading::run_cancel_order(
        &crate::interface::IPerpDex::cancelOrderCall {
            orderId: FixedBytes(keep),
            marketId: MARKET_ID,
        }
        .abi_encode(),
        MAKER,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        cancel_reasons(&mut ctx),
        vec![CancelReason::User as u8],
        "the owner's own cancel is distinguishable from the kill above"
    );
}

/// The aggregates shrink by the entry's FROZEN `assuming_price`, never by the limit price and
/// never by a re-derivation at the current mark. A marked-up sell makes the three disagree: the
/// limit basis would strand the markup in `Ask` forever, and a live re-derivation at the jumped
/// mark would OVERSHOOT and trip the underflow invariant.
#[test]
fn expiry_shrinks_the_aggregates_by_the_frozen_assuming_price() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund_big(&mut ctx, MAKER);
    // Sell limit $95 with the mark at $100: Assuming Price = max(lastTraded×1.0015, mark, limit)
    // = 10_000 ⇒ notional 100, where the limit basis would say 95.
    place_order(&mut ctx, MAKER, 1, 9_500, 1);
    let pos = position(&mut ctx, MAKER);
    assert_eq!(
        (pos.total_sell_qty, pos.total_sell_notional),
        (1, 100_000_000),
        "Ask is priced at the Assuming Price ($100.00), not the limit price ($95.00)"
    );

    let before = conservation_sum(&mut ctx, &[MAKER]);
    oracle_update(&mut ctx, 15_000, 31);

    let pos = position(&mut ctx, MAKER);
    assert_eq!(
        (pos.total_sell_qty, pos.total_sell_notional),
        (0, 0),
        "the aggregate returns EXACTLY to zero — the frozen basis, not the limit price \
         (would leave 5_000_000) and not the new mark (would underflow)"
    );
    assert_eq!(
        conservation_sum(&mut ctx, &[MAKER]),
        before,
        "expiry moves no money: there is no escrow to refund"
    );
}

/// Harm #1, and its fix. `ensure_post_only_does_not_cross` reads the cached best and knows
/// nothing about the band, so an unmatchable bid ABOVE the upper edge makes it reject a
/// legitimate PostOnly sell as "would match" against a level that can never fill.
#[test]
fn a_post_only_sell_mis_rejected_by_an_unmatchable_bid_is_accepted_after_the_gc() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund_big(&mut ctx, MAKER);
    fund_big(&mut ctx, ALICE);
    // A bid at $120 against an empty ask side, placed while the band contains it ($120 mark ⇒
    // [$108, $132]) and then stranded by a mark DRIFT back to $100 ⇒ [$90, $110]. Drift is the
    // only route: at the $100 mark this bid is a too-good new best and placement refuses it.
    // `drift_mark`, not the oracle, because an oracle update runs the GC — and the whole point
    // here is to observe the state BEFORE the GC collects it.
    drift_mark(&mut ctx, 12_000);
    place_order(&mut ctx, MAKER, 0, 12_000, 1);
    drift_mark(&mut ctx, 10_000);
    // It is out of band and it is nonetheless `best_bid` — the two properties that make the
    // PostOnly mis-reject below possible at all.
    assert_out_of_band_best(&mut ctx, Side::Buy, 12_000);

    // Proof the level is unmatchable: a crossing taker sell fills nothing. IOC, because a
    // resting sell that crossed $120 would have to be below the lower edge and would be refused
    // as a too-good best — which would prove nothing about the fill-time band.
    let taker = ioc(&mut ctx, ALICE, 1, 100).unwrap();
    let taker: [u8; 32] = taker[..32].try_into().unwrap();
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "the fill-time band already refuses this level"
    );
    assert!(
        storage::load_order(&mut ctx, &taker).unwrap().is_none(),
        "the IOC expired its whole remainder (delete-on-terminal), so it rested nothing and \
         left the book below untouched"
    );
    assert_eq!(
        bids(&mut ctx),
        vec![12_000],
        "and the unmatchable bid is still there, unfilled"
    );

    let err = post_only(&mut ctx, ALICE, 1, 11_500).unwrap_err();
    assert!(
        err.to_string().contains("PostOnly order would match"),
        "the pre-GC mis-reject: {err}"
    );

    // The mark update collects it (the band centre does not even have to move — the trigger is
    // the update, which is where a stranded level becomes reachable).
    oracle_update(&mut ctx, 10_000, 31);
    assert!(
        bids(&mut ctx).is_empty(),
        "the unmatchable bid is gone: {:?}",
        bids(&mut ctx)
    );

    post_only(&mut ctx, ALICE, 1, 11_500).expect("the same PostOnly sell is now accepted");
    assert_eq!(bids(&mut ctx).len(), 0);
    assert_eq!(asks(&mut ctx), vec![11_500]);
}

/// A market with the band DISABLED must GC nothing — `mark_band_bounds` collapses the lower edge
/// to 0 and blows the upper edge past any price, so neither prefix exists. (This is what keeps
/// the benches and the golden commitment scenario, both of which disable the band, untouched.)
#[test]
fn a_disabled_band_expires_nothing() {
    // Each side needs its own book: with the band disabled a bid above an ask simply TRADES, so
    // the two cases cannot share one context.
    fn disable_band(ctx: &mut TestCtx) {
        let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
        storage::save_market(
            ctx,
            &Market {
                price_band_bps: 1_000_000,
                ..market
            },
        )
        .unwrap();
    }

    // Ask below what would be the lower edge of an active band at the new mark.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund_big(&mut ctx, MAKER);
    disable_band(&mut ctx);
    place_order(&mut ctx, MAKER, 1, 9_500, 1);
    oracle_update(&mut ctx, 15_000, 31);
    assert_eq!(
        asks(&mut ctx),
        vec![9_500],
        "an active ±10% band at mark 15_000 would have expired this; a disabled one must not"
    );

    // Bid above what would be the upper edge.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund_big(&mut ctx, MAKER);
    disable_band(&mut ctx);
    place_order(&mut ctx, MAKER, 0, 12_000, 1);
    oracle_update(&mut ctx, 6_000, 31);
    assert_eq!(bids(&mut ctx), vec![12_000]);
}

/// Harm #2, executable: an unmatchable level at the touch reaches the MARK's own inputs. Every
/// best-quote change records a mid-price sample into the price-basis window, with no knowledge of
/// the band, and that window is `moving_average_basis` → `price2` → the `median(price1, price2,
/// contract)` that IS the mark. So a quote that can never trade steers the mark.
#[test]
fn an_unmatchable_quote_at_the_touch_reaches_the_mark_input_window() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund_big(&mut ctx, MAKER);
    assert_eq!(
        storage::load_price_basis_window(&mut ctx, MARKET_ID)
            .unwrap()
            .count,
        0
    );

    // $120 placed while the band contains it ($120 mark), then stranded by a drift back to the
    // $100 mark ⇒ [$90, $110]. Its own placement sample is recorded on the way in.
    drift_mark(&mut ctx, 12_000);
    place_order(&mut ctx, MAKER, 0, 12_000, 1);
    let w = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(
        (w.count, w.last_mid_price),
        (1, 12_000),
        "the quote entered the mark-input window at placement"
    );
    drift_mark(&mut ctx, 10_000);
    assert_out_of_band_best(&mut ctx, Side::Buy, 12_000);

    // THE POLLUTION, live and post-drift: the level can no longer trade, and it is STILL a mark
    // input. The next best-quote change on the other side samples the MID, and the mid is
    // dragged up by the unmatchable bid — 11_250 instead of the 10_500 the only tradeable quote
    // in the book would give on its own. (A fresh block timestamp is required: `record_
    // observation` takes at most one sample per timestamp, so the placement sample above would
    // otherwise swallow this one.)
    ctx.block.timestamp = U256::from(2);
    place_order(&mut ctx, MAKER, 1, 10_500, 1);
    let w = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(
        (w.count, w.last_mid_price),
        (2, 11_250),
        "an unmatchable level still steers the mark: mid = (12_000 + 10_500)/2, where the \
         tradeable side alone says 10_500"
    );

    // The GC takes the level out, so it can contribute no further samples.
    oracle_update(&mut ctx, 10_000, 31);
    assert!(bids(&mut ctx).is_empty());
    assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), 0);
    assert_eq!(
        asks(&mut ctx),
        vec![10_500],
        "and only the unmatchable side went: the in-band ask is untouched"
    );
    assert_eq!(
        storage::load_price_basis_window(&mut ctx, MARKET_ID)
            .unwrap()
            .count,
        2,
        "the removal itself adds no sample (same block timestamp as the sample above)"
    );
}

/// A mass expiry across many users moves **NO MONEY AT ALL** — not just "conserves value".
/// There is no escrow behind a resting order, so expiry only shrinks `Bid`/`Ask`; every wallet
/// is byte-identical afterwards.
///
/// And it publishes **NO `AccountBalanceChanged`**, which is worth pinning because it is a real
/// consequence, not an oversight: shrinking `Bid`/`Ask` RAISES each of these users'
/// `availableBalance` (ooIM is derived from those aggregates), yet the removal is an
/// aggregates-only write, and the snapshot trigger deliberately does not mark those — see the
/// table on `storage::mark_account_snapshot_dirty`, whose rule is "no wallet moved and no
/// position state moved" and whose citation is Binance's measured "cancelled orders will not
/// make the event ACCOUNT_UPDATE pushed". The trigger is consistent here, and it is exactly why
/// `OrderCancelled.reason` had to be part of this change: `reason != 0` is the only signal a
/// stream consumer gets that its cached `availableBalance` needs a `getAccount` re-read.
#[test]
fn a_mass_expiry_moves_no_money_and_publishes_no_account_snapshot() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let users = [ALICE, MAKER, KEEPER];
    for u in users {
        fund_big(&mut ctx, u);
    }
    // Both sides, several levels each, several orders per level, three owners interleaved so no
    // per-user grouping is implied by the input order.
    for (n, u) in users.iter().enumerate() {
        for k in 0..4u64 {
            place_order(&mut ctx, *u, 1, 9_100 + k * 100, 1); // asks, stranded by the jump
            place_order(&mut ctx, *u, 0, 9_000 - k * 100, 1); // bids, far side after the jump
        }
        assert!(n < 3);
    }
    let wallets_before: Vec<i64> = users
        .iter()
        .map(|u| {
            storage::load_account(&mut ctx, *u)
                .unwrap()
                .perp_wallet_balance
        })
        .collect();
    let value_before = conservation_sum(&mut ctx, &users);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    // These tests drive handlers directly, so nothing has drained the snapshot marks that the
    // `fund_big` account writes left behind. The shell does this at the head of every call.
    storage::begin_perp_call(&mut ctx);

    // Mark $100 → $150 ⇒ band [$135, $165]: every ask (91–94) is stranded below it, and every
    // bid (87–90) is below it too — i.e. on the FAR side, where bids are harmless.
    oracle_update(&mut ctx, 15_000, 31);
    storage::flush_account_snapshots(&mut ctx).unwrap();

    assert!(
        asks(&mut ctx).is_empty(),
        "all four ask levels are in the stranded prefix: {:?}",
        asks(&mut ctx)
    );
    assert_eq!(
        bids(&mut ctx),
        vec![8_700, 8_800, 8_900, 9_000],
        "the bids are BELOW the new band — deep passive liquidity, not the touch. They must all \
         survive; expiring them would be the mass-kill this scoping exists to avoid"
    );

    let wallets_after: Vec<i64> = users
        .iter()
        .map(|u| {
            storage::load_account(&mut ctx, *u)
                .unwrap()
                .perp_wallet_balance
        })
        .collect();
    assert_eq!(
        wallets_after, wallets_before,
        "expiry moves no money: a resting order escrows nothing, so there is nothing to refund"
    );
    assert_eq!(conservation_sum(&mut ctx, &users), value_before);
    for u in users {
        let pos = position(&mut ctx, u);
        assert_eq!(
            (pos.total_sell_qty, pos.total_sell_notional),
            (0, 0),
            "every expired sell left the Ask aggregate"
        );
        assert!(pos.total_buy_notional > 0, "the surviving bids still count");
    }

    let logs = JournalTr::take_logs(ctx.journal_mut());
    let snapshots = logs
        .iter()
        .filter(|l| {
            l.data.topics().first()
                == Some(&crate::interface::IPerpDex::AccountBalanceChanged::SIGNATURE_HASH)
        })
        .count();
    assert_eq!(
        snapshots, 0,
        "aggregates-only writes do not publish a snapshot — availableBalance rose for all \
         three users SILENTLY, which is the documented trigger rule, and is why the cancel \
         event now carries a reason"
    );
    let expiries = logs
        .iter()
        .filter(|l| l.data.topics().first() == Some(&crate::interface::IPerpDex::OrderCancelled::SIGNATURE_HASH))
        .count();
    assert_eq!(expiries, 12, "four levels x three owners");
}

/// The near-side `continue` in `match_order` must STAY. The GC is capped and therefore lags by
/// design, so between the mark move and the update that collects it the fill-time band is the
/// only thing standing between a taker and an off-mark price. Pinned here because the GC makes
/// the `continue` look redundant.
#[test]
fn the_fill_time_band_still_backstops_a_prefix_the_cap_has_not_reached() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    fund_big(&mut ctx, MAKER);
    fund_big(&mut ctx, ALICE);
    for _ in 0..MAX_BAND_EXPIRIES_PER_UPDATE + 1 {
        place_order(&mut ctx, MAKER, 1, 9_500, 1);
    }
    oracle_update(&mut ctx, 15_000, 31);
    // One order survived the cap, so the stranded level is still the touch.
    assert_eq!(
        storage::load_ask_count(&mut ctx, MARKET_ID, 9_500).unwrap(),
        1
    );
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), 9_500);

    // A crossing buy still cannot take it: 9_500 is below the new lower edge (13_500).
    assert_out_of_band_best(&mut ctx, Side::Sell, 9_500);
    // The taker's own limit sits INSIDE the new band, so nothing about the taker is out of band
    // and it rests legitimately — the previous $200 limit is now refused as a too-good best,
    // which would have made this a placement test rather than a fill-band one. $140 still
    // crosses the stranded $95 ask by a wide margin, which is all the walk needs.
    let taker_price = 14_000;
    let (upper, lower) = crate::math::mark_band_bounds(15_000, 0);
    assert!(
        (taker_price as u128) >= lower && (taker_price as u128) <= upper,
        "precondition: the taker itself is in band [{lower}, {upper}]"
    );
    assert!(
        taker_price >= 9_500,
        "precondition: and it crosses the stranded ask in PRICE, so only the band stops it"
    );
    place_order(&mut ctx, ALICE, 0, taker_price, 1);
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "the fill-time band is still the backstop while the GC lags"
    );
    assert_eq!(
        storage::load_ask_count(&mut ctx, MARKET_ID, 9_500).unwrap(),
        1,
        "and the survivor really was still there to be taken — the band, not an empty level, \
         is what stopped the fill"
    );
}
