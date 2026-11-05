use super::*;
use crate::interface::IPerpDex::{getPositionRiskCall, OrderCancelled, OrderRested, Trade};
use std::collections::HashMap;

/// `cancelOrder` through the real handler.
fn cancel(ctx: &mut TestCtx, caller: Address, id: [u8; 32]) {
    run_cancel_order(
        &cancelOrderCall {
            orderId: id.into(),
            marketId: MARKET_ID,
        }
        .abi_encode(),
        caller,
        ctx,
    )
    .unwrap();
}

/// The probe market: `base_decimals 5`, `price_decimals 2`, step 1, so
/// `calc_value(price, qty) == price × qty / 10` and — because every price used below is a
/// multiple of 10 — every term is EXACT. That matters: the aggregates floor PER ORDER, so a
/// fixture with inexact terms would let a Σ agree for the wrong reason (or disagree by 1 ulp
/// for a reason that is not this field's fault).
///
/// `price_band_bps` is set WIDE (10_000 disables the lower bound) on purpose. The default 1_000
/// would put `mark_lower` at exactly 90_000 with a 100_000 mark, and the below-mark sell this
/// fixture needs sits exactly there — passing only because the band test is a strict `<`. This
/// module is about notional accounting, not about the band, so it does not want to be one tick
/// away from testing the band instead.
fn setup_notional_market(ctx: &mut TestCtx) {
    storage::save_admin(ctx, ADMIN).unwrap();
    storage::save_market(
        ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 5,
            price_decimals: 2,
            tick_size: 10,
            step_size: 1,
            min_quantity: 1,
            max_quantity: 1_000_000,
            max_price: 100_000_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 10_000,
            mark_price: 100_000,
            tiers: MarginTiers::default(),
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
        },
    )
    .unwrap();
    // Generously, so nothing in here is ever an admission or a maker-cover story. A wallet that
    // bound would turn a notional-accounting test into a margin test.
    fund(ctx, ALICE, 10_000_000);
    fund(ctx, BOB, 10_000_000);
}

/// One resting order EXACTLY as an indexer holds it: the basis frozen from `OrderRested`, the
/// quantity maintained from the stream. Nothing here is read from storage.
#[derive(Clone, Copy, Debug)]
struct Resting {
    side: u8,
    qty: u64,
    assuming_price: u64,
}

/// The indexer. Its ONLY input is the log stream — this is the point of the test, so there is
/// deliberately no `ctx` in reach of `apply`.
#[derive(Default)]
struct Indexer {
    open: HashMap<[u8; 32], Resting>,
}

impl Indexer {
    /// Feed a drained batch of logs. Three rules, which is the whole projector:
    ///
    /// * `OrderRested` → the order is live at `(quantity, assumingPrice)`.
    /// * `Trade` → if we hold the MAKER order, its resting quantity shrinks by the fill; at 0
    ///   it is gone. The Assuming Price is NOT touched — that is R12, and it is why the event
    ///   carries the BASIS rather than the already-multiplied contribution.
    /// * `OrderCancelled` → gone.
    fn apply(&mut self, logs: &[primitives::Log], subject: Address) {
        for log in logs {
            let Some(topic0) = log.data.topics().first().copied() else {
                continue;
            };
            if topic0 == OrderRested::SIGNATURE_HASH {
                let e = OrderRested::decode_raw_log(log.data.topics(), &log.data.data).unwrap();
                if e.user != subject {
                    continue;
                }
                self.open.insert(
                    e.orderId.0,
                    Resting {
                        side: e.side,
                        qty: e.quantity,
                        assuming_price: e.assumingPrice,
                    },
                );
            } else if topic0 == Trade::SIGNATURE_HASH {
                let e = Trade::decode_raw_log(log.data.topics(), &log.data.data).unwrap();
                if let Some(r) = self.open.get_mut(&e.makerOrderId.0) {
                    r.qty = r.qty.checked_sub(e.quantity).expect(
                        "a fill cannot exceed the resting quantity the stream reported",
                    );
                    if r.qty == 0 {
                        self.open.remove(&e.makerOrderId.0);
                    }
                }
            } else if topic0 == OrderCancelled::SIGNATURE_HASH {
                let e =
                    OrderCancelled::decode_raw_log(log.data.topics(), &log.data.data).unwrap();
                self.open.remove(&e.orderId.0);
            }
        }
    }

