//! Cross margin P2c — the gates and controls on an account with positions CROSSED in force.
//!
//! * the taker fills gate: a cross open's requirement sits inside the post-fill available balance,
//!   so it is checked even when its wallet draw (the fee) is zero — no B1 exemption for an open;
//! * J1: an opening taker fill of an account with cross exposure must leave it at or above cross
//!   maintenance (`E_post − draw ≥ Σ_C MM_post`) — refused at exactly one unit below, even where
//!   the available balance alone admits it; a pure reduce keeps the B1 exemption.
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
