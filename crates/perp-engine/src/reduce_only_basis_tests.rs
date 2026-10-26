//! Reduce-only evictions and the admission basis: an order is never charged for the reduce-only
//! orders its own acceptance evicts.
//!
//! The engine evicts a reduce-only order the moment it can no longer reduce — after a fill shrinks,
//! flips or closes the position under it (the match flush's pass, `ReduceOnlyPositionShrank` /
//! `ReduceOnlyWouldOpen`), and after a NORMAL order rests ahead of it (the overtake pass,
//! `ReduceOnlyOvertaken`). Both passes run BEHIND the write barrier, after the order that triggers
//! them has been admitted, so the gates are measured on the state the passes leave
//! (`settlement::post_flush_snapshot`, and the replay in `rest_in_book`): on Binance a reduce-only
//! order carries no open-order margin and is expired once it can no longer reduce, so an order
//! whose acceptance kills one was never liable for it.
//!
//! Measured on the pre-eviction state instead — the bug these pin, live since reduce-only eviction
//! landed (`be2aa5a0`) — the gates were wrong in BOTH directions:
//! * too STRICT: the evicted order's `Bid`/`Ask` term was charged to the order that evicts it, so
//!   an affordable rest was refused, and the whole order with it — by either pass;
//! * too LOOSE: the taker's cover simulation popped LIFO from lists the real cover loop never sees
//!   (it runs after the flush's pass), dropped an order the real loop then keeps, and the rest was
//!   admitted on a basis without it — leaving the account at `A < 0` after a voluntary placement.
//!
//! In debug builds `rest_in_book`'s basis assertion ("the rest basis diverged from what the flush
//! actually wrote") fired on both flush cases. The overtake case tripped nothing — no assertion
//! compared the rest's decision with what its pass left — until the check after that pass, which
//! is the second oracle every scenario here runs under.
//!
//! A child of `cross_settlement_tests`, so it runs on that fixture ([`Ledger`], `rejects`, …).
//! Every scenario runs once per margin type, and ALICE trades fee-free so each boundary is exact to
//! the quote unit.
use super::*;
use crate::interface::IPerpDex::OrderCancelled;
use crate::types::CancelReason;

/// `placeOrder` flags bit 0.
const REDUCE_ONLY: u8 = 1;

fn reduce_only_bid(price: u64, qty: u64) -> Vec<u8> {
    placeOrderCall {
        marketId: M1,
        side: BUY,
        price,
        quantity: qty,
        orderType: 0,
        tif: GTC,
        clientOrderId: FixedBytes::default(),
        flags: REDUCE_ONLY,
    }
    .abi_encode()
}

/// The id `user`'s next order will get.
fn next_id(ctx: &mut TestCtx, user: Address) -> [u8; 32] {
    crate::trading::peek_order_id(ctx, user).unwrap().0
}

/// `(orderId, reason)` of every `OrderCancelled` in `logs`.
fn cancels(logs: &[Log]) -> Vec<([u8; 32], u8)> {
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
        .map(|l| OrderCancelled::decode_raw_log(l.data.topics(), &l.data.data).unwrap())
        .map(|e| (e.orderId.0, e.reason))
        .collect()
}

/// ALICE's resting buys on M1, as `(price, qty, reduce_only)`, nearest the touch first.
fn alice_bids(ctx: &mut TestCtx) -> Vec<(u64, u64, bool)> {
    storage::load_buy_orders(ctx, ALICE, M1)
        .unwrap()
        .iter()
        .map(|e| (e.price, e.amount, e.reduce_only))
        .collect()
}

fn available(ctx: &mut TestCtx, user: Address) -> i128 {
    crate::margin_view::derived_available_balance(ctx, user).unwrap()
}

/// The place path's insufficient-margin refusal for an order on a position of `margin_type`.
fn margin_reject(margin_type: u8) -> &'static str {
    if margin_type == CROSSED {
        crate::trading::CROSS_INSUFFICIENT_MARGIN_REJECT
    } else {
        "placeOrder: insufficient perp wallet for margin"
    }
}

