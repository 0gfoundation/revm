//! Cross margin P2c — the gates and controls on an account with positions CROSSED in force.
//!
//! * the taker fills gate: a cross open's requirement sits inside the post-fill available balance,
//!   so it is checked even when its wallet draw (the fee) is zero — no B1 exemption for an open;
//! * J1: an opening taker fill of an account with cross exposure must leave it at or above cross
//!   maintenance (`E_post − draw ≥ Σ_C MM_post`) — refused at exactly one unit below, even where
//!   the available balance alone admits it; a pure reduce keeps the B1 exemption;
//! * makers (Q9 / D13): a cross maker fill gets no account-level check, however unhealthy the
//!   account — but every resting order of an account IN LIQUIDATION is cancelled, not filled;
//! * the pre-walk unaffordable-order filter credits a cross order's own open gain, so it never
//!   refuses what the full path admits, and still refuses what it can bound; the resting gate
//!   reads `A`, cross unrealized profit included, to the unit;
//! * Q11 / `-2023`: while the account is IN LIQUIDATION only reduce-only / closePosition orders
//!   are accepted, on every placement entrypoint, and `setMarginType` refuses (after `-4048`,
//!   before the gate) on both of its paths;
//! * `setLeverage` on a CROSSED position: raising always, lowering against `A` (`-2028`) to the
//!   unit; the tier cap (`-2027`) still applies; ISOLATED unchanged.
//!
//! A child of `cross_settlement_tests`, so it runs on that fixture: two markets on an exact grid
//! (base 0 decimals, price 2), `USD` = one dollar in quote units, the custody [`Ledger`] re-checked
//! around every step, and `rejects` asserting a refused call wrote nothing. Every test takes the
//! `#[cfg(test)]` override (`margin_type::test_gate`); counterparties meant to be ISOLATED are
//! switched explicitly.
use super::*;
use crate::types::{MarginTier, MarginTiers};

fn fold(ctx: &mut TestCtx, user: Address) -> crate::margin_view::CrossFold {
    crate::margin_view::cross_fold(ctx, user, None, None, None).unwrap()
}

/// `ROUND_UP(n / l)` — the engine's `PIM` on this grid.
fn ceil_div(n: i64, l: i64) -> i64 {
    (n + l - 1) / l
}

/// M2 re-tabled with two tiers: `[0, $2,000)` at 20x (derived MMR 2.5%) and from `$2,000` at 2x
/// (derived MMR 25%). A 20x position opened in tier 0 that the mark carries into tier 1 then has a
/// maintenance margin ABOVE its initial margin — the shape J1 exists for (the per-open tier guard
/// only ever looks at the market being filled).
fn two_tier_m2(ctx: &mut TestCtx) {
    let mut mk = storage::load_market(ctx, M2).unwrap().unwrap();
    mk.tiers = MarginTiers::from_tiers(&[
        MarginTier { lower_bound_notional: 0, max_leverage: 20, maint_margin_rate_ppm: 0 },
        MarginTier {
            lower_bound_notional: 2_000 * USD as u64,
            max_leverage: 2,
            maint_margin_rate_ppm: 0,
        },
    ])
    .unwrap();
    storage::save_market(ctx, &mk).unwrap();
}

// ── J1: the account-level maintenance check on an opening taker fill ─────────────────────────

/// ALICE: a CROSSED 20x short of 10 on M2, opened at $100 in tier 0 and carried by a $500 mark into
/// tier 1 — `|N| = $5,000`, `MM = $2,000/40 + $3,000/4 = $800` against `PIM = $250`, and
/// `uPnL = −$4,000`. BOB and CAROL are ISOLATED everywhere. ALICE's wallet is the caller's to set.
fn j1_scene(ctx: &mut TestCtx) {
    two_tier_m2(ctx);
    for m in MARKETS {
        set_type(ctx, BOB, m, ISOLATED);
        set_type(ctx, CAROL, m, ISOLATED);
    }
    set_leverage(ctx, ALICE, M2, 20);
    set_leverage(ctx, ALICE, M1, 3);
    trade(ctx, M2, ALICE, BOB, SELL, px(100), 10);
    set_mark(ctx, M2, px(500));
    let f = fold(ctx, ALICE);
    assert_eq!(
        (f.cross_upnl, f.cross_pim, f.cross_mm),
        ((-4_000 * USD) as i128, (250 * USD) as i128, (800 * USD) as i128),
        "the M2 leg sits in tier 1 with MM > PIM"
    );
}

