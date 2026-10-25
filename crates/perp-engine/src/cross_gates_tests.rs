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
//!   unit; the tier cap (`-2027`) still applies; ISOLATED unchanged;
//! * add/removePositionMargin: a CROSSED market `-4049`; an isolated add capped by `X` (`-4050`) to
//!   the unit, cross unrealized profit excluded; an isolated remove unchanged;
//! * every money-out gate stops at `X` to the unit with cross profit standing behind it, and money
//!   in nets against a negative cross wallet;
//! * gated off, every P2c gate is byte-identical to an explicitly ISOLATED account.
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

/// J1 does not hold a pure REDUCE: ALICE's account is already below cross maintenance
/// (`E = $500 < Σ MM = $800`), and buying back 2 of her short at $500 realizes $800 of loss and
/// leaves it still below (`E = $499.50 < $550`) — but it reduces, so J1 does not apply (nor does
/// the available-balance gate: a pure reduce on a cross wallet is not margin-gated, see the
/// reduce tests below).
#[test]
fn j1_does_not_hold_a_pure_reduce() {
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

// ── A pure reduce on a cross wallet is not margin-gated ──────────────────────────────────────

/// `AlgoOrderCancelled(reason)` rows for `user`.
fn algo_cancels(logs: &[Log], user: Address) -> Vec<u8> {
    use crate::interface::IPerpDex::AlgoOrderCancelled;
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&AlgoOrderCancelled::SIGNATURE_HASH))
        .map(|l| AlgoOrderCancelled::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .filter(|e| e.user == user)
        .map(|e| e.reason)
        .collect()
}

/// ALICE: a CROSSED 2x long of 90 on M1, bought at $100 from an ISOLATED BOB on a $4,600 wallet
/// (fee $4.50). At a $80 mark her account is HEALTHY — `E = $2,795.50` against `Σ MM = $1,200` —
/// but under INITIAL margin: `PIM = $3,600` puts her available balance at `−$804.50`. CAROL
/// (ISOLATED) is the counterparty for her reduces.
fn healthy_under_im_scene(ctx: &mut TestCtx) {
    for m in MARKETS {
        set_type(ctx, BOB, m, ISOLATED);
        set_type(ctx, CAROL, m, ISOLATED);
    }
    set_leverage(ctx, ALICE, M1, 2);
    set_wallet(ctx, ALICE, 4_600 * USD);
    trade(ctx, M1, ALICE, BOB, BUY, px(100), 90);
    assert_eq!(wallet(ctx, ALICE), 4_595 * USD + USD / 2);
}

/// The healthy-but-under-IM account of [`healthy_under_im_scene`] at the $80 mark.
fn assert_healthy_under_im(ctx: &mut TestCtx) {
    let f = fold(ctx, ALICE);
    assert_eq!(
        (f.e, f.cross_mm, f.a),
        ((2_795 * USD + USD / 2) as i128, (1_200 * USD) as i128, -(804 * USD + USD / 2) as i128),
        "healthy (E ≥ Σ MM), under initial margin (A < 0)"
    );
}

/// A pure reduce of a CROSSED position is not margin-gated: its draw is the fee, and on a cross
/// wallet nothing comes back to cover it — `A_post` still charges the remaining legs' initial
/// margin — so holding it to `A_post ≥ fee` refused healthy accounts the very trade that lowers
/// their risk. ALICE (healthy, `A = −$804.50`) sells 18 of her 90 reduce-only at the mark: it fills,
/// the fee and the realized loss land in the wallet, and her available balance is STILL negative
/// afterwards (`−$85.22` — the old gate's refusal, by `$85.94`). Binance gates no reduce-only order.
///
/// The same holds for closing a WHOLE losing leg whose loss the other legs' initial margin outweighs
/// — see [`a_cross_wallet_closes_a_losing_leg_whatever_the_other_legs_initial_margin`].
#[test]
fn a_cross_reduce_is_not_margin_gated_on_a_healthy_account_under_initial_margin() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    healthy_under_im_scene(&mut ctx);
    set_mark(&mut ctx, M1, px(80));
    assert_healthy_under_im(&mut ctx);
    ok(&mut ctx, CAROL, &order(M1, BUY, px(80), 18, GTC));
    let ledger = Ledger::open(&mut ctx);

    let logs = ok(&mut ctx, ALICE, &ro_order(M1, SELL, px(80), 18, IOC));
    ledger.check(&mut ctx, "after the reduce");
    assert_eq!(trades_with(&logs, CAROL), 1);
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 72);
    assert!(cancels(&logs, ALICE).is_empty(), "nothing was cancelled to cover it");
    let realized = value(px(80), 18) - value(px(100), 18);
    let reduce_fee = fee(value(px(80), 18), TAKER_PPM);
    assert_eq!(wallet(&mut ctx, ALICE), 4_595 * USD + USD / 2 + realized - reduce_fee);
    let f = fold(&mut ctx, ALICE);
    assert!(!f.below_cross_maintenance());
    assert_eq!(
        f.a,
        (wallet(&mut ctx, ALICE) - 1_440 * USD - 2_880 * USD) as i128,
        "A_post = W − $1,440 of uPnL − $2,880 of PIM"
    );
    assert!(f.a < reduce_fee as i128, "the old gate (A_post ≥ fee) would have refused it");
}

