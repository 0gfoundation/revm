use super::*;
use crate::{
    interface::IPerpDex::{
        batchCancelOrdersCall, batchPlaceOrdersCall, liquidateCall, setLeverageCall, PlaceItem,
    },
    risk::{run_liquidate, run_set_leverage},
    run_perp_dex_call,
    storage::keys as storage_keys,
    types::MAX_USER_MARKETS,
};

/// One `PlaceItem` (the batch analogue of [`try_place_in`]'s arguments).
fn item_in(
    market_id: u64,
    side: u8,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
) -> PlaceItem {
    PlaceItem {
        marketId: market_id,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
                flags: 0,
    }
}

/// Markets the property test roams over.
const MARKETS: [u64; 3] = [1, 2, 3];
/// Deep pockets: the point of these tests is membership, never a margin reject.
const RICH: u64 = WALLET * 10_000;

fn market_at(id: u64) -> Market {
    Market {
        market_id: id,
        base_decimals: 8,
        price_decimals: 9,
        tick_size: TICK,
        step_size: QTY,
        min_quantity: QTY,
        max_quantity: QTY * 1_000,
        max_price: PRICE * 1_000,
        price_update_interval: 15,
        active: true,
        funding_interval: 0,
        interest_rate: 0,
        liquidation_fee_rate_bps: 0,
        // mark 0 disables the fill-time band, so matching in these tests is driven purely by
        // the book. The liquidation op sets a mark deliberately and puts it back.
        price_band_bps: 0,
        mark_price: 0,
        tiers: MarginTiers::default(),
        basis_mode: 0,
        deferred_work: 0,
        cumulative_funding_index: 0,
    }
}

fn setup_markets(ctx: &mut TestCtx, ids: &[u64], users: &[Address], wallet: u64) {
    storage::save_admin(ctx, ADMIN).unwrap();
    for &id in ids {
        storage::save_market(ctx, &market_at(id)).unwrap();
    }
    for &u in users {
        fund(ctx, u, wallet);
    }
}

#[allow(clippy::too_many_arguments)]
fn try_place_in(
    ctx: &mut TestCtx,
    caller: Address,
    market: u64,
    side: u8,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
) -> Result<Bytes, PerpError> {
    let input = placeOrderCall {
        marketId: market,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
                flags: 0,
    }
    .abi_encode();
    run_place_order(&input, caller, ctx)
}

fn place_in(ctx: &mut TestCtx, caller: Address, market: u64, side: u8, price: u64) -> [u8; 32] {
    let ret = try_place_in(ctx, caller, market, side, price, QTY, 0, 0).expect("placement");
    ret[..32].try_into().unwrap()
}

fn cancel_in(
    ctx: &mut TestCtx,
    caller: Address,
    market: u64,
    id: [u8; 32],
) -> Result<Bytes, PerpError> {
    let input = cancelOrderCall {
        orderId: id.into(),
        marketId: market,
    }
    .abi_encode();
    run_cancel_order(&input, caller, ctx)
}

/// The STORED index.
fn index(ctx: &mut TestCtx, user: Address) -> Vec<u64> {
    storage::load_user_markets(ctx, user).unwrap()
}

/// The index recomputed from scratch out of the state it mirrors — the ground truth the
/// property test compares against. Deliberately re-derives the predicate from the four
/// underlying reads rather than reusing any engine helper (the conditional list joined the
/// predicate in the cross P3 review).
fn ground_truth(ctx: &mut TestCtx, user: Address, markets: &[u64]) -> Vec<u64> {
    markets
        .iter()
        .copied()
        .filter(|&m| {
            storage::load_position(ctx, user, m).unwrap().amount != 0
                || !storage::load_buy_orders(ctx, user, m).unwrap().is_empty()
                || !storage::load_sell_orders(ctx, user, m).unwrap().is_empty()
                || !storage::load_user_algo_orders(ctx, user, m).unwrap().is_empty()
        })
        .collect()
}

fn assert_index_matches(ctx: &mut TestCtx, users: &[Address], markets: &[u64], step: &str) {
    for &u in users {
        assert_eq!(
            index(ctx, u),
            ground_truth(ctx, u, markets),
            "user {u} index diverged from ground truth after {step}"
        );
    }
}

// ── Step 0 of the derived-ooIM migration: are the maintained aggregates Bid/Ask? ────────
//
// The derived open-order requirement is
//     ooIM = ROUND_UP(max(|N + Bid|, |N − Ask|) / L) − ROUND_UP(|N| / L)
// where Binance defines `Bid` as Σ over the user's CURRENTLY RESTING buy orders of
// (REMAINING quantity × that order's ASSUMING PRICE, frozen at placement), and `Ask` the same
// over sells. The whole migration rests on `PerpPosition::total_buy_notional` /
// `total_sell_notional` being EXACTLY those two quantities, maintained incrementally.
//
// `assert_side_totals_are_bid_and_ask` below is the proof obligation. It does not reuse
// `math::sum_side_totals` (the engine's own fold — reusing it would only prove the engine
// agrees with itself); it re-derives the sum inline, and additionally re-derives each term
// from the authoritative `Order` record so that "remaining quantity" and "LIMIT price" are
// checked as CLAIMS about the order, not just as fields of the mirror.
//
// ⚠️ A sell's FROZEN assuming price deliberately cannot be re-derived from current state — that
// is what freezing means, and it is why the mark moves freely in the sweep below without the
// ground truth having to track it. What the fold pins instead is that the maintained total is
// the Σ of exactly the per-entry terms present in the list, at whatever price each entry
// carries; the freeze RULE (buy: `assuming_price == price`; sell: `>= price`, and `== price`
// above the floor) is asserted per entry alongside it.

