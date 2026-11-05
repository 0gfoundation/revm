use super::*;

/// $150 — the size-weighted average of a $100 fill and a $200 fill of equal size.
const AVG_PRICE: u64 = 150 * TICK;

/// The LAST `PositionChanged` this call emitted for `user`. Every field on the event is an
/// after-image, so for a multi-fill call this is the settled row an indexer would put in
/// `P[]` (see the ABI note: last-one-wins per `(user, marketId)`).
fn last_change(
    ctx: &mut TestCtx,
    user: Address,
) -> crate::interface::IPerpDex::PositionChanged {
    take_position_changes(ctx)
        .into_iter()
        .rfind(|c| c.user == user)
        .expect("expected at least one PositionChanged")
}

/// Drive the shared derivation directly on a hand-built position at a hand-set mark. This is
/// the same `crate::events::emit_position_changed` all seven production sites go through, so
/// what it reports here is what they report — without a fixture that has to keep a matching
/// engine happy at every mark under test.
fn emit_at(
    ctx: &mut TestCtx,
    amount: i64,
    v_quote: i64,
    margin: i64,
    mark: u64,
) -> crate::interface::IPerpDex::PositionChanged {
    storage::save_position(
        ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance: v_quote,
            margin,
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    set_mark(ctx, mark);
    let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    let p = pos(ctx, ALICE);
    // The account header the production sites emit before their position row. Without it this
    // helper would publish an ORPHAN `PositionChanged`, which the group guard in `crate::events`
    // rejects — correctly: the derivation under test is only ever reached from inside a group.
    storage::publish_account_snapshot_now(ctx, ALICE, AccountUpdateReason::Adjustment).unwrap();
    crate::events::emit_position_changed(ctx, ALICE, &market, &p, 0, 0).unwrap();
    last_change(ctx, ALICE)
}

// ── `ep` ─────────────────────────────────────────────────────────────────────────────────

/// `P[].ep` is specified as `"0"` when flat, and a full close is how a position gets there on
/// the REAL path — so this drives an actual open-then-close through the book rather than
/// hand-setting `amount = 0`.
#[test]
fn a_full_close_reports_a_flat_position_with_a_zero_entry_price() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);

    // ALICE opens a long, then sells the whole thing back into CAROL's bid.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    place(&mut ctx, CAROL, 0, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);

    let flat = last_change(&mut ctx, ALICE);
    assert_eq!(
        pos(&mut ctx, ALICE).amount,
        0,
        "fixture: ALICE must be flat"
    );
    assert_eq!(flat.amount, 0);
    assert_eq!(
        flat.entryPrice, 0,
        "a flat position has no entry price — `P[].ep` must be 0"
    );
    assert_eq!(
        flat.unrealizedProfit, 0,
        "nothing open, nothing to mark to market"
    );
}

/// **The average, not the last fill.** ALICE opens `QTY` at $100 and `QTY` at $200, then
/// closes HALF at $500. `entryPrice` must read $150 throughout — the size-weighted average of
/// the two opens — and specifically must not be $200 (the last opening fill) or $500 (the
/// close). A partial close scales `v_quote_balance` proportionally
/// (`settlement::apply_position_fill`), which is exactly why the average survives it.
#[test]
fn entry_price_after_a_partial_close_is_the_average_not_the_last_fill() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);
    let dave = user_addr(9);
    fund(&mut ctx, dave, WALLET);

    // Open leg 1: QTY @ $100.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert_eq!(last_change(&mut ctx, ALICE).entryPrice, PRICE);

    // Open leg 2: QTY @ $200 → 2×QTY long, average entry $150.
    place(&mut ctx, CAROL, 1, 200 * TICK, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, 200 * TICK, QTY, 0, 0);
    let after_second_open = last_change(&mut ctx, ALICE);
    assert_eq!(after_second_open.amount, 2 * QTY as i64);
    assert_eq!(
        after_second_open.entryPrice, AVG_PRICE,
        "two equal-size fills at $100 and $200 average to $150"
    );

    // Close HALF at $500 — far from both opens, so a "last fill price" bug is unmissable.
    place(&mut ctx, dave, 0, 500 * TICK, QTY, 0, 0);
    place(&mut ctx, ALICE, 1, 500 * TICK, QTY, 0, 0);

    let after_close = last_change(&mut ctx, ALICE);
    assert_eq!(after_close.amount, QTY as i64, "fixture: half still open");
    assert_eq!(after_close.closedQuantity, QTY);
    assert_eq!(
        after_close.entryPrice, AVG_PRICE,
        "a partial close must not move the entry price"
    );
    assert_ne!(
        after_close.entryPrice,
        200 * TICK,
        "entryPrice is the average, not the last OPENING fill price"
    );
    assert_ne!(
        after_close.entryPrice,
        500 * TICK,
        "entryPrice is the average, not the CLOSING fill price"
    );
}