/// The whole-leg case: ALICE holds CROSSED 3x longs of 20 on M1 and 60 on M2. M1 falls to $60:
/// her account is healthy (`E = $1,896 ≥ Σ MM = $1,200`), and closing the losing M1 leg outright
/// leaves `A_post = E − PIM(M2) = −$104` — the M2 leg's `$2,000` of initial margin outweighs the
/// account's whole equity. The close goes through; the account is healthier after it.
#[test]
fn a_cross_wallet_closes_a_losing_leg_whatever_the_other_legs_initial_margin() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
        set_type(&mut ctx, CAROL, m, ISOLATED);
        set_leverage(&mut ctx, ALICE, m, 3);
    }
    set_wallet(&mut ctx, ALICE, 2_700 * USD);
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 20);
    trade(&mut ctx, M2, ALICE, BOB, BUY, px(100), 60);
    assert_eq!(wallet(&mut ctx, ALICE), 2_696 * USD);
    set_mark(&mut ctx, M1, px(60));
    let f = fold(&mut ctx, ALICE);
    assert_eq!((f.e, f.cross_mm), ((1_896 * USD) as i128, (1_200 * USD) as i128));
    assert!(f.a < 0, "under initial margin: {f:?}");
    ok(&mut ctx, CAROL, &order(M1, BUY, px(60), 20, GTC));
    let ledger = Ledger::open(&mut ctx);

    ok(&mut ctx, ALICE, &ro_order(M1, SELL, px(60), 20, IOC));
    ledger.check(&mut ctx, "after the close");
    assert_eq!((pos(&mut ctx, ALICE, M1).amount, pos(&mut ctx, ALICE, M2).amount), (0, 60));
    let close_fee = fee(value(px(60), 20), TAKER_PPM);
    assert_eq!(wallet(&mut ctx, ALICE), 2_696 * USD - 800 * USD - close_fee);
    let f = fold(&mut ctx, ALICE);
    assert_eq!(f.a, (1_896 * USD - close_fee - 2_000 * USD) as i128, "A_post = E_post − PIM(M2)");
    assert_eq!(f.cross_mm, (1_000 * USD) as i128);
    assert!(!f.below_cross_maintenance());
}

/// A fired stop-loss is a reduce-only TAKER, so the same gate decided it — and refused it on a
/// healthy account under initial margin, which `fire` treats as final: it spent the conditional as
/// `AlgoOrderCancelled(TriggerRejected)` and left the position open. ALICE (the scene above) holds a
/// reduce-only STOP_MARKET sell of 18 at $81; the tick to $80 fires it and it fills into CAROL's bid.
#[test]
fn a_fired_cross_stop_loss_fills_at_a_negative_available_balance() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    healthy_under_im_scene(&mut ctx);
    const STOP_MARKET: u8 = 1;
    ok(&mut ctx, ALICE, &algo(SELL, STOP_MARKET, px(81), 0, 18, RO));
    ok(&mut ctx, CAROL, &order(M1, BUY, px(80), 18, GTC));

    let logs = tick(&mut ctx, M1, px(80), 30);
    assert!(algo_cancels(&logs, ALICE).is_empty(), "the stop was not spent as TriggerRejected");
    assert_eq!(trades_with(&logs, CAROL), 1, "it filled");
    assert_eq!(pos(&mut ctx, ALICE, M1).amount, 72);
    assert!(fold(&mut ctx, ALICE).a < 0, "at a still-negative available balance");
}