/// The J1 boundary, to the unit. ALICE (J1 scene) opens a CROSSED long of 5 on M1 at the mark: it
/// adds `MM = $500/6` and draws the taker fee from the wallet. With the wallet one unit short of
/// `E_post − fee == Σ MM_post` the open is refused with J1's `-2019` — although her available
/// balance covers it by more than $400, because the M2 leg's `MM` exceeds its `PIM` by $550 — and
/// it writes nothing. One unit more and it goes through, leaving the account exactly AT
/// maintenance, which is healthy.
#[test]
fn j1_refuses_an_open_the_available_balance_admits_one_unit_below_maintenance() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    j1_scene(&mut ctx);
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 5, GTC));

    let open = value(px(100), 5);
    let mm_m1 = open / 6;
    let w_edge = 4_000 * USD + 800 * USD + mm_m1 + fee(open, TAKER_PPM);

    set_wallet(&mut ctx, ALICE, w_edge - 1);
    let ledger = Ledger::open(&mut ctx);
    // What the available-balance gate would see after the fill: the M1 leg's `PIM` comes off `A`,
    // its `uPnL` is 0 (filled at the mark) and it rests nothing.
    let a_post = fold(&mut ctx, ALICE).a - ceil_div(open, 3) as i128;
    assert!(
        a_post >= 400 * USD as i128,
        "the available balance alone admits the open by far: A_post = {a_post}"
    );
    rejects(
        &mut ctx,
        ALICE,
        &order(M1, BUY, px(100), 5, IOC),
        crate::trading::CROSS_MAINTENANCE_REJECT,
    );
    ledger.check(&mut ctx, "after the refusal");

    set_wallet(&mut ctx, ALICE, w_edge);
    let ledger = Ledger::open(&mut ctx);
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 5, IOC));
    ledger.check(&mut ctx, "after the open");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 5);
    let f = fold(&mut ctx, ALICE);
    assert_eq!(f.cross_mm, (800 * USD + mm_m1) as i128);
    assert_eq!(f.e, f.cross_mm, "exactly at cross maintenance");
    assert!(!f.below_cross_maintenance(), "E == Σ MM is healthy");
}

/// An ISOLATED open by the same account is held to J1 too: its silo cash leaves the cross wallet,
/// so it lowers `E` by exactly the silo (the fee comes out of the silo). Refused one unit below the
/// boundary — where the available balance and J6 (the silo's cash is in the wallet) both admit it —
/// and admitted at it.
#[test]
fn j1_refuses_an_isolated_open_that_drains_the_cross_wallet_below_maintenance() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    j1_scene(&mut ctx);
    set_type(&mut ctx, ALICE, M1, ISOLATED);
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 6, GTC));

    let silo = value(px(100), 6) / 3;
    let w_edge = 4_000 * USD + 800 * USD + silo;

    set_wallet(&mut ctx, ALICE, w_edge - 1);
    let ledger = Ledger::open(&mut ctx);
    let a = fold(&mut ctx, ALICE).a;
    assert!(a >= (silo + 400 * USD) as i128, "the available balance admits it: A = {a}");
    assert!(silo < w_edge - 1, "J6 admits it: the wallet holds the silo's cash");
    rejects(
        &mut ctx,
        ALICE,
        &order(M1, BUY, px(100), 6, IOC),
        crate::trading::CROSS_MAINTENANCE_REJECT,
    );
    ledger.check(&mut ctx, "after the refusal");

    set_wallet(&mut ctx, ALICE, w_edge);
    let ledger = Ledger::open(&mut ctx);
    let logs = ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 6, IOC));
    ledger.check(&mut ctx, "after the open");
    assert_eq!(position_changes(&logs, ALICE)[0].marginType, ISOLATED);
    assert_eq!(wallet(&mut ctx, ALICE), w_edge - silo, "the silo's cash left the wallet");
    let f = fold(&mut ctx, ALICE);
    assert_eq!(f.e, f.cross_mm, "exactly at cross maintenance");
}

/// A pure REDUCE keeps the B1 exemption: ALICE's account is already below cross maintenance
/// (`E = $500 < Σ MM = $800`), and buying back 2 of her short at $500 realizes $800 of loss and
/// leaves it still below (`E = $499.50 < $550`) — but it reduces, so J1 does not apply and the
/// available balance covers the fee.
#[test]
fn j1_leaves_a_pure_reduce_to_the_available_balance_gate() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    j1_scene(&mut ctx);
    set_wallet(&mut ctx, ALICE, 4_500 * USD);
    assert!(fold(&mut ctx, ALICE).below_cross_maintenance());
    ok(&mut ctx, CAROL, &order(M2, SELL, px(500), 2, GTC));
    let ledger = Ledger::open(&mut ctx);

    ok(&mut ctx, ALICE, &order(M2, BUY, px(500), 2, IOC));
    ledger.check(&mut ctx, "after the reduce");
    assert_eq!(pos(&mut ctx, ALICE, M2).amount, -8);
    let realized = -(value(px(500), 2) - value(px(100), 2));
    assert_eq!(wallet(&mut ctx, ALICE), 4_500 * USD + realized - fee(value(px(500), 2), TAKER_PPM));
    let f = fold(&mut ctx, ALICE);
    assert_eq!(f.cross_mm, (550 * USD) as i128);
    assert!(f.below_cross_maintenance(), "still below — J1 would have refused an open here");

    // The batch status table files the J1 refusal with its isolated twin (K9).
    assert_eq!(
        crate::batch::reason_code(&crate::errors::perp_err(
            crate::trading::CROSS_MAINTENANCE_REJECT
        )),
        crate::batch::PerpBatchReason::OpenIntoInsolvency
    );
}

// ── The fills gate on a cross open ───────────────────────────────────────────────────────────