/// The test market's fixed-point widths (see [`market_at`]).
const BD: u32 = 8;
const PD: u32 = 9;

/// Assert, for every `(user, market)`, that the four maintained per-side aggregates equal a
/// ground truth recomputed independently from the user's actual resting orders, AND that
/// each resting entry really is `(remaining qty, limit price)` of a live order.
///
/// Five distinct claims, each of which a maintenance bug breaks differently:
/// 1. every entry in an order LIST is backed by an `Order` that is still resting
///    (`Open`/`PartiallyFilled`) — so the list is "currently resting orders", not a graveyard;
/// 2. `entry.price` is that order's LIMIT price, untouched — the margin freeze may not leak
///    into the field matching, sorting and the fill price all key on;
/// 3. `entry.amount` is `quantity − filled`, the REMAINING quantity (so a partial fill really
///    does shrink the term);
/// 4. the freeze RULE holds per entry: a BUY's assuming price IS its limit price (no markup,
///    measured), a SELL's is at least its limit price and exactly it when the order rests above
///    the floor;
/// 5. the stored aggregates equal the inline Σ over those entries, at each entry's assuming
///    price.
fn assert_side_totals_are_bid_and_ask(
    ctx: &mut TestCtx,
    users: &[Address],
    markets: &[u64],
    step: &str,
) -> u32 {
    // Sell entries whose FROZEN assuming price sits strictly above their own limit — the shape
    // that makes this whole assertion discriminating (with none of them, folding at the limit
    // price would pass too). Returned so the driving test can assert the sweep reaches them.
    let mut marked_up_sells = 0u32;
    for &u in users {
        for &m in markets {
            let mut truth = [0u64; 4]; // tbq, tbn, tsq, tsn
            for (side, buy) in [(Side::Buy, true), (Side::Sell, false)] {
                let entries = if buy {
                    storage::load_buy_orders(ctx, u, m).unwrap()
                } else {
                    storage::load_sell_orders(ctx, u, m).unwrap()
                };
                for e in entries.iter() {
                    // (1) backed by a live, still-resting order in THIS market on THIS side.
                    let o = storage::load_order(ctx, &e.order_id)
                        .unwrap()
                        .unwrap_or_else(|| {
                            panic!(
                            "{step}: user {u} market {m} {side:?} list holds entry {:?} with \
                             no order record — a filled/cancelled order was left in the list",
                            e.order_id
                        )
                        });
                    assert!(
                        matches!(o.status, OrderStatus::Open | OrderStatus::PartiallyFilled),
                        "{step}: user {u} market {m} {side:?} list holds entry {:?} whose \
                         order is terminal ({:?}) — it is not a RESTING order",
                        e.order_id,
                        o.status
                    );
                    assert_eq!(
                        (o.market_id, o.side),
                        (m, side),
                        "{step}: entry {:?} filed under the wrong (market, side)",
                        e.order_id
                    );
                    // (2) `entry.price` is still the order's LIMIT price. This is the guard
                    // against `assuming_price` leaking into the execution field: matching,
                    // price priority, the sorted insert, the book level and the fill price all
                    // read `entry.price`, so a markup landing here would be silent corruption.
                    assert_eq!(
                        e.price, o.price,
                        "{step}: user {u} market {m} entry {:?} price {} is not the order's \
                         LIMIT price {}",
                        e.order_id, e.price, o.price
                    );
                    // (3) the quantity is what is still RESTING, i.e. net of partial fills.
                    assert_eq!(
                        e.amount,
                        o.quantity - o.filled,
                        "{step}: user {u} market {m} entry {:?} amount {} != remaining \
                         (quantity {} − filled {}) — a partial fill did not shrink the term",
                        e.order_id,
                        e.amount,
                        o.quantity,
                        o.filled
                    );
                    // (4) the freeze rule, per entry.
                    if buy {
                        assert_eq!(
                            e.assuming_price, e.price,
                            "{step}: user {u} market {m} BUY entry {:?} carries a marked-up \
                             assuming price {} over limit {} — the buy side has NO markup",
                            e.order_id, e.assuming_price, e.price
                        );
                    } else {
                        assert!(
                            e.assuming_price >= e.price,
                            "{step}: user {u} market {m} SELL entry {:?} assuming price {} is \
                             BELOW its limit {} — max(T, limit) can never be",
                            e.order_id,
                            e.assuming_price,
                            e.price
                        );
                        if e.assuming_price > e.price {
                            marked_up_sells += 1;
                        }
                    }
                    // (5) fold, inline — deliberately NOT `math::sum_side_totals`, and at the
                    // entry's FROZEN assuming price, which is the basis every maintenance site
                    // adds and subtracts.
                    let i = if buy { 0 } else { 2 };
                    truth[i] += e.amount;
                    truth[i + 1] +=
                        crate::math::calc_value(e.assuming_price, e.amount, BD, PD).unwrap();
                }
            }
            let p = storage::load_position(ctx, u, m).unwrap();
            assert_eq!(
                (
                    p.total_buy_qty,
                    p.total_buy_notional,
                    p.total_sell_qty,
                    p.total_sell_notional
                ),
                (truth[0], truth[1], truth[2], truth[3]),
                "{step}: user {u} market {m}: maintained (tbq, tbn=Bid, tsq, tsn=Ask) diverged \
                 from the resting-order ground truth (Σ at each entry's FROZEN assuming price)"
            );
        }
    }
    marked_up_sells
}