// ── `up` ─────────────────────────────────────────────────────────────────────────────────

/// A LONG is in profit above its entry and in loss below it. Position: `QTY` long at $100
/// (`v_quote = -FILL_VALUE`); at $150 the notional is `1.5 × FILL_VALUE`, so
/// `up = +0.5 × FILL_VALUE`.
#[test]
fn unrealised_pnl_sign_is_positive_for_a_long_above_entry_and_negative_below() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let (amount, v_quote) = (QTY as i64, -(FILL_VALUE as i64));

    let above = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, AVG_PRICE);
    assert_eq!(above.entryPrice, PRICE);
    assert_eq!(above.unrealizedProfit, FILL_VALUE as i64 / 2);
    assert!(above.unrealizedProfit > 0, "long above entry is in PROFIT");

    let below = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, 50 * TICK);
    assert_eq!(below.unrealizedProfit, -(FILL_VALUE as i64 / 2));
    assert!(below.unrealizedProfit < 0, "long below entry is in LOSS");

    let at_entry = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, PRICE);
    assert_eq!(at_entry.unrealizedProfit, 0, "mark == entry ⟹ up == 0");
}

/// A SHORT is the mirror: in profit BELOW its entry. Position: `QTY` short at $100
/// (`v_quote = +FILL_VALUE`, `amount` negative), so the signed notional is negative and shrinks
/// in magnitude as the mark falls.
#[test]
fn unrealised_pnl_sign_is_positive_for_a_short_below_entry_and_negative_above() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let (amount, v_quote) = (-(QTY as i64), FILL_VALUE as i64);

    let below = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, 50 * TICK);
    assert_eq!(
        below.entryPrice, PRICE,
        "a short's entry price is the same positive price as a long's"
    );
    assert_eq!(below.unrealizedProfit, FILL_VALUE as i64 / 2);
    assert!(below.unrealizedProfit > 0, "short below entry is in PROFIT");

    let above = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, AVG_PRICE);
    assert_eq!(above.unrealizedProfit, -(FILL_VALUE as i64 / 2));
    assert!(above.unrealizedProfit < 0, "short above entry is in LOSS");
}

/// `up` is mark-to-market: the SAME stored position reports a different `up` at every mark,
/// moving one-for-one with `amount × Δmark`. This is the whole reason the field is on the
/// event — with the `@position` stream gone, an indexer has no other way to fill it.
#[test]
fn unrealised_pnl_tracks_a_mark_move() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let (amount, v_quote) = (QTY as i64, -(FILL_VALUE as i64));

    let mut seen = Vec::new();
    for mark in [
        PRICE,
        PRICE + 10 * TICK,
        PRICE + 20 * TICK,
        PRICE - 40 * TICK,
    ] {
        let e = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, mark);
        // Stored state never moved, so `pa`/`ep` are constant and only `up` responds.
        assert_eq!((e.amount, e.entryPrice), (amount, PRICE));
        seen.push(e.unrealizedProfit);
    }
    // calc_value(10 * TICK, QTY, 8, 9) = 100_000 quote units per $10 of mark move.
    let per_ten_dollars = FILL_VALUE as i64 / 10;
    assert_eq!(
        seen,
        vec![
            0,
            per_ten_dollars,
            2 * per_ten_dollars,
            -4 * per_ten_dollars
        ],
        "up must move one-for-one with the mark on an unchanged position"
    );
}

