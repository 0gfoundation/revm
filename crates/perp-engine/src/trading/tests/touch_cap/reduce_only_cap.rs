//! The reduce-only cap (owner decision 2026-11-17): an account holds at most
//! `MAX_REDUCE_ONLY_ORDERS_PER_MARKET` = 10 reduce-only orders RESTING in one market (resting-kind
//! conditionals share the budget — covered with the trigger fixture in `algo::trailing_spec_tests`).
//! It bounds the post-fill eviction pass a touched maker can cost (see
//! `adversarial::brk_reduce_only_evictions_are_bounded_by_the_reduce_only_cap`).
//!
//! Every expected value is derived by hand from the book each test builds. The fixture is
//! `touch_cap`'s: tick 1, step 1, no fees, a $100.00 mark. A position is opened against CAROL at the
//! mark ([`open_pos`]); the reduce-only orders rest 500 ticks away from the touch on the closing
//! side, where nothing crosses them.
use super::*;
use crate::interface::IPerpDex::{asSubAccountCall, createSubAccountCall};
use crate::trading::reduce_only::{
    ALGO_REDUCE_ONLY_CAP_REJECT, MAX_REDUCE_ONLY_ORDERS_PER_MARKET, REDUCE_ONLY_CAP_REJECT,
};

const RO: u8 = 1;
const PO: u8 = 3;
const RO_CAP: usize = 10;
const E_RO: &str = REDUCE_ONLY_CAP_REJECT;
/// The two closing sides: SELL closes a long, BUY a short.
const CLOSES: [u8; 2] = [SELL, BUY];

/// The opposite side.
fn opp(side: u8) -> u8 {
    1 - side
}

/// A price on `close`'s side 500 ticks behind the touch — never crossed in these tests.
fn far(close: u8) -> u64 {
    if close == SELL {
        MARK + 500
    } else {
        MARK - 500
    }
}

/// Opens `qty` on `user` so that `close` is its closing side: CAROL rests `qty` on `close` at the
/// mark and `user` takes it with an IOC.
fn open_pos(ctx: &mut TestCtx, user: Address, close: u8, qty: u64) {
    rest(ctx, CAROL, close, MARK, qty);
    ok(ctx, user, &limit(opp(close), MARK, qty, IOC, 0));
    let want = if close == SELL { qty as i64 } else { -(qty as i64) };
    assert_eq!(amount(ctx, user), want);
}

/// `user`'s resting reduce-only orders in the market, both sides.
fn ro_count(ctx: &mut TestCtx, user: Address) -> usize {
    let b = storage::load_buy_orders(ctx, user, MARKET_ID).unwrap();
    let s = storage::load_sell_orders(ctx, user, MARKET_ID).unwrap();
    b.iter().chain(s.iter()).filter(|e| e.reduce_only).count()
}

/// A reduce-only order that must REST, untouched; returns its id.
fn rest_ro(ctx: &mut TestCtx, user: Address, close: u8, tif: u8) -> [u8; 32] {
    let (out, logs) = ok(ctx, user, &limit(close, far(close), 1, tif, RO));
    assert_eq!(evs::<OrderRested>(&logs).len(), 1, "the reduce-only order did not rest");
    assert!(evs::<Trade>(&logs).is_empty());
    order_id_of(&out)
}

/// `n` resting reduce-only orders for `user`.
fn fill_ro(ctx: &mut TestCtx, user: Address, close: u8, n: usize) -> Vec<[u8; 32]> {
    (0..n).map(|_| rest_ro(ctx, user, close, GTC)).collect()
}

fn ro_item(side: u8, price: u64, qty: u64, order_type: u8, tif: u8) -> PlaceItem {
    PlaceItem { flags: RO, ..item(side, price, qty, order_type, tif) }
}