/// Places an order that must be accepted and returns its id (`placeOrder` answers with it).
fn place(ctx: &mut TestCtx, user: Address, input: &[u8]) -> [u8; 32] {
    let out = call(ctx, user, input);
    assert!(!out.reverted, "call reverted: {}", revert_reason(&out));
    let _ = JournalTr::take_logs(ctx.journal_mut());
    out.bytes[..32].try_into().unwrap()
}

/// `OrderCancelled(reason)` rows for `user`, as `(orderId, reason)`.
fn cancels(logs: &[Log], user: Address) -> Vec<([u8; 32], u8)> {
    use crate::interface::IPerpDex::OrderCancelled;
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
        .map(|l| OrderCancelled::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .filter(|e| e.user == user)
        .map(|e| (e.orderId.0, e.reason))
        .collect()
}

/// A cross open's wallet draw is its fee alone; what it REQUIRES (its `PIM`, net of its own fill's
/// `uPnL`) is inside the post-fill available balance. So a ZERO-fee account's cross open is still
/// held to `A_post ≥ 0` — B1's "a non-positive requirement is always affordable" would otherwise
/// wave through an open of any size. ALICE (taker fee 0, 3x) buys 3 at the mark: `PIM = $100`.
#[test]
fn a_zero_fee_cross_open_is_still_held_to_its_initial_margin() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 3);
    set_taker_fee(&mut ctx, ALICE, 0);
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 10, GTC));

    set_wallet(&mut ctx, ALICE, 100 * USD - 1);
    let ledger = Ledger::open(&mut ctx);
    rejects(
        &mut ctx,
        ALICE,
        &order(M1, BUY, px(100), 3, IOC),
        "placeOrder: insufficient perp wallet for margin",
    );
    ledger.check(&mut ctx, "after the refusal");

    set_wallet(&mut ctx, ALICE, 100 * USD);
    let ledger = Ledger::open(&mut ctx);
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 3, IOC));
    ledger.check(&mut ctx, "after the open");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 3);
    assert_eq!(wallet(&mut ctx, ALICE), 100 * USD, "no fee, no silo: nothing left the wallet");
    assert_eq!(fold(&mut ctx, ALICE).a, 0, "exactly at the initial-margin boundary");
}

/// The cover loop serves a zero-fee cross open too: ALICE's resting buy holds `$83.33` of `ooIM`,
/// without which the open fits. The compute phase cancels it in simulation and the apply phase
/// cancels it for real — both on the same no-exemption predicate (with B1's shortcut the apply
/// phase would skip the cancel the compute phase decided on, and leave the account short).
#[test]
fn a_zero_fee_cross_open_cancels_its_own_same_side_orders_to_fit() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 3);
    set_taker_fee(&mut ctx, ALICE, 0);
    set_wallet(&mut ctx, ALICE, 150 * USD);
    let resting_id = place(&mut ctx, ALICE, &order(M1, BUY, px(50), 5, GTC));
    ok(&mut ctx, BOB, &order(M1, SELL, px(100), 3, GTC));
    // Post-fill with the order still resting: `IM = ⌈max($300 + $250, $300) / 3⌉ = $183.33` > W.
    let ledger = Ledger::open(&mut ctx);

    let logs = ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 3, IOC));
    ledger.check(&mut ctx, "after the covered open");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 3);
    assert_eq!(
        cancels(&logs, ALICE),
        vec![(resting_id, crate::types::CancelReason::TakerMarginCover as u8)],
        "the resting buy was cancelled to make room"
    );
    assert_eq!(fold(&mut ctx, ALICE).a, 50 * USD as i128, "W − PIM = $150 − $100");
}

// ── Makers ────────────────────────────────────────────────────────────────────────────────────

fn trades_with(logs: &[Log], maker: Address) -> usize {
    use crate::interface::IPerpDex::Trade;
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&Trade::SIGNATURE_HASH))
        .map(|l| Trade::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .filter(|t| t.maker == maker)
        .count()
}

/// Q9 / D13: a resting cross order fills whatever the health of its account. ALICE's account is
/// below cross maintenance (`E = $199 < Σ MM = $200`) with a NEGATIVE available balance — no new
/// order of hers would be admitted — and her resting cross buy, admitted while she was healthy,
/// still fills in full when CAROL sells into it.
#[test]
fn a_cross_maker_fill_is_not_refused_on_account_health() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
        set_type(&mut ctx, CAROL, m, ISOLATED);
        set_leverage(&mut ctx, ALICE, m, 3);
    }
    set_wallet(&mut ctx, ALICE, 1_000 * USD);
    trade(&mut ctx, M2, ALICE, BOB, BUY, px(100), 20);
    let id = place(&mut ctx, ALICE, &order(M1, BUY, px(100), 3, GTC));
    set_mark(&mut ctx, M2, px(60));
    let f = fold(&mut ctx, ALICE);
    assert!(f.below_cross_maintenance() && f.a < 0, "underwater and over-committed: {f:?}");
    let ledger = Ledger::open(&mut ctx);

    let logs = ok(&mut ctx, CAROL, &order(M1, SELL, px(100), 3, IOC));
    ledger.check(&mut ctx, "after the maker fill");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 3, "filled, not cancelled");
    assert_eq!(trades_with(&logs, ALICE), 1);
    assert!(cancels(&logs, ALICE).is_empty(), "order {id:?} was not cancelled");
    assert!(fold(&mut ctx, ALICE).below_cross_maintenance());
}