/// An ISOLATED reduce of an account that still holds cross exposure after it draws its fee from the
/// same cross wallet, and the silo it hands back only raises `A` — so it is not gated either. CAROL
/// holds a CROSSED 3x long of 10 on M2 and an ISOLATED 3x long of 10 on M1; M2 falls to $30, which
/// takes her account under water (`A < 0`). Selling 1 of the isolated position at $100 goes through.
///
/// An account with NO cross exposure keeps the isolated rule byte for byte: once CAROL has closed
/// the M2 leg — its $700 loss leaves her wallet negative — the same isolated reduce must cover its
/// fee from the wallet it hands its silo back to, and is refused (the cover loop has no same-side
/// order to cancel).
#[test]
fn an_isolated_reduce_on_a_cross_wallet_is_not_margin_gated_either() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
        set_leverage(&mut ctx, CAROL, m, 3);
    }
    set_type(&mut ctx, CAROL, M1, ISOLATED);
    set_wallet(&mut ctx, CAROL, 700 * USD);
    trade(&mut ctx, M2, CAROL, BOB, BUY, px(100), 10); // cross
    trade(&mut ctx, M1, CAROL, BOB, BUY, px(100), 10); // isolated: $333.33 of silo cash
    set_mark(&mut ctx, M2, px(30));
    ok(&mut ctx, BOB, &order(M1, BUY, px(100), 2, GTC));
    let f = fold(&mut ctx, CAROL);
    assert!(f.has_cross && f.a < 0, "cross exposure, under water: {f:?}");
    let ledger = Ledger::open(&mut ctx);

    ok(&mut ctx, CAROL, &ro_order(M1, SELL, px(100), 1, IOC));
    ledger.check(&mut ctx, "after the isolated reduce");
    assert_eq!(pos(&mut ctx, CAROL, M1).amount, 9);
    assert!(fold(&mut ctx, CAROL).a < 0, "admitted at a negative available balance");

    // The cross leg closes at its $700 loss (itself a reduce on a cross wallet): no cross exposure
    // is left, and the wallet is negative.
    ok(&mut ctx, BOB, &order(M2, BUY, px(30), 10, GTC));
    ok(&mut ctx, CAROL, &ro_order(M2, SELL, px(30), 10, IOC));
    assert_eq!(pos(&mut ctx, CAROL, M2).amount, 0);
    let f = fold(&mut ctx, CAROL);
    assert!(!f.has_cross && f.w < 0, "isolated only, on a negative wallet: {f:?}");
    rejects(
        &mut ctx,
        CAROL,
        &ro_order(M1, SELL, px(100), 1, IOC),
        "placeOrder: insufficient perp wallet for margin",
    );
    ledger.check(&mut ctx, "after the refusal");
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

// ── add/removePositionMargin ──────────────────────────────────────────────────────────────────

fn add_margin(market: u64, amount: i64) -> Vec<u8> {
    crate::interface::IPerpDex::addPositionMarginCall { marketId: market, amount: amount as u64 }
        .abi_encode()
}

fn remove_margin(market: u64, amount: i64) -> Vec<u8> {
    crate::interface::IPerpDex::removePositionMarginCall {
        marketId: market,
        amount: amount as u64,
    }
    .abi_encode()
}

/// A market CROSSED in force has no silo: adding to it or removing from it is Binance's `-4049` —
/// with a position or without one (the type check comes first) — and writes nothing.
#[test]
fn position_margin_on_a_crossed_market_is_4049() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    set_type(&mut ctx, BOB, M1, ISOLATED);
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    let (add, remove) =
        (crate::risk::ADD_MARGIN_CROSSED_REJECT, crate::risk::REMOVE_MARGIN_CROSSED_REJECT);
    rejects(&mut ctx, ALICE, &add_margin(M1, USD), add);
    rejects(&mut ctx, ALICE, &remove_margin(M1, USD), remove);
    // M2: CROSSED, flat — still -4049, not "no open position".
    rejects(&mut ctx, ALICE, &add_margin(M2, USD), add);
    rejects(&mut ctx, ALICE, &remove_margin(M2, USD), remove);
}