/// The strings state the number, and a batch item reports the refusal as `Other` (255).
#[test]
fn ro_cap_strings_state_their_constant() {
    assert_eq!(MAX_REDUCE_ONLY_ORDERS_PER_MARKET, RO_CAP);
    assert!(REDUCE_ONLY_CAP_REJECT.starts_with("placeOrder: too many reduce-only orders"));
    assert!(ALGO_REDUCE_ONLY_CAP_REJECT.starts_with("placeAlgoOrder: too many reduce-only orders"));
    for msg in [REDUCE_ONLY_CAP_REJECT, ALGO_REDUCE_ONLY_CAP_REJECT] {
        assert!(msg.contains(&format!(" {MAX_REDUCE_ONLY_ORDERS_PER_MARKET} ")), "{msg}");
    }
    assert_eq!(
        crate::batch::reason_code(&crate::errors::perp_err(REDUCE_ONLY_CAP_REJECT)) as u8,
        R_OTHER
    );
}

/// Boundaries 9 / 10 / 11, both closing sides, GTC and PostOnly: the 10th rests, the 11th is
/// refused write-clean whichever resting kind it is; cancelling one frees exactly one slot; and the
/// count is per ACCOUNT — BOB, also at 10 of his own, is unaffected by ALICE's and vice versa.
#[test]
fn ro_cap_boundaries_9_10_11() {
    for close in CLOSES {
        let mut ctx = fresh();
        open_pos(&mut ctx, ALICE, close, 100);
        open_pos(&mut ctx, BOB, close, 100);
        let ids = fill_ro(&mut ctx, ALICE, close, 9);
        assert_eq!(ro_count(&mut ctx, ALICE), 9);
        // The 10th — a PostOnly this time — rests.
        rest_ro(&mut ctx, ALICE, close, PO);
        assert_eq!(ro_count(&mut ctx, ALICE), 10);
        // The 11th: refused for both resting kinds, with zero writes.
        rejected(&mut ctx, ALICE, &limit(close, far(close), 1, GTC, RO), E_RO);
        rejected(&mut ctx, ALICE, &limit(close, far(close), 1, PO, RO), E_RO);
        assert_eq!(ro_count(&mut ctx, ALICE), 10);
        // BOB has his own ten.
        fill_ro(&mut ctx, BOB, close, 10);
        rejected(&mut ctx, BOB, &limit(close, far(close), 1, GTC, RO), E_RO);
        // A NORMAL order is not limited by it (ALICE's opening side, far from the touch).
        let normal = if close == SELL { MARK - 500 } else { MARK + 500 };
        rest(&mut ctx, ALICE, opp(close), normal, 1);
        // Cancel one: exactly one slot frees.
        ok(&mut ctx, ALICE, &cancelOrderCall { orderId: ids[0].into(), marketId: MARKET_ID }.abi_encode());
        assert_eq!(ro_count(&mut ctx, ALICE), 9);
        rest_ro(&mut ctx, ALICE, close, GTC);
        rejected(&mut ctx, ALICE, &limit(close, far(close), 1, GTC, RO), E_RO);
        assert_eq!((ro_count(&mut ctx, ALICE), ro_count(&mut ctx, BOB)), (10, 10));
    }
}