/// An account IN LIQUIDATION (`liq_flags`, set by P3 — here by the test stand-in) has every resting
/// order removed when the walk reaches it instead of filled: a cross buy, an isolated sell, and a
/// reduce-only order alike, each with `OrderCancelled(reason = Liquidation)`. The taker walks on to
/// the next maker and fills there; the flagged account's positions do not move. Cleared, the
/// account's next resting order fills again.
#[test]
fn a_maker_of_an_account_in_liquidation_is_cancelled_not_filled() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
        set_type(&mut ctx, CAROL, m, ISOLATED);
    }
    set_type(&mut ctx, ALICE, M2, ISOLATED);
    // A cross long of 5 on M1 for the reduce-only sell to work against.
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 5);
    let cross_buy = place(&mut ctx, ALICE, &order(M1, BUY, px(100), 3, GTC));
    ok(&mut ctx, BOB, &order(M1, BUY, px(99), 3, GTC));
    let iso_sell = place(&mut ctx, ALICE, &order(M2, SELL, px(100), 2, GTC));
    let reduce_only = place(
        &mut ctx,
        ALICE,
        &placeOrderCall {
            marketId: M1,
            side: SELL,
            price: px(101),
            quantity: 2,
            orderType: 0,
            tif: GTC,
            clientOrderId: FixedBytes::default(),
            flags: 1,
        }
        .abi_encode(),
    );
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, true).unwrap();
    // Position state and wallet (the order aggregates do move: the cancels shrink `Bid`/`Ask`).
    let alice = |ctx: &mut TestCtx| {
        let (p1, p2) = (pos(ctx, ALICE, M1), pos(ctx, ALICE, M2));
        let state = |p: crate::types::PerpPosition| (p.amount, p.v_quote_balance, p.margin);
        (state(p1), state(p2), wallet(ctx, ALICE))
    };
    let before = alice(&mut ctx);
    let ledger = Ledger::open(&mut ctx);
    let liquidation = crate::types::CancelReason::Liquidation as u8;

    // CAROL sells 3 down to $99: ALICE's $100 cross buy is first in line and is cancelled; BOB's
    // $99 bid fills.
    let logs = ok(&mut ctx, CAROL, &order(M1, SELL, px(99), 3, IOC));
    ledger.check(&mut ctx, "after the cross maker was skipped");
    assert_eq!(cancels(&logs, ALICE), vec![(cross_buy, liquidation)]);
    assert_eq!((trades_with(&logs, ALICE), trades_with(&logs, BOB)), (0, 1));
    assert_eq!(pos(&mut ctx, CAROL, M1).amount, -3);
    // An isolated order and a reduce-only one are frozen alike: the flag is the ACCOUNT's.
    let logs = ok(&mut ctx, CAROL, &order(M2, BUY, px(100), 2, IOC));
    assert_eq!(cancels(&logs, ALICE), vec![(iso_sell, liquidation)]);
    assert_eq!(pos(&mut ctx, CAROL, M2).amount, 0, "nothing else on the book: it expired");
    ok(&mut ctx, BOB, &order(M1, SELL, px(101), 3, GTC));
    let logs = ok(&mut ctx, CAROL, &order(M1, BUY, px(101), 2, IOC));
    assert_eq!(cancels(&logs, ALICE), vec![(reduce_only, liquidation)]);
    assert_eq!(trades_with(&logs, BOB), 1, "the walk went on to BOB's ask");
    ledger.check(&mut ctx, "after the isolated and reduce-only makers were skipped");
    assert_eq!(alice(&mut ctx), before, "ALICE's positions and wallet did not move");

    // Cleared: her next resting order fills as usual.
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, false).unwrap();
    ok(&mut ctx, ALICE, &order(M1, BUY, px(98), 1, GTC));
    let logs = ok(&mut ctx, CAROL, &order(M1, SELL, px(98), 1, IOC));
    assert_eq!(trades_with(&logs, ALICE), 1);
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 6);
    ledger.check(&mut ctx, "after the flag was cleared");
}

// ── The pre-walk filter and the resting gate ─────────────────────────────────────────────────