/// An ISOLATED add is capped by the withdrawable balance `X`, not by `A`: ALICE's cross long on M1
/// has doubled, so `A` holds $1,000 of profit that `X` does not (`X = W − Σ_C MM`). One unit above
/// `X` is refused `-4050`; `X` itself is accepted, and lands in the silo. Removing more than the
/// silo holds still answers with the isolated rule's own string.
#[test]
fn an_isolated_add_is_capped_by_x_to_the_unit_and_remove_is_unchanged() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
    }
    set_type(&mut ctx, ALICE, M2, ISOLATED);
    set_leverage(&mut ctx, ALICE, M1, 3);
    set_wallet(&mut ctx, ALICE, 2_000 * USD);
    trade(&mut ctx, M1, ALICE, BOB, BUY, px(100), 10);
    trade(&mut ctx, M2, ALICE, BOB, BUY, px(100), 5);
    set_mark(&mut ctx, M1, px(200));
    let ledger = Ledger::open(&mut ctx);

    let f = fold(&mut ctx, ALICE);
    let w = wallet(&mut ctx, ALICE) as i128;
    let mm = (value(px(200), 10) / 6) as i128;
    assert_eq!(f.x, w - mm, "X = cWB − Σ_C MM: none of the profit");
    assert!(f.a > f.x + 500 * USD as i128, "A counts the profit: A = {}, X = {}", f.a, f.x);
    let x = f.x as i64;
    rejects(&mut ctx, ALICE, &add_margin(M2, x + 1), crate::risk::ADD_MARGIN_CAP_REJECT);
    let silo = pos(&mut ctx, ALICE, M2).margin;
    ok(&mut ctx, ALICE, &add_margin(M2, x));
    ledger.check(&mut ctx, "after the add");
    assert_eq!(pos(&mut ctx, ALICE, M2).margin, silo + x);
    assert_eq!(wallet(&mut ctx, ALICE) as i128, w - f.x);
    assert_eq!(fold(&mut ctx, ALICE).x, 0);

    let silo = pos(&mut ctx, ALICE, M2).margin;
    rejects(
        &mut ctx,
        ALICE,
        &remove_margin(M2, silo + 1),
        "removePositionMargin: insufficient position margin",
    );
}

// ── Money out stops at X ──────────────────────────────────────────────────────────────────────

fn internal(from_wallet: u8, to_wallet: u8, amount: i64) -> Vec<u8> {
    crate::interface::IPerpDex::internalTransferCall {
        to: ALICE,
        fromWallet: from_wallet,
        toWallet: to_wallet,
        amount: amount as u64,
    }
    .abi_encode()
}

fn set_spot(ctx: &mut TestCtx, user: Address, amount: u64) {
    storage::mutate_account(ctx, user, |a| a.usdc_balance = amount.to_string().into()).unwrap();
}