// ── `cumulativeRealizedPnl` (`P[].cr`) ───────────────────────────────────────────────────

/// `cr` on its simplest shape: ONE close, so the lifetime total is that close's own
/// `realizedPnl` — the level and the delta coincide exactly once in a position's life and this
/// is that moment. Every test below is about the cases where they do NOT.
///
/// The fixture pays non-zero commissions on both sides on purpose: `cr` excludes fees (as
/// `realizedPnl` does), so a `cr` that quietly netted them would fail here rather than as a
/// drift in a consumer's chart.
#[test]
fn cumulative_realized_pnl_equals_the_single_close_that_produced_it() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);
    for user in [ALICE, BOB, CAROL] {
        storage::save_user_fee_rates(
            &mut ctx,
            user,
            UserFeeRates {
                maker_fee_ppm: 200,
                taker_fee_ppm: 500,
            },
        )
        .unwrap();
    }

    // Open QTY long at $100, then close HALF at $200 → a realised profit plus four fee legs.
    place(&mut ctx, BOB, 1, PRICE, 2 * QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, 2 * QTY, 0, 0);
    place(&mut ctx, CAROL, 0, 200 * TICK, QTY, 0, 0);
    place(&mut ctx, ALICE, 1, 200 * TICK, QTY, 0, 0);

    let e = last_change(&mut ctx, ALICE);
    assert!(
        e.realizedPnl > 0 && e.closedQuantity == QTY,
        "fixture must actually realise PnL, else this proves nothing: realizedPnl={} \
         closedQuantity={}",
        e.realizedPnl,
        e.closedQuantity
    );
    assert!(market_fee_total(&mut ctx) > 0, "fixture must pay fees");
    assert_eq!(
        e.cumulativeRealizedPnl, e.realizedPnl,
        "one close ⇒ the lifetime total IS that close's PnL"
    );
    // The value, not just the relation — closing QTY of a $100 long at $200 realises $1.
    assert_eq!(e.cumulativeRealizedPnl, 1_000_000);
    // …and it is the STORED field, not something the emit site computed on the way past.
    assert_eq!(pos(&mut ctx, ALICE).cumulative_realized_pnl, 1_000_000);
}