/// A cross order whose fill lands on the good side of the mark has its open GAIN in the available
/// balance (`uPnL` at the mark), so it needs less than the isolated bound the pre-walk filter used
/// to apply. ALICE (cross, 3x, `A = $250`) buys 10 at $90 with the mark at $100: the isolated bound
/// is `⌈$900 / 3⌉ = $300 > A` and the old filter refused it outright, yet the order needs only
/// `PIM $333.33 − gain $100 + fee $0.45` and goes through. The mirror, a sell of 10 at $110 into a
/// $110 bid (bound `⌈$1,000 / 3⌉`, the sell valued at `min(limit, mark)`), likewise.
#[test]
fn the_pre_walk_filter_credits_a_cross_orders_own_open_gain() {
    for side in [BUY, SELL] {
        let mut ctx = make_ctx();
        let _cross = crate::margin_type::test_gate::enable();
        set_type(&mut ctx, BOB, M1, ISOLATED);
        set_leverage(&mut ctx, ALICE, M1, 3);
        set_wallet(&mut ctx, ALICE, 250 * USD);
        // The counter-order on the good side of the $100 mark.
        let price = if side == BUY { px(90) } else { px(110) };
        ok(&mut ctx, BOB, &order(M1, 1 - side, price, 10, GTC));
        let ledger = Ledger::open(&mut ctx);

        // What the filter used to compare `A` against: the order resting whole at
        // `p_min = min(limit, best ask for a buy, mark)`.
        let market = storage::load_market(&mut ctx, M1).unwrap().unwrap();
        let flat = pos(&mut ctx, ALICE, M1);
        let mut resting = flat.clone();
        let p_min = price.min(px(100));
        if side == BUY {
            resting.total_buy_notional = value(p_min, 10) as u64;
        } else {
            resting.total_sell_notional = value(p_min, 10) as u64;
        }
        let isolated_bound =
            crate::margin_view::derived_requirement_delta(&market, &flat, &resting).unwrap();
        assert_eq!(isolated_bound, ceil_div(value(p_min, 10), 3) as i128);
        assert!(fold(&mut ctx, ALICE).a < isolated_bound, "the old filter refused it");

        ok(&mut ctx, ALICE, &order(M1, side, price, 10, GTC));
        ledger.check(&mut ctx, "after the order");
        let amount = pos(&mut ctx, ALICE, M1).amount;
        assert_eq!(amount, if side == BUY { 10 } else { -10 }, "filled in full, nothing rested");
        let want = 250 * USD - ceil_div(value(px(100), 10), 3) + 100 * USD
            - fee(value(price, 10), TAKER_PPM);
        assert_eq!(fold(&mut ctx, ALICE).a, want as i128, "A = W − PIM + gain − fee");
    }
}

/// The credit is bounded, so the filter keeps its teeth on a cross order: with `A = $199` against a
/// true requirement of `$233.78` (the buy above), the filter itself refuses it before any walk —
/// called directly here, since the full path's own refusal reads the same.
#[test]
fn the_pre_walk_filter_still_refuses_a_cross_order_it_can_bound() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 3);
    ok(&mut ctx, BOB, &order(M1, SELL, px(90), 10, GTC));
    let market = storage::load_market(&mut ctx, M1).unwrap().unwrap();
    let filter = |ctx: &mut TestCtx| {
        crate::trading::reject_provably_unaffordable_limit_order(
            ctx,
            ALICE,
            M1,
            crate::types::Side::Buy,
            px(90),
            10,
            &market,
        )
    };

    set_wallet(&mut ctx, ALICE, 199 * USD);
    match filter(&mut ctx) {
        Err(crate::PerpError::Reject(m)) => {
            assert_eq!(m, "placeOrder: insufficient perp wallet for margin")
        }
        other => panic!("the filter let a provably unaffordable cross order through: {other:?}"),
    }
    rejects(
        &mut ctx,
        ALICE,
        &order(M1, BUY, px(90), 10, GTC),
        "placeOrder: insufficient perp wallet for margin",
    );
    // At $234 the full path admits it, and the filter, sound, lets it through.
    set_wallet(&mut ctx, ALICE, 234 * USD);
    assert!(filter(&mut ctx).is_ok());
    ok(&mut ctx, ALICE, &order(M1, BUY, px(90), 10, GTC));
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 10);
}

/// The resting gate (`rest_in_book`) reads `A`, cross unrealized profit included: ALICE's cross
/// long on M2 has doubled (`uPnL = +$1,000`, `PIM = $666.67`), which backs a resting buy on M1
/// (`ooIM = $100`) even on a NEGATIVE wallet. Refused one unit below `A_after = 0`, rested at it.
#[test]
fn the_resting_gate_reads_a_with_cross_profit_to_the_unit() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
        set_leverage(&mut ctx, ALICE, m, 3);
    }
    trade(&mut ctx, M2, ALICE, BOB, BUY, px(100), 10);
    set_mark(&mut ctx, M2, px(200));
    let oo_im = value(px(100), 3) / 3;
    let w_edge = ceil_div(value(px(200), 10), 3) + oo_im - 1_000 * USD;
    assert!(w_edge < 0, "the edge is a negative wallet: {w_edge}");

    set_wallet(&mut ctx, ALICE, w_edge - 1);
    rejects(
        &mut ctx,
        ALICE,
        &order(M1, BUY, px(100), 3, GTC),
        "placeOrder: insufficient perp wallet for margin",
    );
    set_wallet(&mut ctx, ALICE, w_edge);
    let ledger = Ledger::open(&mut ctx);
    ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 3, GTC));
    ledger.check(&mut ctx, "after the rest");
    assert_eq!(pos(&mut ctx, ALICE, M1).total_buy_qty, 3, "it rests");
    assert_eq!(fold(&mut ctx, ALICE).a, 0);
}

// ── Q11 / -2023: an account in liquidation ────────────────────────────────────────────────────

use crate::interface::IPerpDex::{
    batchPlaceOrdersCall, batchPlaceOrdersSignedCall, cancelOrderCall, placeAlgoOrderCall,
    placeOrderSignedCall, setMarginTypeSignedCall, PlaceItem,
};
use ed25519_dalek::{Signer, SigningKey};