// ── The stored `Σ pos.margin` aggregate: is it exactly the sum? ─────────────────────────
//
// `AccountBalanceChanged.totalWalletBalance` is `perp_wallet_balance + Σ pos.margin`, and the Σ
// leg is no longer walked — it is the STORED `UserAccount::total_position_margin`, maintained
// incrementally by `storage::save_position` from `pos.margin − old.margin`. A stored aggregate can
// drift where the walked one could not, so it needs the same treatment the side aggregates got:
// recomputed ground truth after EVERY transition.
//
// Like `assert_side_totals_are_bid_and_ask`, this deliberately does NOT ask the engine for its own
// fold (`margin_view::index_account_scalars` / `fold_account_margin`) — that would only prove the
// engine agrees with itself. It sums `pos.margin` inline out of the position blobs, over ALL the
// sweep's markets rather than the per-user index, which additionally pins the identity the
// published surfaces depend on: `Σ_all pos.margin == Σ_index pos.margin`, i.e. no margin is ever
// stranded on a position outside the index (`getAccount` sums over the index, this field over
// everything).

/// Assert, for every user, that the stored `total_position_margin` equals `Σ pos.margin`
/// recomputed from the position blobs. Returns the number of users whose aggregate is currently
/// non-zero, so the driving test can prove the sweep is not asserting `0 == 0`.
fn assert_stored_total_position_margin(
    ctx: &mut TestCtx,
    users: &[Address],
    markets: &[u64],
    step: &str,
) -> u32 {
    let mut non_zero = 0u32;
    for &u in users {
        let mut truth: i128 = 0;
        for &m in markets {
            let p = storage::load_position(ctx, u, m).unwrap();
            // The invariant that makes the index-driven `getAccount` fold and this all-markets
            // sum the same number — and which `storage::save_position` asserts on every write.
            assert!(
                p.amount != 0 || p.margin == 0,
                "{step}: user {u} market {m} is flat but holds margin {} — margin stranded \
                 outside the per-user market index would make getAccount's totalWalletBalance \
                 disagree with the event's",
                p.margin
            );
            truth += p.margin as i128;
        }
        let stored = storage::load_account(ctx, u).unwrap().total_position_margin;
        assert_eq!(
            stored as i128, truth,
            "{step}: user {u} stored total_position_margin diverged from Σ pos.margin \
             recomputed from the position blobs"
        );
        if truth != 0 {
            non_zero += 1;
        }
    }
    non_zero
}