/// ALICE, `margin_type` on M1 at leverage 1 (the default), fee-free, holding `wallet`: SHORT
/// `short` at $100 against BOB, then a reduce-only BUY 5 @ $90 resting — `Bid = $450` and no
/// `ooIM`, since it only closes what the short opened. Returns that order's id.
fn short_with_a_reduce_only_bid(
    ctx: &mut TestCtx,
    margin_type: u8,
    wallet: i64,
    short: u64,
) -> [u8; 32] {
    // A position nobody switched is CROSSED already (`-4046` for switching it to that).
    if margin_type == ISOLATED {
        set_type(ctx, ALICE, M1, ISOLATED);
    }
    storage::mutate_account(ctx, ALICE, |a| {
        a.taker_fee_ppm = 0;
        a.maker_fee_ppm = 0;
    })
    .unwrap();
    set_wallet(ctx, ALICE, wallet);
    trade(ctx, M1, ALICE, BOB, SELL, px(100), short);
    let reduce_only = next_id(ctx, ALICE);
    ok(ctx, ALICE, &reduce_only_bid(px(90), 5));
    assert_eq!(pos(ctx, ALICE, M1).amount, -(short as i64));
    assert_eq!(alice_bids(ctx), vec![(px(90), 5, true)]);
    reduce_only
}

// ── The flush's pass: a fill that leaves the taker's reduce-only order unable to reduce ─────────

/// The probe that found it, on deep wallets so nothing but the basis is under test: ALICE closes
/// her short 5 with a crossing GTC buy 8 and rests 3. Flat after the fill, her reduce-only buy can
/// only open, so the flush evicts it — and the rest is decided on the state without it. Before the
/// fix `rest_in_book` asserted `(0, 0, 0, 0) == (5, $450, 0, 0)` here in debug builds: the basis
/// still held the evicted order.
#[test]
fn a_crossing_order_that_closes_the_position_rests_on_the_basis_the_flush_leaves() {
    for margin_type in [ISOLATED, CROSSED] {
        let mut ctx = make_ctx();
        let reduce_only = short_with_a_reduce_only_bid(&mut ctx, margin_type, WALLET, 5);
        ok(&mut ctx, BOB, &order(M1, SELL, px(100), 5, GTC));
        let ledger = Ledger::open(&mut ctx);

        let rest = next_id(&mut ctx, ALICE);
        let logs = ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 8, GTC));
        ledger.check(&mut ctx, "after the crossing buy");

        let p = pos(&mut ctx, ALICE, M1);
        assert_eq!(p.amount, 0, "the short is closed");
        assert_eq!(
            cancels(&logs),
            vec![(reduce_only, CancelReason::ReduceOnlyWouldOpen as u8)],
            "flat, the reduce-only buy could only open — and it is the only order cancelled"
        );
        assert_eq!(alice_bids(&mut ctx), vec![(px(100), 3, false)], "only the remainder rests");
        assert_eq!((p.total_buy_qty, p.total_buy_notional), (3, value(px(100), 3) as u64));
        assert!(storage::load_order(&mut ctx, &rest).unwrap().is_some());
    }
}

/// The release-build half, to the unit: a rest is priced WITHOUT the reduce-only order its own
/// fill evicts. ALICE closes her short 5 with a crossing GTC buy 11 @ $100 and rests 6 — flat, the
/// remainder's `ooIM` is `$600` at leverage 1, against a wallet of `$600` (an isolated short's silo
/// is back in it, a cross wallet never left). Accepted at exactly `$600`, landing on `A = 0`;
/// refused one quote unit below, writing nothing.
///
/// Before the fix the rest was priced beside the evicted `$450` (`Bid = $1,050`, flat), so this
/// order was refused at any wallet below `$1,050`.
#[test]
fn a_rest_is_not_charged_for_the_reduce_only_order_its_own_fill_evicts() {
    for margin_type in [ISOLATED, CROSSED] {
        // One unit short of the remainder's `ooIM`: refused whole — fills included — and the
        // reduce-only buy is still resting.
        let mut ctx = make_ctx();
        short_with_a_reduce_only_bid(&mut ctx, margin_type, 600 * USD - 1, 5);
        ok(&mut ctx, BOB, &order(M1, SELL, px(100), 5, GTC));
        rejects(&mut ctx, ALICE, &order(M1, BUY, px(100), 11, GTC), margin_reject(margin_type));
        assert_eq!(alice_bids(&mut ctx), vec![(px(90), 5, true)]);
        assert_eq!(pos(&mut ctx, ALICE, M1).amount, -5);

        // Exactly enough: accepted, the reduce-only buy evicted, `A = 0`.
        let mut ctx = make_ctx();
        let reduce_only = short_with_a_reduce_only_bid(&mut ctx, margin_type, 600 * USD, 5);
        ok(&mut ctx, BOB, &order(M1, SELL, px(100), 5, GTC));
        let ledger = Ledger::open(&mut ctx);
        let logs = ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 11, GTC));
        ledger.check(&mut ctx, "after the crossing buy");
        assert_eq!(pos(&mut ctx, ALICE, M1).amount, 0);
        assert_eq!(cancels(&logs), vec![(reduce_only, CancelReason::ReduceOnlyWouldOpen as u8)]);
        assert_eq!(alice_bids(&mut ctx), vec![(px(100), 6, false)]);
        assert_eq!(wallet(&mut ctx, ALICE), 600 * USD);
        assert_eq!(available(&mut ctx, ALICE), 0, "the rest used the whole wallet, to the unit");
    }
}