    /// `Σ calc_value(assumingPrice, qty)` over one side — the per-order floor applied per order,
    /// which is how the engine's own aggregate fold applies it.
    fn side_notional(&self, side: u8) -> u64 {
        self.open
            .values()
            .filter(|r| r.side == side)
            .map(|r| crate::math::calc_value(r.assuming_price, r.qty, 5, 2).unwrap())
            .sum()
    }

    fn get(&self, id: &[u8; 32]) -> Resting {
        *self.open.get(id).expect("order should still be resting")
    }
}

/// `getPositionRisk(user, MARKET_ID)`'s `(bidNotional, askNotional)`, read through the REAL
/// selector — calldata in, ABI-decoded return out — not through `compute_margin_info`. The
/// claim being tested is about what a consumer can poll, so it is polled.
fn reported_notionals(ctx: &mut TestCtx, user: Address) -> (u64, u64) {
    let out = crate::margin::view::run_get_position_risk(
        &getPositionRiskCall {
            user,
            marketId: MARKET_ID,
        }
        .abi_encode(),
        ctx,
    )
    .unwrap();
    let row = getPositionRiskCall::abi_decode_returns(&out).unwrap();
    (row.bidNotional, row.askNotional)
}

/// THE TEST. Six resting orders across both sides at five distinct prices, including the two
/// cases that make the field non-trivial:
///
/// * a SELL whose Assuming Price EXCEEDS its limit (resting below the mark), so the field is not
///   a copy of `price`; and
/// * two PARTIALLY FILLED resting orders, one that partially filled ON PLACEMENT and one that
///   rested whole and was partially filled LATER — after which the mark is moved, so a
///   re-resolution of `T` would give a different answer and the frozen one is the only one that
///   still closes the Σ.
///
/// The Σ is asserted after EVERY step, not just at the end: a projector that is wrong only in
/// some intermediate state is a projector that is wrong.
#[test]
fn resting_notional_sum_from_the_event_stream_equals_get_position_risk() {
    let mut ctx = make_ctx();
    setup_notional_market(&mut ctx);
    let mut ix = Indexer::default();

    // Drain whatever setup emitted, then keep the stream and the projector in lock-step.
    let _ = JournalTr::take_logs(ctx.journal_mut());
    let step = |ctx: &mut TestCtx, ix: &mut Indexer| {
        let logs = JournalTr::take_logs(ctx.journal_mut());
        ix.apply(&logs, ALICE);
        let (bid, ask) = reported_notionals(ctx, ALICE);
        assert_eq!(
            (ix.side_notional(0), ix.side_notional(1)),
            (bid, ask),
            "the stream-derived Σ must equal what getPositionRisk reports"
        );
        (bid, ask)
    };

    // ── 1. Two plain resting BUYS at different prices. A buy's Assuming Price IS its limit
    //       (no markup on the buy side — MEASURED, R11), and the field says so rather than
    //       leaving the consumer to remember the asymmetry.
    let bid_a = place(&mut ctx, ALICE, 0, 50_000, 6, 0, 0); // 50_000 × 6 / 10 = 30_000
    let bid_b = place(&mut ctx, ALICE, 0, 40_000, 3, 0, 0); // 40_000 × 3 / 10 = 12_000
    let (bid, ask) = step(&mut ctx, &mut ix);
    assert_eq!((bid, ask), (42_000, 0));
    assert_eq!(ix.get(&bid_a).assuming_price, 50_000, "buy: no markup");
    assert_eq!(ix.get(&bid_b).assuming_price, 40_000, "buy: no markup");

    // ── 2. A BUY that PARTIALLY FILLS ON PLACEMENT and rests its remainder. `OrderRested`
    //       reports the resting 6, not the submitted 10, so the Σ needs no correction here —
    //       and the fill drags `lastTraded` to 60_000, which is what makes step 3 interesting.
    place(&mut ctx, BOB, 1, 60_000, 4, 0, 0); // BOB's ask, 4 available
    let _ = JournalTr::take_logs(ctx.journal_mut()); // BOB's own rest is not ALICE's
    let bid_c = place(&mut ctx, ALICE, 0, 60_000, 10, 0, 0); // fills 4, rests 6
    let (bid, ask) = step(&mut ctx, &mut ix);
    assert_eq!(
        ix.get(&bid_c).qty,
        6,
        "the RESTING quantity, not the 10 sent"
    );
    assert_eq!(ix.get(&bid_c).assuming_price, 60_000);
    assert_eq!((bid, ask), (42_000 + 36_000, 0)); // + 60_000 × 6 / 10
    assert_eq!(
        storage::load_last_traded_price(&mut ctx, MARKET_ID).unwrap(),
        60_000
    );
    assert_eq!(
        pos(&mut ctx, ALICE).amount,
        4,
        "and she is long 4 at 60_000"
    );

    // ── 3. THE ABOVE-LIMIT SELL. `T = max(⌈60_000 × 1.0015⌉, mark 100_000) = 100_000`, so a
    //       sell resting at 90_000 is charged at 100_000 — the field is NOT a copy of `price`,
    //       which is the case that makes it un-derivable from the rest of the event.
    let ask_a = place(&mut ctx, ALICE, 1, 90_000, 5, 0, 0);
    // ── and a control SELL above `T`, where `max(T, limit)` picks the limit instead. Both
    //       branches of the same `max` are therefore exercised in one fixture.
    let ask_b = place(&mut ctx, ALICE, 1, 120_000, 2, 0, 0);
    let (bid, ask) = step(&mut ctx, &mut ix);
    assert_eq!(
        (ix.get(&ask_a).assuming_price, ix.get(&ask_b).assuming_price),
        (100_000, 120_000),
        "the below-mark sell is charged at T, the above-mark one at its own limit"
    );
    assert!(
        ix.get(&ask_a).assuming_price > 90_000,
        "the whole point: for this order the field is NOT the limit price"
    );
    assert_eq!((bid, ask), (78_000, 50_000 + 24_000)); // 100_000×5/10 + 120_000×2/10

    // ── 4. THE FREEZE (R12). The resting sell is partially filled LATER, and then the mark
    //       MOVES. A re-resolution of `T` would now give 150_000 — so if anything re-derived
    //       the basis, `askNotional` would read 150_000 × 3 / 10 = 45_000 for this order
    //       instead of 100_000 × 3 / 10 = 30_000, and the stream Σ (which cannot see a mark
    //       move at all) would part from the view. It does not.
    place(&mut ctx, BOB, 0, 90_000, 2, 0, 0); // BOB lifts 2 of ALICE's 5
    let (bid, ask) = step(&mut ctx, &mut ix);
    assert_eq!(ix.get(&ask_a).qty, 3, "3 of the 5 still rest");
    assert_eq!(
        ix.get(&ask_a).assuming_price,
        100_000,
        "a partial fill must NOT re-resolve the Assuming Price"
    );
    assert_eq!((bid, ask), (78_000, 30_000 + 24_000));

    storage::save_mark_price(&mut ctx, MARKET_ID, 150_000).unwrap();
    let (bid_after, ask_after) = step(&mut ctx, &mut ix);
    assert_eq!(
        (bid_after, ask_after),
        (bid, ask),
        "a mark move cannot touch Bid/Ask — every basis is frozen (R12). If this fails, the \
         notional is being re-derived from the live mark somewhere."
    );

    // ── 5. Cancel one of each side and confirm the projector tracks removals too, so the
    //       agreement above is not an artefact of a monotonically growing book.
    cancel(&mut ctx, ALICE, bid_b);
    cancel(&mut ctx, ALICE, ask_b);
    let (bid, ask) = step(&mut ctx, &mut ix);
    assert_eq!((bid, ask), (78_000 - 12_000, 30_000));

    // ── 6. NON-TRIVIALITY. Everything above would also hold if the field were always `price`
    //       and every quantity were the submitted one. It is not, and these are the two
    //       witnesses — asserted against the STORE, so the event's numbers are pinned to state
    //       rather than merely to each other.
    let entry = storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID)
        .unwrap()
        .into_iter()
        .find(|e| e.order_id == ask_a)
        .expect("the partially filled sell is still in the book");
    assert_eq!(
        (entry.price, entry.assuming_price, entry.amount),
        (90_000, 100_000, 3),
        "the stored entry: limit 90_000, basis 100_000, 3 left — and the event reported all \
         three of those"
    );
    assert_eq!(
        (
            ix.get(&ask_a).qty,
            ix.get(&ask_a).assuming_price,
            entry.price
        ),
        (entry.amount, entry.assuming_price, 90_000)
    );
}