/// At 10, a reduce-only order that does NOT rest is never refused: IOC, market, FOK, a GTC that
/// fills completely, and a GTC whose remainder self-trade prevention expires. A GTC whose remainder
/// WOULD rest is refused whole and write-clean — its fills do not happen either.
///
/// Hand-derived for a long (close = SELL; the short is the mirror): ALICE long 100 with ten 1-lot
/// reduce-only asks; DAVE bids 20 at the mark.
///   IOC 2 → fills 2 (DAVE 18 left), market 1 → 17, FOK 1 → 16, GTC 1 at the mark → 15, all with no
///   `OrderRested`; position 100 − 5 = 95, and Σ reduce-only 10 ≤ 95 evicts nothing.
///   GTC 30 at the mark would fill DAVE's 15 and rest 15 → refused; DAVE still bids 15.
///   ALICE's own normal bid one tick above the mark is met first by an EXPIRE_TAKER GTC sell at the
///   mark → nothing fills, the whole 3 expire (reason 9), nothing rests.
#[test]
fn ro_cap_a_reduce_only_order_that_does_not_rest_is_not_counted() {
    for close in CLOSES {
        let mut ctx = fresh();
        open_pos(&mut ctx, ALICE, close, 100);
        fill_ro(&mut ctx, ALICE, close, RO_CAP);
        rest(&mut ctx, DAVE, opp(close), MARK, 20);
        let sign = if close == SELL { 1 } else { -1 };
        for (input, q) in [
            (limit(close, MARK, 2, IOC, RO), 2),
            (place(close, 0, 1, MKT, GTC, RO), 1),
            (limit(close, MARK, 1, FOK, RO), 1),
            (limit(close, MARK, 1, GTC, RO), 1),
        ] {
            let (_, logs) = ok(&mut ctx, ALICE, &input);
            assert_eq!(evs::<Trade>(&logs).iter().map(|t| t.quantity).sum::<u64>(), q);
            assert!(evs::<OrderRested>(&logs).is_empty());
            assert!(cancels(&logs).is_empty(), "nothing evicted");
        }
        assert_eq!(amount(&mut ctx, ALICE), 95 * sign);
        assert_eq!(ro_count(&mut ctx, ALICE), RO_CAP);
        assert_eq!(level(&mut ctx, opp(close), MARK).1, 1, "DAVE's one order, 15 left");
        // Would fill 15 and rest 15: refused whole.
        rejected(&mut ctx, ALICE, &limit(close, MARK, 30, GTC, RO), E_RO);
        assert_eq!(amount(&mut ctx, ALICE), 95 * sign);
        // STP stops it before anything fills: the remainder expires, so nothing rests.
        let own = if close == SELL { MARK + 1 } else { MARK - 1 };
        rest(&mut ctx, ALICE, opp(close), own, 1);
        let (out, logs) = ok(&mut ctx, ALICE, &limit(close, MARK, 3, GTC, RO | ET));
        assert_eq!(expiry(&logs), (order_id_of(&out), 0, 3, X_STP));
        assert!(evs::<OrderRested>(&logs).is_empty());
        assert_eq!(ro_count(&mut ctx, ALICE), RO_CAP);
    }
}

/// `placeOrderSigned`: the 10th rests, the 11th is refused keeping exactly the signature burn (the
/// burn measured on a twin refused by validation instead), and its signature is spent.
#[test]
fn ro_cap_signed_placement() {
    for close in CLOSES {
        let burn = {
            let mut twin = fresh();
            register_alice_key(&mut twin);
            let input = signed_place(&mut twin, close, far(close), 1, GTC, 0x10);
            let w = twin.perp_write_count();
            let out = run_perp_dex_call(&input, GAS, DAVE, U256::ZERO, false, &mut twin).unwrap();
            assert_eq!(revert_reason(&out), "placeOrder: unknown order flags 0x10");
            twin.perp_write_count() - w
        };
        let mut ctx = fresh();
        register_alice_key(&mut ctx);
        open_pos(&mut ctx, ALICE, close, 100);
        fill_ro(&mut ctx, ALICE, close, 9);
        // Distinct prices keep the two signatures distinct.
        let tenth = signed_place(&mut ctx, close, far(close), 1, GTC, RO);
        let (_, logs) = ok(&mut ctx, DAVE, &tenth);
        assert_eq!(evs::<OrderRested>(&logs).len(), 1);
        let p11 = if close == SELL { far(close) + 1 } else { far(close) - 1 };
        let eleventh = signed_place(&mut ctx, close, p11, 1, GTC, RO);
        rejected_burning(&mut ctx, DAVE, &eleventh, E_RO, burn);
        rejected_burning(&mut ctx, DAVE, &eleventh, E_DUP_SIG, 0);
        assert_eq!(ro_count(&mut ctx, ALICE), RO_CAP);
    }
}