/// Binance's `maxWithdrawAmount` is every money-out gate, to the unit: with a doubled cross long
/// behind it (`uPnL = +$500`, which `A` counts and `X` does not), `internalTransfer` out of the
/// perp wallet and the admin's `depositInsuranceFund` refuse `X + 1` and take `X` (the
/// `transferFromPerp` twin is P2b's test). Then money IN onto a NEGATIVE cross wallet — through
/// `internalTransfer` from spot — nets against it, and `X` only reopens once the wallet is back
/// above the cross maintenance margin.
#[test]
fn every_money_out_gate_stops_at_x_to_the_unit_with_cross_profit() {
    let mut ctx = make_ctx();
    let _cross = crate::margin_type::test_gate::enable();
    for m in MARKETS {
        set_type(&mut ctx, BOB, m, ISOLATED);
    }
    for u in [ALICE, ADMIN] {
        set_leverage(&mut ctx, u, M2, 3);
        set_wallet(&mut ctx, u, 2_000 * USD);
        trade(&mut ctx, M2, u, BOB, BUY, px(100), 5);
    }
    set_mark(&mut ctx, M2, px(200));
    let ledger = Ledger::open(&mut ctx);

    // internalTransfer, PERP (1) → SPOT (0).
    let f = fold(&mut ctx, ALICE);
    assert_eq!(f.x, wallet(&mut ctx, ALICE) as i128 - (value(px(200), 5) / 6) as i128);
    // `A − X = uPnL − PIM + MM` = $500 − $333.33 + $166.67.
    assert!(f.a > f.x + 300 * USD as i128, "the profit is in A, not in X");
    let x = f.x as i64;
    rejects(
        &mut ctx,
        ALICE,
        &internal(1, 0, x + 1),
        "internalTransfer: insufficient perp wallet balance",
    );
    ok(&mut ctx, ALICE, &internal(1, 0, x));
    assert_eq!(fold(&mut ctx, ALICE).x, 0);

    // depositInsuranceFund, the admin's perp wallet → the fund.
    let x = fold(&mut ctx, ADMIN).x as u64;
    let deposit = |amount: u64| {
        crate::interface::IPerpDex::depositInsuranceFundCall { amount }.abi_encode()
    };
    rejects(
        &mut ctx,
        ADMIN,
        &deposit(x + 1),
        "depositInsuranceFund: insufficient perp wallet balance",
    );
    let if0 = insurance_fund(&mut ctx);
    ok(&mut ctx, ADMIN, &deposit(x));
    assert_eq!(insurance_fund(&mut ctx), if0 + x);
    assert_eq!(fold(&mut ctx, ADMIN).x, 0);
    ledger.check(&mut ctx, "after the withdrawals");

    // A negative cross wallet: X is 0, and money in nets against the deficit first.
    set_wallet(&mut ctx, ALICE, -100 * USD);
    set_spot(&mut ctx, ALICE, 1_000 * USD as u64);
    let ledger = Ledger::open(&mut ctx);
    assert_eq!(fold(&mut ctx, ALICE).x, 0);
    let refused = "internalTransfer: insufficient perp wallet balance";
    rejects(&mut ctx, ALICE, &internal(1, 0, 1), refused);
    ok(&mut ctx, ALICE, &internal(0, 1, 350 * USD));
    assert_eq!(wallet(&mut ctx, ALICE), 250 * USD, "the deposit netted the deficit");
    let mm = value(px(200), 5) / 6;
    assert_eq!(fold(&mut ctx, ALICE).x, (250 * USD - mm) as i128, "X reopens above Σ MM");
    ledger.check(&mut ctx, "after the deposit");
}

// ── Gated off: byte identity ──────────────────────────────────────────────────────────────────