/// The other direction, to the unit: the cover simulation starts from the lists the flush leaves,
/// so it never "cancels" an order the real cover loop keeps.
///
/// ALICE: short 5 with the reduce-only buy 5 @ $90, plus a NORMAL buy 6 @ $80 behind it (free: it
/// hedges the short). A crossing GTC buy 9 @ $100 fills 8 — closing the short and opening a long
/// 3, whose `$300` initial margin the fills gate has to find — and rests 1. The flush evicts the
/// reduce-only buy (it would open the long further), which leaves the fills covered with the $80
/// buy still resting: `A = W − $480 ≥ $300` (isolated: `W` after the silo came back; cross: the
/// same with the `$300` inside `A`). The rest then lands on `Bid = $580` against `W − $300`, so it
/// needs `W ≥ $880` exactly.
///
/// Before the fix the fills gate counted the evicted `$450` too (`A = W − $930`), found itself
/// short, and simulated popping the $80 buy — the LIFO back of the PRE-eviction list. The real loop
/// runs after the flush, finds the fills covered and cancels nothing. The rest was then decided on
/// the simulation's `Bid = $450 + $100` and ADMITTED down to `W = $850`: at `W = $880 − 1` it left
/// ALICE at `A = −1` after a voluntary placement, which is what this refuses now.
#[test]
fn the_cover_simulation_starts_from_the_lists_the_flush_leaves() {
    for margin_type in [ISOLATED, CROSSED] {
        let scene = |ctx: &mut TestCtx, wallet: i64| {
            let reduce_only = short_with_a_reduce_only_bid(ctx, margin_type, wallet, 5);
            ok(ctx, ALICE, &order(M1, BUY, px(80), 6, GTC));
            ok(ctx, BOB, &order(M1, SELL, px(100), 8, GTC));
            assert_eq!(
                alice_bids(ctx),
                vec![(px(90), 5, true), (px(80), 6, false)],
                "the normal buy sits BEHIND the reduce-only one, so it is the LIFO back"
            );
            reduce_only
        };

        let mut ctx = make_ctx();
        scene(&mut ctx, 880 * USD - 1);
        rejects(&mut ctx, ALICE, &order(M1, BUY, px(100), 9, GTC), margin_reject(margin_type));
        assert_eq!(alice_bids(&mut ctx), vec![(px(90), 5, true), (px(80), 6, false)]);

        let mut ctx = make_ctx();
        let reduce_only = scene(&mut ctx, 880 * USD);
        let ledger = Ledger::open(&mut ctx);
        let logs = ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 9, GTC));
        ledger.check(&mut ctx, "after the crossing buy");
        assert_eq!(pos(&mut ctx, ALICE, M1).amount, 3, "flipped to a long 3");
        assert_eq!(
            cancels(&logs),
            vec![(reduce_only, CancelReason::ReduceOnlyWouldOpen as u8)],
            "the eviction is the only cancel — no cover cancel of the $80 buy"
        );
        assert_eq!(alice_bids(&mut ctx), vec![(px(100), 1, false), (px(80), 6, false)]);
        assert_eq!(available(&mut ctx, ALICE), 0, "the rest used the whole wallet, to the unit");
    }
}

// ── The overtake pass: a normal order resting ahead of the owner's reduce-only order ───────────