const KEY_ID: u8 = 1;
/// The default test block's timestamp, so a signed message is fresh.
const SIGNED_TS: u64 = 1;
const SIGNED_RECV: u64 = 60;
const RO: u8 = 1;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[0x2c; 32])
}

fn register_alice_key(ctx: &mut TestCtx) {
    let pubkey = key().verifying_key().to_bytes();
    storage::save_api_key(ctx, ALICE, KEY_ID, crate::types::ApiKey { pubkey, expiry: 0 }).unwrap();
}

fn chain_id(ctx: &TestCtx) -> u64 {
    crate::host::PerpHost::chain_id(ctx)
}

fn item(market: u64, side: u8, price: u64, qty: u64, tif: u8, flags: u8) -> PlaceItem {
    PlaceItem {
        marketId: market,
        side,
        price,
        quantity: qty,
        orderType: 0,
        tif,
        clientOrderId: FixedBytes::default(),
        flags,
    }
}

fn ro_order(market: u64, side: u8, price: u64, qty: u64, tif: u8) -> Vec<u8> {
    let i = item(market, side, price, qty, tif, RO);
    placeOrderCall {
        marketId: i.marketId,
        side: i.side,
        price: i.price,
        quantity: i.quantity,
        orderType: i.orderType,
        tif: i.tif,
        clientOrderId: i.clientOrderId,
        flags: i.flags,
    }
    .abi_encode()
}

/// `placeOrderSigned` for ALICE (the 105-byte message).
fn signed_order(ctx: &TestCtx, it: &PlaceItem, salt: u8) -> Vec<u8> {
    let mut client = [0u8; 16];
    client[15] = salt;
    let mut msg = Vec::new();
    msg.extend_from_slice(b"perpdex_v1_order");
    msg.extend_from_slice(&chain_id(ctx).to_be_bytes());
    msg.extend_from_slice(ALICE.as_slice());
    msg.extend_from_slice(&it.marketId.to_be_bytes());
    msg.push(it.side);
    msg.extend_from_slice(&it.price.to_be_bytes());
    msg.extend_from_slice(&it.quantity.to_be_bytes());
    msg.extend_from_slice(&[it.orderType, it.tif]);
    msg.extend_from_slice(&client);
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.extend_from_slice(&[KEY_ID, it.flags]);
    assert_eq!(msg.len(), 105);
    placeOrderSignedCall {
        account: ALICE,
        marketId: it.marketId,
        side: it.side,
        price: it.price,
        quantity: it.quantity,
        orderType: it.orderType,
        tif: it.tif,
        clientOrderId: client.into(),
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: KEY_ID,
        flags: it.flags,
        signature: key().sign(&msg).to_bytes().to_vec().into(),
    }
    .abi_encode()
}

/// `batchPlaceOrdersSigned` for ALICE.
fn signed_batch(ctx: &TestCtx, items: Vec<PlaceItem>) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(b"perpdex_v1_batch_order");
    msg.extend_from_slice(&chain_id(ctx).to_be_bytes());
    msg.extend_from_slice(ALICE.as_slice());
    msg.push(KEY_ID);
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.extend_from_slice(&(items.len() as u32).to_be_bytes());
    for o in &items {
        msg.extend_from_slice(&o.marketId.to_be_bytes());
        msg.push(o.side);
        msg.extend_from_slice(&o.price.to_be_bytes());
        msg.extend_from_slice(&o.quantity.to_be_bytes());
        msg.push(o.orderType);
        msg.push(o.tif);
        msg.extend_from_slice(&o.clientOrderId.0);
        msg.push(o.flags);
    }
    batchPlaceOrdersSignedCall {
        account: ALICE,
        keyId: KEY_ID,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        orders: items,
        signature: key().sign(&msg).to_bytes().to_vec().into(),
    }
    .abi_encode()
}

/// `setMarginTypeSigned` for ALICE (the 75-byte message).
fn signed_margin_type(ctx: &TestCtx, market: u64, margin_type: u8) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(b"perpdex_v1_margintype");
    msg.extend_from_slice(&chain_id(ctx).to_be_bytes());
    msg.extend_from_slice(ALICE.as_slice());
    msg.extend_from_slice(&market.to_be_bytes());
    msg.push(margin_type);
    msg.extend_from_slice(&SIGNED_TS.to_be_bytes());
    msg.extend_from_slice(&SIGNED_RECV.to_be_bytes());
    msg.push(KEY_ID);
    assert_eq!(msg.len(), 75);
    setMarginTypeSignedCall {
        account: ALICE,
        marketId: market,
        marginType: margin_type,
        timestamp: SIGNED_TS,
        recvWindow: SIGNED_RECV,
        keyId: KEY_ID,
        signature: key().sign(&msg).to_bytes().to_vec().into(),
    }
    .abi_encode()
}

/// `(tag, reason)` per record of a batch status blob.
fn statuses(out: &PerpOutput) -> Vec<(u8, u8)> {
    let blob = batchPlaceOrdersCall::abi_decode_returns(&out.bytes).unwrap();
    blob.chunks(crate::batch::BATCH_STATUS_RECORD_LEN).map(|r| (r[0], r[33])).collect()
}