/// Deterministic xorshift (same generator as the settlement conservation fuzz).
fn next(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

// ── 1. The deliverable: ground-truth property test ──────────────────────

/// What one randomised pass actually reached. A property test that never gets to the
/// interesting transitions proves nothing, so the pass returns its coverage and the test
/// asserts on it.
#[derive(Debug, Default)]
struct FuzzCoverage {
    enters: u32,
    leaves: u32,
    liquidations: u32,
    flips: u32,
    /// Max number of resting SELLS seen in one sweep whose FROZEN assuming price is strictly
    /// above their own limit price. Zero would make the Bid/Ask fold non-discriminating: with no
    /// marked-up sell anywhere, folding at the limit price would agree too.
    marked_up_sells: u32,
    /// Max number of users seen in one sweep whose stored `Σ pos.margin` is non-zero. Zero would
    /// make [`assert_stored_total_position_margin`] assert `0 == 0` for the whole pass.
    margin_bearing_users: u32,
    /// Self-trade-prevention cancels. Both users rest AND cross on the same prices, so their
    /// GTC and market takers keep meeting their own orders — and an STP cancel empties an order
    /// list mid-match, through the registry flush, exactly like the K9 and cover cancels.
    stp_cancels: u32,
}

/// One randomised pass: a mix of place / cancel / fill / partial fill / close / flip /
/// liquidate across 3 markets and 2 users, asserting after EVERY step that each user's STORED
/// index equals `{ market : position != 0 OR buy_orders non-empty OR sell_orders non-empty }`,
/// recomputed independently from storage.
///
/// This is what proves the maintenance hooks cover every transition: any write path that moves
/// `pos.amount` or an order list without going through them desynchronises the index within a
/// step or two of being exercised. Rejected operations are kept in the mix on purpose — a
/// reject must leave the index untouched too (validate-then-apply).
fn run_fuzz(seed: u64, wallet: u64, steps: u32) -> FuzzCoverage {
    let users = [ALICE, BOB];
    let mut ctx = make_ctx();
    setup_markets(&mut ctx, &MARKETS, &users, wallet);
    // Leverage 3 (the default tier cap) so an adverse mark can actually breach maintenance —
    // a leverage-1 position is mathematically never liquidatable.
    for &u in &users {
        for &m in &MARKETS {
            run_set_leverage(
                &setLeverageCall {
                    marketId: m,
                    leverage: 3,
                }
                .abi_encode(),
                u,
                &mut ctx,
            )
            .unwrap();
        }
    }
    assert_index_matches(&mut ctx, &users, &MARKETS, "setup");

    // Live order ids per (user index, market) so cancels have something real to aim at.
    let mut live: Vec<Vec<[u8; 32]>> = vec![Vec::new(); users.len() * MARKETS.len()];
    let slot = |ui: usize, mi: usize| ui * MARKETS.len() + mi;

    let mut cov = FuzzCoverage::default();
    let mut s: u64 = seed;
    for step in 0..steps {
        let r = next(&mut s);
        let ui = (r % users.len() as u64) as usize;
        let mi = ((r >> 8) % MARKETS.len() as u64) as usize;
        let user = users[ui];
        let market = MARKETS[mi];
        let op = (r >> 16) % 8;
        // Prices straddle $100 in $5 steps so orders both rest and cross.
        let price = (90 + ((r >> 24) % 5) * 5) * TICK;
        // 1..=4 lots: partial fills, full fills and over-fills (flips) all occur.
        let qty = QTY * (1 + ((r >> 32) % 4));

        let before: Vec<Vec<u64>> = users.iter().map(|&u| index(&mut ctx, u)).collect();
        let amount_before = storage::load_position(&mut ctx, user, market)
            .unwrap()
            .amount;

        let what: &str = match op {
            // Resting / crossing GTC limits on both sides (the bread and butter: rest,
            // full fill, partial fill, position open/increase/decrease/close/flip).
            0..=3 => {
                let side = (op % 2) as u8;
                if let Ok(ret) = try_place_in(&mut ctx, user, market, side, price, qty, 0, 0) {
                    let id: [u8; 32] = ret[..32].try_into().unwrap();
                    // Only a still-live (resting) order is a future cancel target.
                    if storage::load_order(&mut ctx, &id).unwrap().is_some() {
                        live[slot(ui, mi)].push(id);
                    }
                }
                "limit place"
            }
            // Market order: fills or expires, never rests.
            4 => {
                let side = ((r >> 40) % 2) as u8;
                let _ = try_place_in(&mut ctx, user, market, side, 0, qty, 1, 1);
                "market place"
            }
            // Cancel one live order (the last-cancel is what removes an order-only member).
            5 | 6 => {
                if let Some(id) = live[slot(ui, mi)].pop() {
                    let _ = cancel_in(&mut ctx, user, market, id);
                }
                "cancel"
            }
            // Liquidate: cancel-all + close, the widest single-step transition there is.
            // Crash/spike the mark against the holder's side, liquidate, then restore mark 0
            // (which is also what keeps the fill-time band out of the other ops' way).
            _ => {
                let amount = storage::load_position(&mut ctx, user, market)
                    .unwrap()
                    .amount;
                if amount != 0 {
                    let mark = if amount > 0 { 10 * TICK } else { 400 * TICK };
                    storage::save_mark_price(&mut ctx, market, mark).unwrap();
                    let _ = run_liquidate(
                        &liquidateCall {
                            user,
                            marketId: market,
                        }
                        .abi_encode(),
                        users[1 - ui],
                        &mut ctx,
                    );
                    storage::save_mark_price(&mut ctx, market, 0).unwrap();
                    // Liquidation cancel-all kills every resting order of that user here.
                    live[slot(ui, mi)].clear();
                    if storage::load_position(&mut ctx, user, market)
                        .unwrap()
                        .amount
                        == 0
                    {
                        cov.liquidations += 1;
                    }
                }
                "liquidate"
            }
        };

        let amount_after = storage::load_position(&mut ctx, user, market)
            .unwrap()
            .amount;
        if amount_before.signum() * amount_after.signum() == -1 {
            cov.flips += 1;
        }
        for (i, &u) in users.iter().enumerate() {
            let after = index(&mut ctx, u);
            cov.enters += after.iter().filter(|m| !before[i].contains(m)).count() as u32;
            cov.leaves += before[i].iter().filter(|m| !after.contains(m)).count() as u32;
        }

        cov.stp_cancels += stp_cancel_count(&JournalTr::take_logs(ctx.journal_mut()));
        let where_ = format!("step {step} ({what}, user {ui}, market {market})");
        assert_index_matches(&mut ctx, &users, &MARKETS, &where_);
        // Derived-ooIM Step 0: the SAME sweep also proves the per-side reservation
        // aggregates are exactly Binance's Bid/Ask after every transition. One pass, two
        // invariants — the op mix (rest / partial fill / full fill / flip / cancel /
        // mid-match auto-cancel / liquidation cancel-all / reject) is precisely the set of
        // transitions that can desynchronise them.
        cov.marked_up_sells = cov.marked_up_sells.max(assert_side_totals_are_bid_and_ask(
            &mut ctx, &users, &MARKETS, &where_,
        ));
        // ...and the THIRD invariant on the same sweep: the stored `Σ pos.margin` aggregate the
        // `AccountBalanceChanged` payload is now read from. Same op mix, and it is precisely the
        // set of transitions that move `pos.margin` — open, partial/full fill, flip, funding,
        // liquidation close, ADL, add/removePositionMargin.
        cov.margin_bearing_users =
            cov.margin_bearing_users
                .max(assert_stored_total_position_margin(
                    &mut ctx, &users, &MARKETS, &where_,
                ));
        // The index is a SET, always ascending, and never exceeds the cap.
        for &u in &users {
            let ix = index(&mut ctx, u);
            assert!(
                ix.windows(2).all(|w| w[0] < w[1]),
                "index not strictly ascending at step {step}: {ix:?}"
            );
            assert!(ix.len() <= MAX_USER_MARKETS, "cap breached at step {step}");
        }
    }
    cov
}

/// The centrepiece. Two passes with deliberately different money:
///
/// * **deep pockets** — placements are never margin-rejected, so the mix is dominated by clean
///   rests, fills, flips and liquidations;
/// * **thin wallets** — the paths that only appear when money runs out: the taker wallet-cover
///   LIFO cancel cascade and the maker open-into-insolvency reject. Both empty an order list
///   *mid-match*, through the match registry's flush, with the holder flat before AND after —
///   the one shape no other hook can repair, so it is the pass that makes the order-list
///   `save_*` hooks load-bearing.
#[test]
fn index_matches_ground_truth_after_every_operation() {
    let deep = run_fuzz(0x0d_e4_11_ed_5e_ed_00_1f, RICH, 800);
    let thin = run_fuzz(0x5c_a4_ce_11_a7_10_00_23, WALLET, 800);
    println!("user-market-index fuzz coverage: deep={deep:?} thin={thin:?}");
    for (name, cov) in [("deep", &deep), ("thin", &thin)] {
        assert!(
            cov.enters >= 20,
            "{name}: too few market entries ({})",
            cov.enters
        );
        assert!(
            cov.leaves >= 20,
            "{name}: too few market exits ({})",
            cov.leaves
        );
    }
    assert!(deep.liquidations >= 1, "no liquidation ever completed");
    assert!(deep.flips >= 1, "no position ever flipped sign");
    // About half of what the two seeded passes reach (115 / 91).
    for (name, cov) in [("deep", &deep), ("thin", &thin)] {
        assert!(cov.stp_cancels >= 40, "{name}: too few STP cancels ({})", cov.stp_cancels);
    }
}

/// **Derived-ooIM Step 0 deliverable, on the FROZEN basis.** The maintained per-side aggregates
/// `total_buy_notional` / `total_sell_notional` ARE Binance's `Bid` / `Ask` — Σ over the user's
/// currently resting orders of (remaining quantity × that order's ASSUMING PRICE, frozen when it
/// was placed) — and the two qty aggregates are the matching Σ quantity, after EVERY step of a
/// randomised sequence.
///
/// The per-step assertion lives inside [`run_fuzz`] (see
/// [`assert_side_totals_are_bid_and_ask`]); this test drives it over its own seeds so the
/// property has a named owner, and asserts the pass actually reached the transitions that
/// could break it. Two passes, deliberately different money, exactly as the index property
/// test: the thin-wallet pass is the only one that reaches the taker wallet-cover LIFO
/// cancel cascade and the mid-match order-list rewrite, which are the two paths that resync
/// the aggregates by RECOMPUTE rather than incrementally.
///
/// `marked_up_sells` is the coverage floor the freeze added: the sweep's sells straddle the
/// Assuming-Price floor implied by the last print, so entries with `assuming_price > price`
/// really do occur. Without one, folding at the LIMIT price would satisfy the assertion too and
/// the whole property would be blind to the change.
///
/// Mutation-tested on the frozen basis. **Four maintenance sites, each killed:**
///
/// | site | mutation | result |
/// |---|---|---|
/// | placement increment, BUY arm (`trading/mod.rs`) | drop the `checked_add` | FAILS |
/// | placement increment, SELL arm (`trading/mod.rs`) | drop the `checked_add` | FAILS |
/// | the shared decrement (`remove_entry_from_side_aggregates`) — cancel, partial fill, LIFO cover | no-op the `checked_sub` | FAILS |
/// | partial-fill delta feeding it (`settle_maker_fill_core`) | pass 0 instead of the delta | FAILS |
/// | liquidation cancel-all zeroing (`risk/mod.rs`, the zeroed `SideAggregates`) | drop the call | FAILS |
///
/// …and mutation-tested against the FREEZE specifically: maintaining any one site at
/// `entry.price` instead of `entry.assuming_price` also FAILS, because the sites then disagree
/// with each other on a marked-up sell (tried on the cancel decrement and on
/// `math::sum_side_totals`).
///
/// ⚠️ **One thing this test does NOT prove, and an earlier version of this comment wrongly
/// claimed it did:** the match flush's `w.pos.total_* = t*` RESYNC (`settlement.rs::flush`)
/// SURVIVES deletion. It is redundant by construction — the walk already maintains the
/// aggregates incrementally, and the `debug_assert_eq!` two lines above the assignment is
/// exactly the statement that the recompute equals what is already there. So the assignment can
/// only matter if the incremental maintenance is wrong, and in that case the debug assert fires
/// first: zeroing the partial-fill delta (row 4) panics AT that assert, not at this test's own
/// fold. The resync is a release-build backstop, not an independently observable site. This
/// survival is pre-existing (that block is untouched by the freeze), not a regression.
#[test]
fn side_aggregates_are_exactly_bid_and_ask_after_every_operation() {
    // 1_200 rather than 800 only for THIS seed: ADL now cancels the counterparty's resting
    // orders, which perturbs the walk enough that this seed's first sign flip moved past step
    // 800. Lengthening the run restores the coverage floor below without weakening it.
    let deep = run_fuzz(0x00_1f_bd_a5_c0_de_00_11, RICH, 1_200);
    let thin = run_fuzz(0x7a_51_de_ad_be_ef_00_29, WALLET, 800);
    println!("ooIM Bid/Ask fuzz coverage: deep={deep:?} thin={thin:?}");
    // A pass that never rests, never partially fills and never liquidates would assert
    // "0 == 0" 800 times. Reuse the index pass's own coverage floors.
    for (name, cov) in [("deep", &deep), ("thin", &thin)] {
        assert!(cov.enters >= 20, "{name}: too few market entries");
        assert!(cov.leaves >= 20, "{name}: too few market exits");
        assert!(
            cov.marked_up_sells >= 1,
            "{name}: no resting sell ever carried a marked-up assuming price, so the fold \
             cannot tell the frozen basis from the limit-price one"
        );
    }
    assert!(
        deep.liquidations >= 1,
        "no liquidation cancel-all exercised"
    );
    assert!(deep.flips >= 1, "no position ever flipped sign");
}

/// **The stored `Σ pos.margin` aggregate equals a recomputed `Σ pos.margin` after EVERY
/// operation.** `UserAccount::total_position_margin` is what
/// `AccountBalanceChanged.totalWalletBalance` is now read from (`wb = cw + this`), replacing a
/// walk of the user's market index and a position load per member market. Stored, it can DRIFT —
/// this is the property that says it does not.
///
/// The per-step assertion lives inside [`run_fuzz`] (see
/// [`assert_stored_total_position_margin`]); this test drives it over its own seeds so the
/// property has a named owner, and asserts the pass actually reached states where the aggregate is
/// non-zero (otherwise it would compare `0 == 0` 800 times). Two passes with deliberately
/// different money, exactly as its two sibling properties: the thin-wallet pass is the one that
/// reaches the underfunded maker fill, whose silo is short of its own IM, and the taker
/// wallet-cover cascade.
///
/// Ground truth is computed INDEPENDENTLY — `Σ` over the position blobs, inline, not
/// `margin_view`'s own fold — for the same reason the Bid/Ask property does it: reusing the
/// engine's fold would only prove the engine agrees with itself. It also sums over ALL the
/// sweep's markets rather than the per-user index, which pins the identity the two published
/// surfaces depend on (`getAccount` sums over the index, the event reads this all-markets field).
///
/// Mutation-tested on the single maintenance point in `storage::save_position`. **Each broken
/// variant killed:**
///
/// | mutation | caught by |
/// |---|---|
/// | drop the increment entirely | THIS test (`step 1`), and the `debug_assertions` cross-check in `margin_view::index_account_scalars` via ~15 other tests |
/// | wrong sign (`old_margin − pos.margin`) | THIS test, and the same cross-check |
/// | `pos.margin` instead of the delta | THIS test, and the same cross-check |
///
/// The cross-check is what makes a missed maintenance point loud everywhere rather than only
/// here; this test is what pins the arithmetic over a transition mix no single scenario reaches.
#[test]
fn stored_position_margin_is_exactly_the_sum_after_every_operation() {
    let deep = run_fuzz(0x50_5f_a2_91_00_00_00_11, RICH, 800);
    let thin = run_fuzz(0x9e_ed_5c_a4_1e_d0_00_37, WALLET, 800);
    println!("Σ pos.margin fuzz coverage: deep={deep:?} thin={thin:?}");
    for (name, cov) in [("deep", &deep), ("thin", &thin)] {
        assert!(cov.enters >= 20, "{name}: too few market entries");
        assert!(cov.leaves >= 20, "{name}: too few market exits");
        assert!(
            cov.margin_bearing_users >= 2,
            "{name}: never reached a state where two users held margin at once, so the \
             aggregate was compared against a trivial sum"
        );
    }
    assert!(
        deep.liquidations >= 1,
        "no liquidation ever zeroed a silo through the close path"
    );
    assert!(deep.flips >= 1, "no position ever flipped sign");
}

// ── 2. Enter / leave round trip ─────────────────────────────────────────

/// Entering a market inserts ONCE (a second order in the same market is not a second entry);
/// leaving happens on the LAST order cancel, not the first; and a position with no orders at
/// all still keeps the user in.
#[test]
fn enter_once_leave_on_the_last_order_and_a_position_alone_keeps_membership() {
    let users = [ALICE, BOB];
    let mut ctx = make_ctx();
    setup_markets(&mut ctx, &MARKETS, &users, RICH);
    assert!(index(&mut ctx, ALICE).is_empty(), "starts in no market");

    // First resting bid in market 1 → enter.
    let a1 = place_in(&mut ctx, ALICE, 1, 0, 95 * TICK);
    assert_eq!(index(&mut ctx, ALICE), vec![1]);

    // Second order in the SAME market → still one entry, not two.
    let a2 = place_in(&mut ctx, ALICE, 1, 0, 94 * TICK);
    assert_eq!(
        index(&mut ctx, ALICE),
        vec![1],
        "second order must not re-enter"
    );
    // ...and the other side of the same market is not a third entry either.
    let a3 = place_in(&mut ctx, ALICE, 1, 1, 105 * TICK);
    assert_eq!(index(&mut ctx, ALICE), vec![1]);

    // A different market appends (ascending).
    let b1 = place_in(&mut ctx, ALICE, 2, 0, 95 * TICK);
    assert_eq!(index(&mut ctx, ALICE), vec![1, 2]);

    // Cancelling all but one order in market 1 keeps her in it.
    cancel_in(&mut ctx, ALICE, 1, a1).unwrap();
    cancel_in(&mut ctx, ALICE, 1, a3).unwrap();
    assert_eq!(
        index(&mut ctx, ALICE),
        vec![1, 2],
        "one order left → still a member"
    );
    // The LAST cancel leaves.
    cancel_in(&mut ctx, ALICE, 1, a2).unwrap();
    assert_eq!(
        index(&mut ctx, ALICE),
        vec![2],
        "last order cancelled → left market 1"
    );

    // A POSITION with no orders keeps her in: BOB lifts her market-2 bid, so her only
    // remaining trace there is the long it opened.
    place_in(&mut ctx, BOB, 2, 1, 95 * TICK);
    assert_terminal(&mut ctx, b1);
    assert!(
        storage::load_buy_orders(&mut ctx, ALICE, 2)
            .unwrap()
            .is_empty()
            && storage::load_position(&mut ctx, ALICE, 2).unwrap().amount != 0,
        "market 2 must now be position-only for ALICE"
    );
    assert_eq!(
        index(&mut ctx, ALICE),
        vec![2],
        "a position alone keeps membership"
    );
    assert_eq!(
        index(&mut ctx, BOB),
        vec![2],
        "and gives BOB the opposite side"
    );

    // Closing the position is the last thing she had there: BOB quotes a bid (he is short,
    // so this closes both sides) and ALICE market-sells into it.
    place_in(&mut ctx, BOB, 2, 0, 95 * TICK);
    try_place_in(&mut ctx, ALICE, 2, 1, 0, QTY, 1, 1).unwrap();
    assert_eq!(
        storage::load_position(&mut ctx, ALICE, 2).unwrap().amount,
        0
    );
    assert_eq!(storage::load_position(&mut ctx, BOB, 2).unwrap().amount, 0);
    assert!(
        index(&mut ctx, ALICE).is_empty(),
        "flat everywhere → index key deleted"
    );
    assert!(
        index(&mut ctx, BOB).is_empty(),
        "the maker leaves on the same fill"
    );
}

// ── 3. The cap ──────────────────────────────────────────────────────────

/// Entering one market past [`MAX_USER_MARKETS`] rejects with its own error string, and does
/// so BEFORE any write: the refused market has no order, no book entry and no index slot, and
/// the user's wallet and order nonce are exactly where they were.
#[test]
fn cap_rejects_the_market_past_the_limit_before_any_write() {
    let over = MAX_USER_MARKETS as u64 + 1;
    let all: Vec<u64> = (1..=over).collect();
    let mut ctx = make_ctx();
    setup_markets(&mut ctx, &all, &[ALICE], RICH);

    for m in 1..=MAX_USER_MARKETS as u64 {
        place_in(&mut ctx, ALICE, m, 0, 95 * TICK);
    }
    let full: Vec<u64> = (1..=MAX_USER_MARKETS as u64).collect();
    assert_eq!(index(&mut ctx, ALICE), full, "exactly at the cap");

    let wallet_before = wallet(&mut ctx, ALICE);
    let nonce_before = storage::load_user_nonce(&mut ctx, ALICE).unwrap();
    let err = try_place_in(&mut ctx, ALICE, over, 0, 95 * TICK, QTY, 0, 0).unwrap_err();
    assert!(
        err.to_string().contains("user market limit reached"),
        "wrong error: {err}"
    );

    // Pre-write: nothing about the refused market exists, and nothing of ALICE's moved.
    assert_eq!(
        index(&mut ctx, ALICE),
        full,
        "index unchanged by the reject"
    );
    assert!(storage::load_buy_orders(&mut ctx, ALICE, over)
        .unwrap()
        .is_empty());
    assert_eq!(
        storage::load_position(&mut ctx, ALICE, over)
            .unwrap()
            .amount,
        0
    );
    assert!(storage::load_bid_prices(&mut ctx, over).unwrap().is_empty());
    assert_eq!(wallet(&mut ctx, ALICE), wallet_before, "no margin reserved");
    assert_eq!(
        storage::load_user_nonce(&mut ctx, ALICE).unwrap(),
        nonce_before,
        "a rejected placement must not consume an order id"
    );

    // A market she is ALREADY in is still admitted at the cap — the gate bounds the SET, not
    // the order count.
    place_in(&mut ctx, ALICE, 1, 0, 94 * TICK);
    assert_eq!(index(&mut ctx, ALICE), full);

    // Leaving one frees exactly one slot.
    let orders = storage::load_buy_orders(&mut ctx, ALICE, 2).unwrap();
    for e in orders.iter() {
        cancel_in(&mut ctx, ALICE, 2, e.order_id).unwrap();
    }
    assert!(!index(&mut ctx, ALICE).contains(&2));
    place_in(&mut ctx, ALICE, over, 0, 95 * TICK);
    assert!(
        index(&mut ctx, ALICE).contains(&over),
        "freed slot is reusable"
    );
}

// ── 4. Canonical ordering ───────────────────────────────────────────────

/// The stored blob depends only on the SET, never on the order the markets were entered in.
/// Compared on the real block-delta bytes (what the commitment folds), not on the decoded Vec.
#[test]
fn same_set_reached_by_different_orders_has_byte_identical_blobs() {
    fn blob_after(entry_order: &[u64]) -> Vec<u8> {
        let mut ctx = make_ctx();
        setup_markets(&mut ctx, &MARKETS, &[ALICE], RICH);
        for (i, &m) in entry_order.iter().enumerate() {
            // Vary the side and price too, so nothing but the set can coincide.
            let side = (i % 2) as u8;
            let price = if side == 0 { 95 * TICK } else { 105 * TICK };
            place_in(&mut ctx, ALICE, m, side, price);
        }
        let delta = JournalTr::take_perp_delta(ctx.journal_mut());
        delta
            .get(&storage_keys::user_markets_key(ALICE))
            .expect("the index key is in the block delta")
            .bytes
            .clone()
    }

    let ascending = blob_after(&[1, 2, 3]);
    let descending = blob_after(&[3, 2, 1]);
    let shuffled = blob_after(&[2, 3, 1]);
    assert_eq!(
        ascending, descending,
        "entry order must not reach the bytes"
    );
    assert_eq!(ascending, shuffled, "entry order must not reach the bytes");
    assert!(!ascending.is_empty());
    // And it really is the canonical encoding of the ascending set.
    assert_eq!(ascending, storage::encode(&vec![1u64, 2, 3]).unwrap());

    // Re-entering after leaving is likewise order-free: {1,3} is {1,3} either way.
    assert_eq!(blob_after(&[1, 3]), blob_after(&[3, 1]));
}

// ── 6. The batch single-initiator working-set ───────────────────────────

/// A batch routes the initiator's position and order lists through a call-scoped working-set
/// that is only flushed into the main store at the end, while the index itself is NOT hoisted.
/// The maintenance hooks therefore read one copy and write another — this pins that the two
/// stay in agreement, both per item and after the flush.
#[test]
fn batch_place_and_cancel_maintain_the_index_through_the_working_set() {
    let mut ctx = make_ctx();
    setup_markets(&mut ctx, &MARKETS, &[ALICE], RICH);

    let items: Vec<PlaceItem> = MARKETS
        .iter()
        .flat_map(|&m| {
            [
                item_in(m, 0, 95 * TICK, QTY, 0, 0),
                item_in(m, 1, 105 * TICK, QTY, 0, 0),
            ]
        })
        .collect();
    let out = run_perp_dex_call(
        &batchPlaceOrdersCall {
            orders: items.clone(),
        }
        .abi_encode(),
        30_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .expect("batch place");
    assert!(!out.reverted);
    assert_eq!(
        index(&mut ctx, ALICE),
        MARKETS.to_vec(),
        "one batch entered all three markets"
    );
    assert_index_matches(&mut ctx, &[ALICE], &MARKETS, "batch place");

    // Batch-cancel everything: every leg empties, so ALICE leaves every market.
    let ids: Vec<FixedBytes<32>> = MARKETS
        .iter()
        .flat_map(|&m| {
            let buys = storage::load_buy_orders(&mut ctx, ALICE, m).unwrap();
            let sells = storage::load_sell_orders(&mut ctx, ALICE, m).unwrap();
            buys.iter()
                .chain(sells.iter())
                .map(|e| FixedBytes(e.order_id))
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(ids.len(), items.len());
    let out = run_perp_dex_call(
        &batchCancelOrdersCall { orderIds: ids }.abi_encode(),
        30_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .expect("batch cancel");
    assert!(!out.reverted);
    assert_index_matches(&mut ctx, &[ALICE], &MARKETS, "batch cancel");
    assert!(
        index(&mut ctx, ALICE).is_empty(),
        "cancelling every order leaves every market"
    );
}

// ── 5. Liquidation clears the market ────────────────────────────────────

/// A liquidation cancels every resting order AND closes the position, so it is the one
/// operation that can clear all three legs at once — the user must leave the market.
#[test]
fn liquidation_removes_the_user_from_the_market_it_clears() {
    let users = [ALICE, BOB];
    let mut ctx = make_ctx();
    setup_markets(&mut ctx, &MARKETS, &users, RICH);
    for &m in &[1u64, 2] {
        run_set_leverage(
            &setLeverageCall {
                marketId: m,
                leverage: 3,
            }
            .abi_encode(),
            ALICE,
            &mut ctx,
        )
        .unwrap();
    }

    // ALICE goes long market 1 (BOB is the maker), and keeps resting orders on BOTH sides
    // there plus an unrelated order in market 2.
    place_in(&mut ctx, BOB, 1, 1, 100 * TICK);
    place_in(&mut ctx, ALICE, 1, 0, 100 * TICK);
    assert!(storage::load_position(&mut ctx, ALICE, 1).unwrap().amount > 0);
    place_in(&mut ctx, ALICE, 1, 0, 90 * TICK);
    place_in(&mut ctx, ALICE, 1, 1, 130 * TICK);
    place_in(&mut ctx, ALICE, 2, 0, 95 * TICK);
    assert_eq!(index(&mut ctx, ALICE), vec![1, 2]);

    // Crash the mark: the long breaches maintenance and CAROL liquidates it.
    storage::save_mark_price(&mut ctx, 1, 10 * TICK).unwrap();
    run_liquidate(
        &liquidateCall {
            user: ALICE,
            marketId: 1,
        }
        .abi_encode(),
        CAROL,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(
        storage::load_position(&mut ctx, ALICE, 1).unwrap().amount,
        0
    );
    assert!(storage::load_buy_orders(&mut ctx, ALICE, 1)
        .unwrap()
        .is_empty());
    assert!(storage::load_sell_orders(&mut ctx, ALICE, 1)
        .unwrap()
        .is_empty());
    assert_eq!(
        index(&mut ctx, ALICE),
        vec![2],
        "liquidation cleared market 1; the untouched market 2 order keeps her there"
    );
}