/// **THE PROPERTY `cr` EXISTS FOR: it survives the position going FLAT and being REOPENED.**
///
/// This is the whole reason the number has to be stored rather than derived. A reopened
/// position's `(amount, vQuoteBalance, margin)` carry no trace of the round before it — that is
/// what "flat" means — so a `cr` reconstructed from current state would restart at 0 on every
/// reopen, and a consumer that missed one `PositionChanged` could never recover the total.
///
/// Two rounds, deliberately of DIFFERENT magnitudes and OPPOSITE signs, so the final value is
/// distinguishable from every wrong answer:
///
/// ```text
///                              ALICE (taker)          BOB (maker)
///   round 1  open  @ $100        cr = 0                 cr = 0        (opening ⇒ 0)
///            close @ $110        cr = +100_000          cr = −100_000
///            ── FLAT ──          cr = +100_000          cr = −100_000  ← must NOT reset
///   round 2  open  @ $100        cr = +100_000          cr = −100_000  (opening ⇒ 0)
///            close @  $85        cr =  −50_000          cr = +50_000
/// ```
///
/// A `cr` zeroed on close would report `−150_000` / `+150_000` at the end; one zeroed on reopen
/// the same; one that saturated or took an absolute value would differ too. And the flat-interval
/// assertion is read off STORED STATE, not off the event, so it pins the field surviving in the
/// blob rather than merely being echoed once.
///
/// Both parties are checked because they exercise different accumulation paths for the same
/// arithmetic: ALICE closes as the TAKER (`finalize_core` on the summed legs), BOB as the MAKER
/// (`settle_maker_fill_core`, per fill).
#[test]
fn cumulative_realized_pnl_survives_a_close_and_a_reopen() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    // ── round 1: open QTY long at $100, close it at $110 ──
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert_eq!(
        (
            pos(&mut ctx, ALICE).cumulative_realized_pnl,
            pos(&mut ctx, BOB).cumulative_realized_pnl
        ),
        (0, 0),
        "an OPENING fill realises nothing"
    );

    place(&mut ctx, BOB, 0, 110 * TICK, QTY, 0, 0);
    place(&mut ctx, ALICE, 1, 110 * TICK, QTY, 0, 0);
    let (alice_flat, bob_flat) = (pos(&mut ctx, ALICE), pos(&mut ctx, BOB));
    assert_eq!(
        (alice_flat.amount, bob_flat.amount),
        (0, 0),
        "the fixture must actually go FLAT, or the survival claim below is untested"
    );
    assert_eq!(
        (alice_flat.margin, bob_flat.margin),
        (0, 0),
        "and a flat position holds no margin (save_position's own invariant)"
    );
    // ⚠️ THE FLAT INTERVAL. Read off the STORED position, so this is the blob keeping the value
    // and not the event echoing it.
    assert_eq!(
        (
            alice_flat.cumulative_realized_pnl,
            bob_flat.cumulative_realized_pnl
        ),
        (100_000, -100_000),
        "`cr` is deliberately NOT zeroed with `amount`/`margin` — a flat position carrying a \
         non-zero lifetime total is the FEATURE"
    );

    // ── round 2: reopen QTY long at $100, close it at $85 (a bigger LOSS) ──
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert_eq!(
        (
            pos(&mut ctx, ALICE).cumulative_realized_pnl,
            pos(&mut ctx, BOB).cumulative_realized_pnl
        ),
        (100_000, -100_000),
        "reopening carries round 1's total forward — it does not restart"
    );

    place(&mut ctx, BOB, 0, 85 * TICK, QTY, 0, 0);
    place(&mut ctx, ALICE, 1, 85 * TICK, QTY, 0, 0);

    // ── the running total across BOTH rounds ──
    // ALICE: +100_000 (round 1) − 150_000 (round 2) = −50_000.
    // BOB:   −100_000 (round 1) + 150_000 (round 2) = +50_000.
    assert_eq!(
        (
            pos(&mut ctx, ALICE).cumulative_realized_pnl,
            pos(&mut ctx, BOB).cumulative_realized_pnl
        ),
        (-50_000, 50_000),
        "the total spans the flat interval; neither round's value alone is right"
    );
    // The last `PositionChanged` each party got must publish the same number — the event and
    // the state are one field, not two.
    assert_eq!(
        last_change(&mut ctx, ALICE).cumulativeRealizedPnl,
        -50_000,
        "the event publishes the stored total"
    );
}