/// A STOP_MARKET (`1`) / TAKE_PROFIT_MARKET (`3`) / TRAILING_STOP_MARKET (`4`) conditional on M1.
fn algo(side: u8, order_type: u8, trigger: u64, price: u64, qty: u64, flags: u8) -> Vec<u8> {
    placeAlgoOrderCall {
        algoType: 0,
        marketId: M1,
        side,
        orderType: order_type,
        triggerPrice: trigger,
        price,
        quantity: qty,
        tif: 0,
        workingType: 0,
        clientAlgoId: FixedBytes::default(),
        flags,
    }
    .abi_encode()
}

/// Q11 across every placement entrypoint. ALICE holds a cross long of 5 on M1 and her account is
/// flagged IN LIQUIDATION: an order that is not reduce-only is refused `-2023` — `placeOrder`
/// (write-clean), `placeOrderSigned` (the signature is burned, nothing else moves), both batch
/// selectors (that item `Rejected`, reason 35, the batch goes on) — while a reduce-only order is
/// accepted on each of them and closes, a reduce-only and a closePosition conditional and a
/// trailing stop are accepted, and a cancel goes through. Cleared, a plain order is accepted again.
#[test]
fn in_liquidation_only_reducing_orders_are_accepted_on_every_entrypoint() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 5);
    ok(&mut ctx, BOB, &order(M1, BUY, px(100), 5, GTC)); // bids for her reducing sells
    register_alice_key(&mut ctx);
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, true).unwrap();
    let reject = crate::trading::IN_LIQUIDATION_REJECT;
    let alice = |ctx: &mut TestCtx| (pos(ctx, ALICE, M1).amount, wallet(ctx, ALICE));

    // placeOrder: refused write-clean, a buy and a sell alike; reduce-only goes through.
    rejects(&mut ctx, ALICE, &order(M1, BUY, px(90), 1, GTC), reject);
    rejects(&mut ctx, ALICE, &order(M1, SELL, px(100), 1, IOC), reject);
    ok(&mut ctx, ALICE, &ro_order(M1, SELL, px(100), 1, IOC));
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 4);

    // placeOrderSigned: refused after the burn, with the same string; reduce-only goes through.
    let before = alice(&mut ctx);
    let input = signed_order(&ctx, &item(M1, BUY, px(90), 1, GTC, 0), 1);
    let out = call(&mut ctx, ALICE, &input);
    assert!(out.reverted);
    assert_eq!(revert_reason(&out), reject);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(alice(&mut ctx), before);
    let input = signed_order(&ctx, &item(M1, SELL, px(100), 1, IOC, RO), 2);
    ok(&mut ctx, ALICE, &input);
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 3);

    // Both batch selectors: the plain item is Rejected (reason 35), the reduce-only one placed.
    let plain = item(M1, BUY, px(90), 1, GTC, 0);
    let reducing = item(M1, SELL, px(100), 1, IOC, RO);
    let out = call(
        &mut ctx,
        ALICE,
        &batchPlaceOrdersCall { orders: vec![plain.clone(), reducing.clone()] }.abi_encode(),
    );
    assert!(!out.reverted, "{}", revert_reason(&out));
    let in_liq = crate::batch::PerpBatchReason::UserInLiquidation as u8;
    assert_eq!(statuses(&out), vec![(0, in_liq), (2, 0)]);
    let input = signed_batch(&ctx, vec![plain, reducing]);
    let out = call(&mut ctx, ALICE, &input);
    assert!(!out.reverted, "{}", revert_reason(&out));
    assert_eq!(statuses(&out), vec![(0, in_liq), (2, 0)]);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 1);

    // Conditionals are reducing by construction: a reduce-only stop, a closePosition take-profit
    // and a trailing stop are all accepted.
    ok(&mut ctx, ALICE, &algo(SELL, 1, px(80), 0, 1, RO));
    ok(&mut ctx, ALICE, &algo(SELL, 3, px(120), 0, 0, 2));
    ok(&mut ctx, ALICE, &algo(SELL, 4, 0, 100, 1, RO));

    // A cancel is never refused.
    let resting = place(&mut ctx, ALICE, &ro_order(M1, SELL, px(130), 1, GTC));
    ok(&mut ctx, ALICE, &cancelOrderCall { orderId: resting.into(), marketId: M1 }.abi_encode());

    // Cleared: a plain order is accepted again.
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, false).unwrap();
    ok(&mut ctx, ALICE, &order(M1, BUY, px(90), 1, GTC));

    assert_eq!(
        crate::batch::reason_code(&crate::errors::perp_err(reject)),
        crate::batch::PerpBatchReason::UserInLiquidation
    );
}