/// A rest that OVERTAKES is priced without the reduce-only order it kills, to the unit. ALICE,
/// short 5 with the reduce-only buy 5 @ $90, rests a normal buy 16 @ $100 (nothing to cross):
/// nearer the touch, it fills first, so the reduce-only buy can no longer be covered and the pass
/// evicts it. What rests is `Bid = $1,600` against `N = −$500` — `ooIM = $1,100 − $500 = $600` at
/// leverage 1 — and the account holds `A = W − $500` (an isolated short's silo left the wallet; a
/// cross short's `PIM` sits in `A`), so it needs `W ≥ $1,100` exactly. One unit below it is refused
/// and the reduce-only buy survives.
///
/// Before the fix the rest was priced beside the order it evicts (`Bid = $2,050`,
/// `ooIM = $1,050`) and refused below `W = $1,550`.
#[test]
fn a_rest_is_not_charged_for_the_reduce_only_order_it_overtakes() {
    for margin_type in [ISOLATED, CROSSED] {
        let mut ctx = make_ctx();
        short_with_a_reduce_only_bid(&mut ctx, margin_type, 1_100 * USD - 1, 5);
        rejects(&mut ctx, ALICE, &order(M1, BUY, px(100), 16, GTC), margin_reject(margin_type));
        assert_eq!(alice_bids(&mut ctx), vec![(px(90), 5, true)]);

        let mut ctx = make_ctx();
        let reduce_only = short_with_a_reduce_only_bid(&mut ctx, margin_type, 1_100 * USD, 5);
        let ledger = Ledger::open(&mut ctx);
        let logs = ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 16, GTC));
        ledger.check(&mut ctx, "after the overtaking buy");
        assert_eq!(pos(&mut ctx, ALICE, M1).amount, -5, "nothing crossed");
        assert_eq!(cancels(&logs), vec![(reduce_only, CancelReason::ReduceOnlyOvertaken as u8)]);
        assert_eq!(alice_bids(&mut ctx), vec![(px(100), 16, false)]);
        assert_eq!(available(&mut ctx, ALICE), 0, "the rest used the whole wallet, to the unit");
    }
}

/// The same on the deferred path: a crossing GTC whose fills only SHRINK the position, so the
/// flush's pass keeps the reduce-only order, and whose REMAINDER then overtakes it — the replay
/// runs on the list the match hands forward (`RestBasis::own_side_list`), not on the store.
///
/// ALICE short 10 with the reduce-only buy 5 @ $90; a GTC buy 22 @ $100 fills 2 against BOB
/// (short 8: `5 ≤ 8`, the reduce-only buy still fits) and rests 20, which overtakes it (`20 + 5 >
/// 8`). What rests is `Bid = $2,000` against `N = −$800`: `ooIM = $1,200 − $800 = $400`, against
/// `A = W − $800`, so `W ≥ $1,200` exactly. Before the fix: `ooIM = $850`, refused below `$1,650`.
#[test]
fn a_crossing_remainder_is_not_charged_for_the_reduce_only_order_it_overtakes() {
    for margin_type in [ISOLATED, CROSSED] {
        let mut ctx = make_ctx();
        short_with_a_reduce_only_bid(&mut ctx, margin_type, 1_200 * USD - 1, 10);
        ok(&mut ctx, BOB, &order(M1, SELL, px(100), 2, GTC));
        rejects(&mut ctx, ALICE, &order(M1, BUY, px(100), 22, GTC), margin_reject(margin_type));
        assert_eq!(alice_bids(&mut ctx), vec![(px(90), 5, true)]);

        let mut ctx = make_ctx();
        let reduce_only = short_with_a_reduce_only_bid(&mut ctx, margin_type, 1_200 * USD, 10);
        ok(&mut ctx, BOB, &order(M1, SELL, px(100), 2, GTC));
        let ledger = Ledger::open(&mut ctx);
        let logs = ok(&mut ctx, ALICE, &order(M1, BUY, px(100), 22, GTC));
        ledger.check(&mut ctx, "after the crossing buy");
        assert_eq!(pos(&mut ctx, ALICE, M1).amount, -8);
        assert_eq!(
            cancels(&logs),
            vec![(reduce_only, CancelReason::ReduceOnlyOvertaken as u8)],
            "the flush's pass kept it (it still fits the short 8); the REST evicted it"
        );
        assert_eq!(alice_bids(&mut ctx), vec![(px(100), 20, false)]);
        assert_eq!(available(&mut ctx, ALICE), 0, "the rest used the whole wallet, to the unit");
    }
}