/// **THE ACCEPTANCE IDENTITY, for BOTH sides:**
///
/// ```text
///   Σ (that user's per-fill realizedPnl from `Trade`)  ==  PositionChanged.cumulativeRealizedPnl
/// ```
///
/// Two INDEPENDENTLY produced fields have to agree — the per-fill split is apportioned for
/// logging by `finalize_core`'s telescoping loop (taker) or is the fill's own value
/// (`makerRealizedPnl`), while `cr` is folded onto the position by `apply_position_fill` from
/// the AGGREGATE. Nothing computes one from the other. This is the technique R15 used to
/// confirm on mainnet that the per-fill split was real rather than a formatting artefact
/// (`cr` was the rolling sum of `rp`, 10/10 exact), applied to our own stream.
///
/// # The scenario is built so the agreement cannot be about one positive number
///
/// ```text
///   phase A   BOB rests SELL 1@$102, 1@$100;  CAROL rests SELL 1@$101
///             ALICE market BUY 3   → three fills, all PURE OPENING on every party ⇒ rp = 0
///   phase B   BOB rests BUY 1@$103, 1@$95;    CAROL rests BUY 3@$98
///             ALICE market SELL 5  → sweeps $103(1), $98(3), $95(1)
/// ```
///
/// What each party's Σ therefore contains:
///
/// * **ALICE (taker)** — long 3 at `vq = −3_030_000`, sells 5: closes 3 and FLIPS to short 2.
///   Per-fill `+20_000`, `−60_000`, `0` — BOTH SIGNS, plus a fill that is pure opening and must
///   report exactly 0. Σ = `−40_000`, a NEGATIVE lifetime total.
/// * **BOB (maker)** — short 2 at `vq = +2_020_000`, bought back one lot ABOVE his basis
///   ($103, a `−20_000` LOSS) and one well below ($95, a `+60_000` gain). Σ = `+40_000`, so
///   neither of his two fills' values is the answer and a sign error on either fails.
/// * **CAROL (maker)** — short 1 at $101, her single 3-lot fill closes that lot at $98
///   (`+30_000`) and opens 2 LONG in the same fill: **a maker-side FLIP**, whose opening leg
///   must contribute nothing.
///
/// Commissions are non-zero on every party, so the identity also pins that `cr` is FEE-FREE:
/// fees are charged out of the same fills and appear in neither side of the equation.
#[test]
fn per_fill_trade_realized_pnl_sums_to_cumulative_realized_pnl_on_both_sides() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);
    // Maker rates are snapshotted onto the order entry AT PLACEMENT, so they go in first.
    for user in [ALICE, BOB, CAROL] {
        storage::save_user_fee_rates(
            &mut ctx,
            user,
            UserFeeRates {
                maker_fee_ppm: 500,
                taker_fee_ppm: 1_000,
            },
        )
        .unwrap();
    }

    // Accumulate the logs of EVERY call: `take_logs` drains, so a per-call snapshot appended
    // here is the only way to see phase A's fills and phase B's together.
    let mut logs: Vec<primitives::Log> = Vec::new();
    macro_rules! collect {
        () => {
            logs.extend(JournalTr::take_logs(ctx.journal_mut()))
        };
    }

    // ── phase A: everyone opens (rp = 0 on every leg of every party) ──
    place(&mut ctx, BOB, 1, 102 * TICK, QTY, 0, 0);
    place(&mut ctx, CAROL, 1, 101 * TICK, QTY, 0, 0);
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    collect!();
    place(&mut ctx, ALICE, 0, 0, 3 * QTY, 1, 1); // market buy 3
    end_call(&mut ctx);
    collect!();

    // ── phase B: ALICE closes 3 and flips to short 2; the makers close (and CAROL flips) ──
    place(&mut ctx, BOB, 0, 103 * TICK, QTY, 0, 0);
    place(&mut ctx, BOB, 0, 95 * TICK, QTY, 0, 0);
    place(&mut ctx, CAROL, 0, 98 * TICK, 3 * QTY, 0, 0);
    collect!();
    place(&mut ctx, ALICE, 1, 0, 5 * QTY, 1, 1); // market sell 5
    end_call(&mut ctx);
    collect!();

    let trades = decode_trades(&logs);
    assert_eq!(
        trades.len(),
        6,
        "3 opening fills + 3 closing fills; got {:?}",
        trades.iter().map(|t| t.price).collect::<Vec<_>>()
    );
    assert_eq!(
        trades.iter().map(|t| t.price).collect::<Vec<_>>(),
        vec![
            PRICE,
            101 * TICK,
            102 * TICK, // buy taker sweeps the book from the BOTTOM
            103 * TICK,
            98 * TICK,
            95 * TICK, // sell taker sweeps it from the TOP
        ],
        "the sweep order is what makes the per-fill values below the ones they are"
    );

    /// `Σ` of a user's own per-fill realised PnL over the whole stream: the TAKER column where
    /// they took, the MAKER column where they were hit. One user can appear in both, which is
    /// exactly why the projection has to be per-row rather than per-event.
    fn sum_fill_rp(trades: &[crate::interface::IPerpDex::Trade], user: Address) -> i64 {
        trades
            .iter()
            .map(|t| {
                (if t.taker == user {
                    t.takerRealizedPnl
                } else {
                    0
                }) + (if t.maker == user {
                    t.makerRealizedPnl
                } else {
                    0
                })
            })
            .sum()
    }

    // ── the per-fill values, pinned ────────────────────────────────────────────────────────
    //
    // ALICE, long 3 at vq = −3_030_000 (= −(1_000_000 + 1_010_000 + 1_020_000)), selling 5:
    //   pos_abs = 3_000_000, closing_qty = 3_000_000
    //   $103 q1  rem_q 2_000_000  rem_vq −2_020_000  vqf −1_010_000  +1_030_000 →  +20_000
    //   $98  q3  rem_q         0  rem_vq          0  vqf −2_020_000  +1_960_000 →  −60_000
    //   $95  q1  pure OPENING (remaining_closing_qty already 0)               →        0
    assert_eq!(
        trades
            .iter()
            .map(|t| t.takerRealizedPnl)
            .collect::<Vec<_>>(),
        vec![0, 0, 0, 20_000, -60_000, 0],
        "taker column: phase A is all opening; phase B mixes signs and ends on a pure open"
    );
    // BOB's two closing fills ($103 a loss, $95 a gain) and CAROL's flip fill ($98).
    assert_eq!(
        trades
            .iter()
            .map(|t| t.makerRealizedPnl)
            .collect::<Vec<_>>(),
        vec![0, 0, 0, -20_000, 30_000, 60_000],
        "maker column: BOB loses on the $103 buy-back and gains on the $95 one"
    );

    // ── THE IDENTITY, per party ────────────────────────────────────────────────────────────
    let changes = decode_position_changes(&logs);
    for (user, label, expected) in [
        (
            ALICE,
            "ALICE (taker, flips short, NEGATIVE total)",
            -40_000i64,
        ),
        (BOB, "BOB (maker, one losing + one winning close)", 40_000),
        (CAROL, "CAROL (maker, flips long in a single fill)", 30_000),
    ] {
        let last = changes
            .iter()
            .rfind(|c| c.user == user)
            .unwrap_or_else(|| panic!("{label}: expected a PositionChanged"));
        assert_eq!(
            sum_fill_rp(&trades, user),
            last.cumulativeRealizedPnl,
            "{label}: Σ per-fill Trade realizedPnl must equal the published lifetime total"
        );
        // The VALUE too, so a derivation broken IDENTICALLY on both sides still fails.
        assert_eq!(last.cumulativeRealizedPnl, expected, "{label}");
        // And the event agrees with what is in the blob.
        assert_eq!(
            storage::load_position(&mut ctx, user, MARKET_ID)
                .unwrap()
                .cumulative_realized_pnl,
            expected,
            "{label}: stored"
        );
    }

    // ── the fixture really is non-trivial ──────────────────────────────────────────────────
    assert!(
        trades.iter().any(|t| t.takerRealizedPnl < 0)
            && trades.iter().any(|t| t.makerRealizedPnl < 0),
        "a NEGATIVE close must be present on both columns"
    );
    // The flips: ALICE ends SHORT having been long, CAROL ends LONG having been short.
    let (alice, carol) = (pos(&mut ctx, ALICE), pos(&mut ctx, CAROL));
    assert!(
        alice.amount < 0 && carol.amount > 0,
        "both flips must have happened: alice {} carol {}",
        alice.amount,
        carol.amount
    );
    assert!(
        market_fee_total(&mut ctx) > 0,
        "fees must be non-zero, so the identity also says `cr` is fee-free"
    );
}