/// `setMarginType`'s `-2023`, at its place in Binance's order (§3.6): after `-4046` / `-4047` /
/// `-4048`, before the gate — on both paths, the signed one after its burn. Gated OFF a CROSSED
/// request reaches it too (the gate is the LAST refusal), and once the flag clears the same switch
/// goes through.
#[test]
fn set_margin_type_refuses_in_liquidation_after_4048_and_before_the_gate() {
    let mut ctx = make_ctx();
    register_alice_key(&mut ctx);
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 1); // a position on M1; M2 flat and order-free
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, true).unwrap();
    let set = |market, margin_type| setMarginTypeCall { marketId: market, marginType: margin_type };

    // Earlier refusals win.
    rejects(
        &mut ctx,
        ALICE,
        &set(M2, CROSSED).abi_encode(),
        "setMarginType: -4046 No need to change margin type.",
    );
    rejects(
        &mut ctx,
        ALICE,
        &set(M1, ISOLATED).abi_encode(),
        "setMarginType: -4048 Margin type cannot be changed if there exists position.",
    );
    // Then -2023, direct and signed.
    let in_liq = "setMarginType: -2023 User in liquidation mode now.";
    rejects(&mut ctx, ALICE, &set(M2, ISOLATED).abi_encode(), in_liq);
    let input = signed_margin_type(&ctx, M2, ISOLATED);
    let out = call(&mut ctx, ALICE, &input);
    assert!(out.reverted);
    assert_eq!(revert_reason(&out), in_liq);
    let _ = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(pos(&mut ctx, ALICE, M2).margin_type, CROSSED, "nothing switched");

    // Before the gate: with cross margin gated off, CROSSED (not in force, so not -4046) hits
    // -2023 before "not enabled".
    drop(_cross);
    rejects(&mut ctx, ALICE, &set(M2, CROSSED).abi_encode(), in_liq);
    storage::set_in_liquidation_for_test(&mut ctx, ALICE, false).unwrap();
    rejects(
        &mut ctx,
        ALICE,
        &set(M2, CROSSED).abi_encode(),
        "setMarginType: CROSSED margin is not enabled yet",
    );

    // Cleared, with the gate on: the switch goes through.
    let _cross = crate::margin_type::test_gate::enable();
    ok(&mut ctx, ALICE, &set(M2, ISOLATED).abi_encode());
    assert_eq!(pos(&mut ctx, ALICE, M2).margin_type, ISOLATED);
}

// ── setLeverage on a CROSSED position ────────────────────────────────────────────────────────

/// A CROSSED position's leverage moves both ways under a live position. ALICE holds a cross long
/// of 10 at $100, opened at 2x (`PIM = $500`) on a $100 wallet (`A = −$400`): RAISING to 3x is
/// admitted even so (the requirement only falls, `PIM = $333.33`). LOWERING back to 2x adds
/// `$166.67` of `PIM`, so it needs `A` to cover exactly that: refused `-2028` with the wallet one
/// unit short, admitted at the boundary (`A_after = 0`). It writes the leverage only, like every
/// leverage change.
#[test]
fn cross_set_leverage_raises_freely_and_lowers_against_a_to_the_unit() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 2);
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    set_wallet(&mut ctx, ALICE, 100 * USD);
    assert!(fold(&mut ctx, ALICE).a < 0);

    let logs = ok(&mut ctx, ALICE, &setLeverageCall { marketId: M1, leverage: 3 }.abi_encode());
    assert_eq!(pos(&mut ctx, ALICE, M1).leverage, 3, "raised at a negative available balance");
    assert_eq!(count_events::<AccountBalanceChanged>(&logs), 0, "a leverage-only write");

    let n = value(px(100), 10);
    let delta = n / 2 - ceil_div(n, 3);
    let w_edge = n / 2; // A(3x) = W − ⌈N/3⌉ must equal Δ = N/2 − ⌈N/3⌉
    set_wallet(&mut ctx, ALICE, w_edge - 1);
    assert_eq!(fold(&mut ctx, ALICE).a, (delta - 1) as i128);
    let lower = setLeverageCall { marketId: M1, leverage: 2 }.abi_encode();
    rejects(&mut ctx, ALICE, &lower, crate::risk::CROSS_LEVERAGE_REJECT);
    set_wallet(&mut ctx, ALICE, w_edge);
    ok(&mut ctx, ALICE, &lower);
    assert_eq!(pos(&mut ctx, ALICE, M1).leverage, 2);
    assert_eq!(fold(&mut ctx, ALICE).a, 0, "exactly covered");
}

/// The tier cap still binds a CROSSED position, raising included: with the table's `notionalCap`
/// installed below the position's notional, no leverage is admitted (`-2027`); and an ISOLATED
/// position keeps its rule — no lowering under an open position, at any balance.
#[test]
fn cross_set_leverage_keeps_the_tier_cap_and_isolated_keeps_its_rule() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
    }
    set_type(&mut ctx, ALICE, M2, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 2);
    set_leverage(&mut ctx, ALICE, M2, 3);
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    trade(&mut ctx, M2, ALICE, BOB, BUY, px(100), 10);

    rejects(
        &mut ctx,
        ALICE,
        &setLeverageCall { marketId: M2, leverage: 2 }.abi_encode(),
        "setLeverage: cannot reduce leverage with open position",
    );

    let mut mk = storage::load_market(&mut ctx, M1).unwrap().unwrap();
    mk.tiers = MarginTiers::default().with_notional_cap(500 * USD as u64);
    storage::save_market(&mut ctx, &mk).unwrap();
    rejects(
        &mut ctx,
        ALICE,
        &setLeverageCall { marketId: M1, leverage: 3 }.abi_encode(),
        "setLeverage: -2027 position exceeds the market's maximum notional",
    );
}