/// A batch item: at 9 the first reduce-only GTC item rests (10), the second is `Rejected` reason
/// 255 and the batch carries on — the third, a reduce-only IOC filling 1 against DAVE, is
/// `Terminal`. Two more items in a second batch: a reduce-only PostOnly `Rejected`, a normal GTC
/// `Accepted`.
#[test]
fn ro_cap_batch_item() {
    for close in CLOSES {
        let mut ctx = fresh();
        open_pos(&mut ctx, ALICE, close, 100);
        fill_ro(&mut ctx, ALICE, close, 9);
        rest(&mut ctx, DAVE, opp(close), MARK, 5);
        let (out, logs) = ok(
            &mut ctx,
            ALICE,
            &batch(vec![
                ro_item(close, far(close), 1, LIMIT, GTC),
                ro_item(close, far(close), 1, LIMIT, GTC),
                ro_item(close, MARK, 1, LIMIT, IOC),
            ]),
        );
        assert_eq!(
            tags(&out),
            vec![(TAG_ACCEPTED, R_NONE), (TAG_REJECTED, R_OTHER), (TAG_TERMINAL, R_NONE)]
        );
        assert_eq!(evs::<OrderRested>(&logs).len(), 1);
        assert_eq!(evs::<Trade>(&logs).len(), 1);
        assert_eq!(ro_count(&mut ctx, ALICE), RO_CAP);
        let normal = if close == SELL { MARK - 500 } else { MARK + 500 };
        let (out, _) = ok(
            &mut ctx,
            ALICE,
            &batch(vec![
                ro_item(close, far(close), 1, LIMIT, PO),
                item(opp(close), normal, 1, LIMIT, GTC),
            ]),
        );
        assert_eq!(tags(&out), vec![(TAG_REJECTED, R_OTHER), (TAG_ACCEPTED, R_NONE)]);
        assert_eq!(ro_count(&mut ctx, ALICE), RO_CAP);
    }
}

/// `asSubAccount`: the cap is the SUB-account's own (ALICE's ten do not use it up), for a wrapped
/// `placeOrder` and a wrapped batch alike.
#[test]
fn ro_cap_as_sub_account() {
    for close in CLOSES {
        let mut ctx = fresh();
        let (out, _) = ok(&mut ctx, ALICE, &createSubAccountCall { name: "desk".into() }.abi_encode());
        let sub = createSubAccountCall::abi_decode_returns(&out.bytes).unwrap();
        set_wallet(&mut ctx, sub, WALLET);
        open_pos(&mut ctx, ALICE, close, 100);
        fill_ro(&mut ctx, ALICE, close, RO_CAP);
        let wrap = |data: Vec<u8>| asSubAccountCall { subAccount: sub, data: data.into() }.abi_encode();
        rest(&mut ctx, CAROL, close, MARK, 50);
        ok(&mut ctx, ALICE, &wrap(limit(opp(close), MARK, 50, IOC, 0)));
        assert_eq!(storage::load_position(&mut ctx, sub, MARKET_ID).unwrap().amount.unsigned_abs(), 50);
        for _ in 0..9 {
            let (_, logs) = ok(&mut ctx, ALICE, &wrap(limit(close, far(close), 1, GTC, RO)));
            assert_eq!(evs::<OrderRested>(&logs).len(), 1);
        }
        assert_eq!(ro_count(&mut ctx, sub), 9);
        let (out, _) = ok(
            &mut ctx,
            ALICE,
            &wrap(batch(vec![
                ro_item(close, far(close), 1, LIMIT, GTC),
                ro_item(close, far(close), 1, LIMIT, GTC),
            ])),
        );
        assert_eq!(tags(&out), vec![(TAG_ACCEPTED, R_NONE), (TAG_REJECTED, R_OTHER)]);
        rejected(&mut ctx, ALICE, &wrap(limit(close, far(close), 1, GTC, RO)), E_RO);
        assert_eq!((ro_count(&mut ctx, sub), ro_count(&mut ctx, ALICE)), (RO_CAP, RO_CAP));
    }
}