// ── the remaining placeholder ────────────────────────────────────────────────────────────

/// ⚠️ **THIS TEST MUST BE UPDATED — not deleted — WHEN `breakevenPrice` IS POPULATED.**
///
/// `breakevenPrice` (`P[].bep`) is carried so the payload shape is stable (the precedent the
/// public docs set for `ACCOUNT_UPDATE.a.m`) and is hardcoded 0 because the state it needs does
/// not exist: cumulative fees paid against the position. See the ABI comment on
/// `PositionChanged`.
///
/// (`cumulativeRealizedPnl` used to be asserted zero alongside it and is now POPULATED — see
/// the three tests above. It is asserted non-zero here so this test also witnesses that the
/// two fields are independent: `bep` being 0 is not a symptom of the payload being unwired.)
///
/// The fixture deliberately GENERATES the quantity — paid commissions on both sides — so the
/// assertion is "zero even though there is something real to report", not "zero because nothing
/// happened". When the field is wired, replace the `assert_eq!(.., 0)` with the true value; a
/// green run here after that change means the wiring never reached the event.
#[test]
fn the_breakeven_price_placeholder_is_zero() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);
    // Non-zero fees on both sides, so `bep` would differ from `ep` if it were populated.
    for user in [ALICE, BOB, CAROL] {
        storage::save_user_fee_rates(
            &mut ctx,
            user,
            UserFeeRates {
                maker_fee_ppm: 200,
                taker_fee_ppm: 500,
            },
        )
        .unwrap();
    }

    // Open QTY long at $100, then close HALF at $200 → a realised profit plus four fee legs.
    place(&mut ctx, BOB, 1, PRICE, 2 * QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, 2 * QTY, 0, 0);
    place(&mut ctx, CAROL, 0, 200 * TICK, QTY, 0, 0);
    place(&mut ctx, ALICE, 1, 200 * TICK, QTY, 0, 0);

    let e = last_change(&mut ctx, ALICE);
    assert!(market_fee_total(&mut ctx) > 0, "fixture must pay fees");
    assert!(
        e.entryPrice > 0,
        "there must be an entry price for `bep` to be an adjustment OF"
    );
    assert_ne!(
        e.cumulativeRealizedPnl, 0,
        "the sibling field is populated — the zero below is `bep`'s own, not a dead payload"
    );
    assert_eq!(
        e.breakevenPrice, 0,
        "PLACEHOLDER: needs cumulative fees paid per position"
    );
}