/// Every P2c gate keys on the margin type IN FORCE (or on the account's liquidation flag, which no
/// margin type changes), so with cross margin gated off — production — an account that still
/// STORES the CROSSED default behaves exactly like one explicitly switched to ISOLATED: the same
/// outcomes (revert flag, return bytes, gas), the same events byte for byte, the same state. The
/// scenario walks every gate this slice touched: the taker fills gate (a zero-fee open included),
/// the maker path, the pre-walk filter, `setLeverage` both ways, add / remove margin at and past
/// their caps, `-2023` on placement and on `setMarginType`, and the money-out gates at `X`.
#[test]
fn gated_off_every_p2c_gate_is_byte_identical_to_explicitly_isolated() {
    fn scenario(ctx: &mut TestCtx) -> (Vec<(bool, Vec<u8>, u64)>, Vec<Log>) {
        let mut outs = Vec::new();
        let mut run = |ctx: &mut TestCtx, who: Address, input: Vec<u8>| {
            let out = call(ctx, who, &input);
            outs.push((out.reverted, out.bytes.to_vec(), out.gas_used));
        };
        // A taker open and a maker fill; leverage raised under the position, then the refused
        // lowering; the margin caps.
        run(ctx, ALICE, setLeverageCall { marketId: M1, leverage: 2 }.abi_encode());
        run(ctx, BOB, order(M1, SELL, px(100), 10, GTC));
        run(ctx, ALICE, order(M1, BUY, px(100), 10, IOC));
        run(ctx, ALICE, setLeverageCall { marketId: M1, leverage: 3 }.abi_encode());
        run(ctx, ALICE, setLeverageCall { marketId: M1, leverage: 1 }.abi_encode());
        run(ctx, ALICE, add_margin(M1, WALLET));
        run(ctx, ALICE, add_margin(M1, 10 * USD));
        run(ctx, ALICE, remove_margin(M1, 5 * USD));
        run(ctx, ALICE, remove_margin(M1, WALLET));
        // A zero-fee taker open (B1 governs an isolated one exactly as before).
        set_taker_fee(ctx, CAROL, 0);
        run(ctx, BOB, order(M1, SELL, px(100), 3, GTC));
        run(ctx, CAROL, order(M1, BUY, px(100), 3, IOC));
        // The pre-walk filter: an order far beyond CAROL's thin wallet, on a book it would cross.
        set_wallet(ctx, CAROL, 50 * USD);
        run(ctx, BOB, order(M2, SELL, px(90), 50, GTC));
        run(ctx, CAROL, order(M2, BUY, px(90), 50, GTC));
        // -2023, on placement (a plain order refused, a reduce-only one through) and on the switch.
        storage::set_in_liquidation_for_test(ctx, ALICE, true).unwrap();
        run(ctx, ALICE, order(M1, BUY, px(90), 1, GTC));
        run(ctx, BOB, order(M1, BUY, px(100), 1, GTC));
        run(ctx, ALICE, ro_order(M1, SELL, px(100), 1, IOC));
        run(ctx, ALICE, setMarginTypeCall { marketId: M2, marginType: ISOLATED }.abi_encode());
        storage::set_in_liquidation_for_test(ctx, ALICE, false).unwrap();
        // The money-out gates at and past X.
        let x = fold(ctx, ALICE).x as i64;
        run(ctx, ALICE, transfer_out(x as u64 + 1));
        run(ctx, ALICE, internal(1, 0, x + 1));
        run(ctx, ALICE, internal(1, 0, x / 2));
        run(ctx, ALICE, transfer_out((x - x / 2) as u64));
        (outs, JournalTr::take_logs(ctx.journal_mut()))
    }

    fn state(ctx: &mut TestCtx) -> Vec<String> {
        let mut out = vec![format!("IF {}", insurance_fund(ctx))];
        for u in USERS {
            out.push(format!("{:?}", storage::load_account(ctx, u).unwrap()));
            for m in MARKETS {
                let mut p = pos(ctx, u, m);
                p.margin_type = 0; // the one byte the two runs store differently, on purpose
                out.push(format!("{:?}", p));
            }
        }
        out
    }

    // Gated off — production: every slot still stores the CROSSED default.
    let mut gated = make_ctx();
    let (gated_outs, gated_logs) = scenario(&mut gated);
    let gated_state = state(&mut gated);

    // Gated on, every slot ISOLATED by choice.
    let mut isolated = make_ctx();
    let (isolated_outs, isolated_logs, isolated_state) = {
        let _cross = crate::margin_type::test_gate::enable();
        for u in USERS {
            for m in MARKETS {
                set_type(&mut isolated, u, m, ISOLATED);
            }
        }
        let (outs, logs) = scenario(&mut isolated);
        (outs, logs, state(&mut isolated))
    };

    assert_eq!(gated_outs, isolated_outs, "the same outcomes, return data and gas");
    assert_eq!(gated_logs, isolated_logs, "the same events, byte for byte");
    assert_eq!(gated_state, isolated_state, "the same stored state");
    // The scenario reached what it claims to.
    let reasons: Vec<String> = gated_outs
        .iter()
        .filter(|o| o.0)
        .map(|o| alloy_sol_types::Revert::abi_decode(&o.1).map(|r| r.reason).unwrap_or_default())
        .collect();
    for want in [
        "setLeverage: cannot reduce leverage with open position",
        crate::risk::ADD_MARGIN_CAP_REJECT,
        "removePositionMargin: insufficient position margin",
        "placeOrder: insufficient perp wallet for margin",
        crate::trading::IN_LIQUIDATION_REJECT,
        "setMarginType: -4046 No need to change margin type.",
        "transferFromPerp: insufficient perp wallet balance",
        "internalTransfer: insufficient perp wallet balance",
    ] {
        assert!(reasons.iter().any(|r| r == want), "never refused with {want:?}: {reasons:?}");
    }
    assert_eq!(gated_outs.iter().filter(|o| o.0).count(), reasons.len());
    assert_eq!(reasons.len(), 8, "{reasons:?}");
}