// ── the funding path emits a position row ────────────────────────────────────────────────

/// Funding moves `pos.margin` = `P[].iw`. It used to do so with NO `PositionChanged` on some
/// paths, publishing an account update (`save_position` marks the snapshot dirty) whose `P[]`
/// was missing the very position whose margin moved. `apply_funding_settlement` now emits one,
/// paired with `FundingSettled` and valued at the same mark.
#[test]
fn a_funding_settlement_emits_a_position_row_carrying_the_new_margin() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    set_mark(&mut ctx, PRICE);

    // ALICE: QTY long at $100, margin INIT_MARGIN.
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let _ = take_position_changes(&mut ctx);

    // A funding index a long OWES against: index_delta > 0 ⟹ the long pays.
    storage::save_funding_state(
        &mut ctx,
        MARKET_ID,
        &FundingState {
            last_funding_rate: 1_000,
            next_funding_ts: u64::MAX,
            // The index accumulates `mark_price × funding_rate` per epoch, and
            // `calc_funding_payment` divides by `1e price_decimals · 1e base_decimals ·
            // FUNDING_RATE_ONE`. At mark $100 (100e9) and rate 1_000 (0.1%) this is one
            // epoch, so a QTY long owes `0.001 × FILL_VALUE = 1_000` quote units.
            cumulative_funding_index: PRICE as i128 * 1_000,
        },
    )
    .unwrap();

    // `addPositionMargin` is the cheapest way to touch the position and trigger the lazy
    // settle. It emits its OWN PositionChanged too, so take the FUNDING one: the first.
    let before = pos(&mut ctx, ALICE).margin;
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

    let changes: Vec<_> = take_position_changes(&mut ctx)
        .into_iter()
        .filter(|c| c.user == ALICE)
        .collect();
    assert_eq!(
        changes.len(),
        2,
        "expected the funding row then the margin-add row, got {} rows",
        changes.len()
    );
    let funding_row = &changes[0];
    let after = pos(&mut ctx, ALICE).margin;
    assert!(
        funding_row.margin < before,
        "the long must have PAID funding out of its margin: {before} -> {}",
        funding_row.margin
    );
    assert_eq!(
        funding_row.margin + 1,
        after,
        "the funding row's margin is the post-funding, pre-margin-add level"
    );
    // Same derivation as everywhere else: funding touches neither leg of `up`.
    assert_eq!(funding_row.entryPrice, PRICE);
    assert_eq!(funding_row.unrealizedProfit, 0, "mark == entry");
}

/// The path that had NO other position row at all: a maker whose fill is rejected as
/// open-into-insolvency (`MakerFillOutcome::Rejected`). Its funding is computed at
/// `MatchRegistry::get_or_load` and flushed to storage with everyone else's, but the walk
/// pushes no `PositionChanged` for it — so before the funding-path emit this transaction moved
/// the maker's `iw` and published an account update with no `P[]` entry for it.
#[test]
fn an_insolvency_rejected_maker_still_reports_its_funding_adjusted_margin() {
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 1_000_000); // band effectively disabled
    set_mark(&mut ctx, PRICE);
    fund(&mut ctx, CAROL, WALLET);

    // CAROL takes BOB's bid and holds a real SHORT, so she has margin for funding to bite.
    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0);
    place(&mut ctx, CAROL, 1, PRICE, QTY, 0, 0);
    assert_eq!(pos(&mut ctx, CAROL).amount, -(QTY as i64));

    // …and rests an ask at $1 while mark is $100. It must be an OPENING leg for the guard to
    // reach it — a pure close is always admitted — hence an ask (she is already short), and it
    // must be far enough below mark that the fill's own margin cannot carry the existing
    // position's buffer: filling would leave equity ~20_000 against a ~333_333 maintenance
    // requirement on 2×QTY, so the K9 guard cancels the order instead (same mechanism as
    // `maker_open_below_maintenance_is_cancelled_not_filled`, which uses a FLAT maker).
    let carol_ask = try_place_limit(&mut ctx, CAROL, 1, TICK).unwrap();
    let carol_ask: [u8; 32] = carol_ask[..32].try_into().unwrap();
    let _ = take_position_changes(&mut ctx);

    // NEGATIVE index so the SHORT is the side that pays (a positive rate credits a short).
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
    let before = pos(&mut ctx, CAROL).margin;

    // ALICE buys into CAROL's stranded ask: the fill is rejected, the order cancelled, and
    // CAROL's ONLY state change is the funding settle.
    let _ = try_place_limit(&mut ctx, ALICE, 0, TICK).unwrap();
    assert_terminal(&mut ctx, carol_ask);
    assert_eq!(
        pos(&mut ctx, CAROL).amount,
        -(QTY as i64),
        "fixture: the insolvent fill must NOT have been applied"
    );

    let after = pos(&mut ctx, CAROL).margin;
    assert_eq!(
        after,
        before - 1_000,
        "fixture: funding must have moved CAROL's margin"
    );
    let carol_rows: Vec<_> = take_position_changes(&mut ctx)
        .into_iter()
        .filter(|c| c.user == CAROL)
        .collect();
    assert_eq!(
        carol_rows.len(),
        1,
        "the rejected maker must get exactly the funding row, got {} rows",
        carol_rows.len()
    );
    assert_eq!(
        carol_rows[0].margin, after,
        "the row must carry the margin that was actually persisted — this is the `iw` an \
         indexer puts in P[]"
    );
    assert_eq!(
        carol_rows[0].amount,
        -(QTY as i64),
        "her short is untouched"
    );
    assert_eq!(carol_rows[0].entryPrice, PRICE);
}
