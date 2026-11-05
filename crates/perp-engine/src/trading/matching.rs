use super::*;

// ── Matching engine ───────────────────────────────────────────────────────────

/// Write back one level the match walk is DONE with — the single place a walked level is closed.
///
/// `count_new` is the level's post-walk LIVE count (`count_old - level_removed`). Zero means the
/// level is logically empty — any ids still in `new_queue` are stale — so the price leaves the index
/// (`RemovePrice`, recorded in `removed` so the post-walk best recompute skips it, and `cleared`
/// set) and the blob is deleted (`SaveLevel` with count 0, which also drops the stale ids).
/// Otherwise the surviving FIFO is saved with its count.
///
/// The walk closes a level from more than one place (the taker ran out of quantity mid-level, or it
/// consumed every live maker here), and each used to carry its own copy of this block. They must
/// never drift: `removed`/`cleared` feed the post-walk `SaveBest` / `MidPriceSample` recompute, and
/// a close that forgot them would leave a stale best quote behind an emptied level.
fn close_level(
    registry: &mut settlement::MatchRegistry,
    is_bid: bool,
    price: u64,
    new_queue: Vec<[u8; 32]>,
    count_new: u64,
    removed: &mut Vec<u64>,
    cleared: &mut bool,
) {
    if count_new == 0 {
        registry.push_event(settlement::MatchEvent::RemovePrice { is_bid, price });
        removed.push(price);
        *cleared = true;
        registry.push_event(settlement::MatchEvent::SaveLevel {
            is_bid,
            price,
            queue: Vec::new(),
            count: 0,
        });
    } else {
        registry.push_event(settlement::MatchEvent::SaveLevel {
            is_bid,
            price,
            queue: new_queue,
            count: count_new,
        });
    }
}

/// Core matching loop. Performs **zero storage writes** and returns the decided-but-unwritten
/// [`MatchOutcome`]; the CALLER owns the write barrier ([`MatchOutcome::apply`]).
pub(super) fn match_order<H: PerpHost>(
    context: &mut H,
    taker_addr: Address,
    taker_order_id: &[u8; 32],
    market_id: u64,
    side: Side,
    limit_price: u64,
    quantity: u64,
    // WHAT the taker order is, as one value. Three behaviours read off it and nothing else:
    // `is_limit` (does `limit_price` bound the walk), `is_fok` (all-or-nothing), and
    // `rests_remainder` (does the caller rest what is left — formerly a separate `rest_remainder`
    // bool the caller had to keep consistent with the TIF it also passed).
    kind: OrderKind,
    market: &crate::types::Market,
    // When true, this is a liquidation close: the taker fee is waived and the number of distinct
    // maker accounts is capped so balance after-image gas has a fixed pre-write upper bound.
    // NOT derivable from `kind` — a liquidation close is an ordinary market order placed in a
    // privileged ROLE, and the role is what waives the fee.
    liquidation_close: bool,
    // The taker's Order threaded in memory (commit-only #23): NOT yet persisted — the caller
    // performs the single final save after every genuine reject has passed, so a rejected
    // placement leaves no phantom order (and a signed order's signature is not burned).
    taker_order: &mut Order,
) -> Result<MatchOutcome, PerpError> {
    let mut remaining = quantity;
    let mut last_trade_price = None;
    let mut taker_settlement =
        TakerSettlement::load(context, taker_addr, market_id, liquidation_close)?;
    // commit-only #23 L1: per-user working copies for this match. Each touched maker is loaded
    // once (funding settled at first touch, exactly where the first per-maker settle did it) and
    // saved once at the flush below — same write-key set and net values as the old per-fill saves.
    let mut registry = settlement::MatchRegistry::new();
    // The taker order is mutated in memory per fill; the CALLER persists the final state once.
    // Nothing reads it from storage mid-match: the registry touches only the makers, finalize
    // touches the taker's position/account, and the taker order is not in any book queue yet.

    // Fill-time price band: a fill may never execute farther than ±band from the CURRENT
    // mark, regardless of order type. Because each side of the book is sorted, this is a
    // clean early break — the first out-of-band level ends matching and everything past it
    // (necessarily farther) is skipped. This is the sole off-mark guard (there is no
    // placement-time band): it sees post-placement mark drift, lets harmless deep passive
    // orders rest, and gives market orders implicit slippage protection. mark_price is a
    // field of the Market already loaded — no separate storage read.
    let mark = market.mark_price;
    let (mark_upper, mark_lower) =
        crate::math::mark_band_bounds(mark, market.price_band_bps);

    // ── Self-trade prevention: decided ONCE, from the TAKER alone ───────────────────────────────
    //
    // `Some(mode)` when STP governs this walk: a GTC, IOC or market taker
    // (`OrderKind::stp_applies`) that is not a liquidation close. `None` for a FOK taker (it trades
    // with its owner's own liquidity, as before — Binance: "STP won't take effect"), for PostOnly
    // (never reaches a walk: it rejects on any cross), and for a liquidation close (the liquidated
    // user's orders in this market are all cancelled before it runs, so it cannot meet one). A
    // fired conditional order carries the mode it was placed with.
    //
    // The check runs per resting ORDER, in FIFO, on the same ADDRESS only (a sub-account is a
    // different address and trades with its master as usual), and only the TAKER's mode is read.
    let stp = (!liquidation_close && kind.stp_applies()).then_some(taker_order.stp);
    // Own resting orders this walk cancelled. NOT capped per order: every one is an open order of
    // the taker's own on the opposite side of this market, so the per-market open-order cap bounds
    // it (`MAX_OPEN_ORDERS_PER_MARKET`), and each was paid for when it was placed.
    let mut stp_cancels = 0usize;
    // The walk was STOPPED at the taker's own order (`EXPIRE_TAKER` / `EXPIRE_BOTH`): whatever is
    // left expires with reason 9 and never rests.
    let mut stp_taker_expired = false;

    match side {
        Side::Buy => {
            // Match against asks (sorted ASC: lowest ask first).
            let ask_prices = storage::load_ask_prices_ref(context, market_id)?;
            let old_best_ask = ask_prices.first().copied().unwrap_or(0);
            let mut ask_levels_cleared = false;
            let mut removed_asks: Vec<u64> = Vec::new();
            'outer: for ask_price in ask_prices.iter().copied() {
                // Fill-time band (lower): an ask below mark-band is an off-mark price
                // (e.g. a closing maker dumping cheap); skip it — higher, in-band asks may
                // still match. It stays resting until the mark legitimately reaches it.
                if (ask_price as u128) < mark_lower {
                    continue;
                }
                // For limit buy: only match if ask_price <= our limit. A market order has no
                // limit price to respect — the band above/below is its only bound.
                if kind.is_limit() && ask_price > limit_price {
                    break;
                }
                // Fill-time band (upper): asks above mark+band end matching (ascending).
                if ask_price as u128 > mark_upper {
                    break;
                }
                // ONE probe gets both the FIFO ids and the live count (Obs-1 merge).
                let blob = storage::load_ask_level_arc(context, market_id, ask_price)?;
                let count_old = blob.count;
                let queue = &blob.ids;
                let mut new_queue: Vec<[u8; 32]> = Vec::new();
                // Live makers that LEAVE this level during the walk (fully filled / K9-rejected /
                // cancelled by self-trade prevention).
                // The post-walk live count is `count_old - level_removed`; stale ids the walk sweeps
                // (already gone from the book) are NOT counted here — they were decremented when
                // they left. See `finalize_level_count` below.
                let mut level_removed = 0u64;
                let mut qi = 0;

                while qi < queue.len() {
                    if remaining == 0 {
                        // Live orders may remain in the untouched tail — keep it verbatim (any stale
                        // ids ride along and are swept on the next walk).
                        new_queue.extend(queue[qi..].iter().copied());
                        close_level(
                            &mut registry,
                            false,
                            ask_price,
                            new_queue,
                            count_old.saturating_sub(level_removed),
                            &mut removed_asks,
                            &mut ask_levels_cleared,
                        );
                        break 'outer;
                    }
                    let maker_id = queue[qi];
                    qi += 1;

                    let mut maker_order = match storage::load_order(context, &maker_id)? {
                        Some(o)
                            if matches!(
                                o.status,
                                OrderStatus::Open | OrderStatus::PartiallyFilled
                            ) =>
                        {
                            o
                        }
                        // lazy-queue sweep: a cancelled/filled maker was deleted (delete-on-terminal)
                        // but its id lingers in the FIFO — drop it (not re-queued), no count change
                        // (it was decremented when it left the book).
                        _ => continue,
                    };
                    let available = maker_order.quantity - maker_order.filled;
                    let fill_qty = remaining.min(available);

                    let maker_addr = Address::from(maker_order.owner);
                    // ── STP: same ADDRESS only; the TAKER's mode governs; per resting ORDER ─────
                    //
                    // BEFORE the liquidation cap and the maker settle, so an own maker never
                    // reaches `settle_maker_fill_registry` (and with it the K9 guard or a fill).
                    // Nothing here writes: the cancel lands in the registry copies and the deferred
                    // event queue like a K9 cancel, so dropping the outcome undoes it.
                    if let Some(mode) = stp {
                        if maker_addr == taker_addr {
                            if mode.expires_maker() {
                                // The maker's WHOLE remainder is cancelled (Binance), not only
                                // what this taker would have filled; the taker's `remaining` is
                                // untouched.
                                settlement::cancel_maker_in_match(
                                    context,
                                    &mut registry,
                                    maker_addr,
                                    market_id,
                                    Side::Sell,
                                    &maker_id,
                                    &mut maker_order,
                                    market,
                                    CancelReason::SelfTradePrevention,
                                )?;
                                stp_cancels += 1;
                                debug_assert!(
                                    stp_cancels <= crate::trading::algo::MAX_OPEN_ORDERS_PER_MARKET,
                                    "STP cancelled more own orders than the open-order cap admits"
                                );
                                // A live maker left the level, exactly as on the K9 path.
                                level_removed += 1;
                                if !mode.expires_taker() {
                                    // EXPIRE_MAKER: keep walking, deeper if need be.
                                    continue;
                                }
                            } else {
                                // EXPIRE_TAKER: the own maker stays, in its FIFO slot.
                                new_queue.push(maker_id);
                            }
                            // EXPIRE_TAKER / EXPIRE_BOTH: stop the walk here. The untouched tail
                            // stays verbatim, and the level is closed like any other.
                            new_queue.extend(queue[qi..].iter().copied());
                            let count_new = count_old.saturating_sub(level_removed);
                            debug_assert!(
                                mode.expires_maker() || count_new > 0,
                                "an own maker left live must keep its level alive"
                            );
                            close_level(
                                &mut registry,
                                false,
                                ask_price,
                                new_queue,
                                count_new,
                                &mut removed_asks,
                                &mut ask_levels_cleared,
                            );
                            stp_taker_expired = true;
                            // ⚠️ `break 'outer`: the stop is final. An inner `break` would let the
                            // `for` go on and fill the next level (pinned by both suites' T5). A
                            // flag tested at the top of the `while` fills nothing, but when the own
                            // maker is the level's LAST id the `while` ends normally and the `for`
                            // loads the next level and re-saves it untouched: a redundant write in
                            // the block delta, so a different commitment (pinned by
                            // `stp_spec_tests::t5b_*`).
                            break 'outer;
                        }
                    }
                    if liquidation_close
                        && !registry.can_touch_user(maker_addr, MAX_LIQUIDATION_MAKER_ACCOUNTS)
                    {
                        new_queue.push(maker_id);
                        new_queue.extend(queue[qi..].iter().copied());
                        let count_new = count_old.saturating_sub(level_removed);
                        registry.defer_maker_level(false, ask_price, new_queue, count_new)?;
                        break 'outer;
                    }
                    // This fill's notional and its share of the taker's ONE fee, priced BEFORE the
                    // maker settles: the share is the cap on a negative-rate maker's rebate for this
                    // fill (`settlement::entry_fill_plan`). Pure arithmetic over the taker's running
                    // total — nothing is recorded until the maker accepts the fill (a K9-rejected
                    // maker consumes no share). See `TakerSettlement::fee_share_of_next_fill`.
                    let fill_notional = calc_value(
                        ask_price,
                        fill_qty,
                        market.base_decimals,
                        market.price_decimals,
                    )?;
                    let taker_fee_share = taker_settlement.fee_share_of_next_fill(fill_notional)?;
                    // Maker open-solvency guard (K9): settle the maker first. If filling
                    // it would open its position below maintenance at mark — or its account
                    // is frozen by a liquidation — cancel the maker order (drop it from this
                    // level by not re-queuing) and skip — the taker's `remaining` is
                    // untouched so it keeps matching. Funding is settled+persisted inside
                    // either way.
                    let (maker_fee, maker_pos_snapshot, maker_realized_pnl, maker_closed_qty) =
                        match settlement::settle_maker_fill_registry(
                            context,
                            &mut registry,
                            maker_addr,
                            &maker_id,
                            market_id,
                            ask_price,
                            fill_qty,
                            Side::Buy,
                            taker_fee_share,
                            market,
                            maker_order.reduce_only || maker_order.close_position,
                        )? {
                            MakerFillOutcome::Filled {
                                maker_fee,
                                pos_snapshot,
                                realized_pnl,
                                closed_quantity,
                            } => (maker_fee, pos_snapshot, realized_pnl, closed_quantity),
                            MakerFillOutcome::Rejected { reason } => {
                                settlement::cancel_maker_in_match(
                                    context,
                                    &mut registry,
                                    maker_addr,
                                    market_id,
                                    Side::Sell,
                                    &maker_id,
                                    &mut maker_order,
                                    market,
                                    reason,
                                )?;
                                // A live maker left the level (cancelled): count it. Dropped from the
                                // queue (not re-queued); if it was the last live order the count hits 0
                                // and the level is removed below.
                                level_removed += 1;
                                continue;
                            }
                        };
                    // STP watchdog: a governed walk never FILLS against its own owner — the check
                    // above `continue`s or stops on every own maker before it can get here.
                    debug_assert!(
                        stp.is_none() || maker_addr != taker_addr,
                        "self-trade prevention let a taker fill its own resting order"
                    );
                    taker_settlement.record_fill(
                        ask_price,
                        fill_qty,
                        Side::Buy,
                        fill_notional,
                        taker_fee_share,
                        market,
                    )?;
                    last_trade_price = Some(ask_price);
                    registry.push_event(settlement::MatchEvent::Trade {
                        market_id,
                        taker_order_id: *taker_order_id,
                        maker_order_id: maker_id,
                        taker: taker_addr,
                        maker: Address::from(maker_order.owner),
                        price: ask_price,
                        quantity: fill_qty,
                        taker_side: Side::Buy,
                        taker_fee: taker_fee_share,
                        maker_fee,
                        // The maker settled just above, per fill, so its `rp` is final here. The
                        // taker's is not knowable yet (one settlement for the whole order, after
                        // the walk) — `finalize_compute` backfills this `0`.
                        taker_realized_pnl: 0,
                        maker_realized_pnl,
                    });
                    // ── This fill's `ACCOUNT_UPDATE` group for the MAKER ──────────────────────
                    //
                    // Header first, then the position row. The header is derived from the maker's
                    // registry working copy — that copy, not the store, is the maker's state until
                    // the flush — and both land AFTER the fill's `Trade` row, which is therefore
                    // outside the group. The row used to be pushed from inside
                    // `settle_maker_fill_registry`, i.e. before the `Trade`; it now travels back
                    // out on `MakerFillOutcome::Filled` so it can be pushed here instead.
                    registry.push_maker_snapshot(maker_addr)?;
                    registry.push_event(settlement::MatchEvent::PositionChanged {
                        user: maker_addr,
                        pos: maker_pos_snapshot,
                        realized_pnl: maker_realized_pnl,
                        closed_quantity: maker_closed_qty,
                    });

                    // Update the maker order in place — the registry settle does not touch the
                    // maker Order struct, so the value loaded above is still current.
                    maker_order.filled += fill_qty;
                    maker_order.status = if maker_order.filled >= maker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    // delete-on-terminal: a fully-filled maker leaves the map (and the level, via
                    // level_removed); a partial fill stays and is re-queued below.
                    if maker_order.status == OrderStatus::Filled {
                        registry
                            .push_event(settlement::MatchEvent::DeleteOrder { order_id: maker_id });
                        level_removed += 1;
                    } else {
                        registry.push_event(settlement::MatchEvent::SaveOrder {
                            order_id: maker_id,
                            order: maker_order.clone(),
                        });
                    }

                    // Accumulate into the hoisted taker order (saved once after the loop).
                    taker_order.filled += fill_qty;
                    taker_order.status = if taker_order.filled >= taker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };

                    remaining -= fill_qty;
                    if maker_order.status == OrderStatus::PartiallyFilled {
                        new_queue.push(maker_id);
                    }
                }

                // Normal level end (taker still had capacity → consumed every live maker here).
                close_level(
                    &mut registry,
                    false,
                    ask_price,
                    new_queue,
                    count_old.saturating_sub(level_removed),
                    &mut removed_asks,
                    &mut ask_levels_cleared,
                );
            }
            if ask_levels_cleared {
                // New best ask from the walk's own knowledge: the price-index snapshot minus the
                // levels this match emptied — identical to what refresh_best_ask reads after the
                // (deferred) removals are applied.
                let best_ask = ask_prices
                    .iter()
                    .copied()
                    .find(|p| !removed_asks.contains(p))
                    .unwrap_or(0);
                registry.push_event(settlement::MatchEvent::SaveBest {
                    is_bid: false,
                    price: best_ask,
                });
                if best_ask != old_best_ask {
                    let best_bid = storage::load_best_bid(context, market_id)?;
                    registry
                        .push_event(settlement::MatchEvent::MidPriceSample { best_bid, best_ask });
                }
            }
        }
        Side::Sell => {
            // Match against bids (sorted DESC: highest bid first).
            let bid_prices = storage::load_bid_prices_ref(context, market_id)?;
            let old_best_bid = bid_prices.last().copied().unwrap_or(0); // bids: best = max
            let mut bid_levels_cleared = false;
            let mut removed_bids: Vec<u64> = Vec::new();
            'outer: for bid_price in bid_prices.iter().rev().copied() {
                // Fill-time band (upper): a bid above mark+band is an off-mark price
                // (e.g. a closing maker buying rich); skip it — lower, in-band bids may
                // still match. It stays resting until the mark legitimately reaches it.
                if (bid_price as u128) > mark_upper {
                    continue;
                }
                // For limit sell: only match if bid_price >= our limit. A market order has no
                // limit price to respect — the band above/below is its only bound.
                if kind.is_limit() && bid_price < limit_price {
                    break;
                }
                // Fill-time band (lower): bids below mark-band end matching (descending).
                if (bid_price as u128) < mark_lower {
                    break;
                }
                // ONE probe gets both the FIFO ids and the live count (Obs-1 merge).
                let blob = storage::load_bid_level_arc(context, market_id, bid_price)?;
                let count_old = blob.count;
                let queue = &blob.ids;
                let mut new_queue: Vec<[u8; 32]> = Vec::new();
                // See the mirror on the Buy side.
                let mut level_removed = 0u64;
                let mut qi = 0;

                while qi < queue.len() {
                    if remaining == 0 {
                        new_queue.extend(queue[qi..].iter().copied());
                        close_level(
                            &mut registry,
                            true,
                            bid_price,
                            new_queue,
                            count_old.saturating_sub(level_removed),
                            &mut removed_bids,
                            &mut bid_levels_cleared,
                        );
                        break 'outer;
                    }
                    let maker_id = queue[qi];
                    qi += 1;

                    let mut maker_order = match storage::load_order(context, &maker_id)? {
                        Some(o)
                            if matches!(
                                o.status,
                                OrderStatus::Open | OrderStatus::PartiallyFilled
                            ) =>
                        {
                            o
                        }
                        // lazy-queue sweep (see the Buy mirror): stale id → drop, no count change.
                        _ => continue,
                    };
                    let available = maker_order.quantity - maker_order.filled;
                    let fill_qty = remaining.min(available);

                    let maker_addr = Address::from(maker_order.owner);
                    // STP — see the Buy mirror.
                    if let Some(mode) = stp {
                        if maker_addr == taker_addr {
                            if mode.expires_maker() {
                                settlement::cancel_maker_in_match(
                                    context,
                                    &mut registry,
                                    maker_addr,
                                    market_id,
                                    Side::Buy,
                                    &maker_id,
                                    &mut maker_order,
                                    market,
                                    CancelReason::SelfTradePrevention,
                                )?;
                                stp_cancels += 1;
                                debug_assert!(
                                    stp_cancels <= crate::trading::algo::MAX_OPEN_ORDERS_PER_MARKET,
                                    "STP cancelled more own orders than the open-order cap admits"
                                );
                                level_removed += 1;
                                if !mode.expires_taker() {
                                    continue;
                                }
                            } else {
                                new_queue.push(maker_id);
                            }
                            new_queue.extend(queue[qi..].iter().copied());
                            let count_new = count_old.saturating_sub(level_removed);
                            debug_assert!(
                                mode.expires_maker() || count_new > 0,
                                "an own maker left live must keep its level alive"
                            );
                            close_level(
                                &mut registry,
                                true,
                                bid_price,
                                new_queue,
                                count_new,
                                &mut removed_bids,
                                &mut bid_levels_cleared,
                            );
                            stp_taker_expired = true;
                            break 'outer;
                        }
                    }
                    if liquidation_close
                        && !registry.can_touch_user(maker_addr, MAX_LIQUIDATION_MAKER_ACCOUNTS)
                    {
                        new_queue.push(maker_id);
                        new_queue.extend(queue[qi..].iter().copied());
                        let count_new = count_old.saturating_sub(level_removed);
                        registry.defer_maker_level(true, bid_price, new_queue, count_new)?;
                        break 'outer;
                    }
                    // Priced before the maker settles — see the Buy mirror.
                    let fill_notional = calc_value(
                        bid_price,
                        fill_qty,
                        market.base_decimals,
                        market.price_decimals,
                    )?;
                    let taker_fee_share = taker_settlement.fee_share_of_next_fill(fill_notional)?;
                    // Maker open-solvency guard (K9) — see the mirror on the Buy side.
                    let (maker_fee, maker_pos_snapshot, maker_realized_pnl, maker_closed_qty) =
                        match settlement::settle_maker_fill_registry(
                            context,
                            &mut registry,
                            maker_addr,
                            &maker_id,
                            market_id,
                            bid_price,
                            fill_qty,
                            Side::Sell,
                            taker_fee_share,
                            market,
                            maker_order.reduce_only || maker_order.close_position,
                        )? {
                            MakerFillOutcome::Filled {
                                maker_fee,
                                pos_snapshot,
                                realized_pnl,
                                closed_quantity,
                            } => (maker_fee, pos_snapshot, realized_pnl, closed_quantity),
                            MakerFillOutcome::Rejected { reason } => {
                                settlement::cancel_maker_in_match(
                                    context,
                                    &mut registry,
                                    maker_addr,
                                    market_id,
                                    Side::Buy,
                                    &maker_id,
                                    &mut maker_order,
                                    market,
                                    reason,
                                )?;
                                // A live maker left the level (see the Buy mirror): count it.
                                level_removed += 1;
                                continue;
                            }
                        };
                    debug_assert!(
                        stp.is_none() || maker_addr != taker_addr,
                        "self-trade prevention let a taker fill its own resting order"
                    );
                    taker_settlement.record_fill(
                        bid_price,
                        fill_qty,
                        Side::Sell,
                        fill_notional,
                        taker_fee_share,
                        market,
                    )?;
                    last_trade_price = Some(bid_price);
                    registry.push_event(settlement::MatchEvent::Trade {
                        market_id,
                        taker_order_id: *taker_order_id,
                        maker_order_id: maker_id,
                        taker: taker_addr,
                        maker: Address::from(maker_order.owner),
                        price: bid_price,
                        quantity: fill_qty,
                        taker_side: Side::Sell,
                        taker_fee: taker_fee_share,
                        maker_fee,
                        // Sell-side mirror; see the note on the Buy walk.
                        taker_realized_pnl: 0,
                        maker_realized_pnl,
                    });
                    // Sell-side mirror of the buy walk's per-fill maker group (header then row); see
                    // the note there.
                    registry.push_maker_snapshot(maker_addr)?;
                    registry.push_event(settlement::MatchEvent::PositionChanged {
                        user: maker_addr,
                        pos: maker_pos_snapshot,
                        realized_pnl: maker_realized_pnl,
                        closed_quantity: maker_closed_qty,
                    });

                    maker_order.filled += fill_qty;
                    maker_order.status = if maker_order.filled >= maker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    // delete-on-terminal (see the Buy mirror).
                    if maker_order.status == OrderStatus::Filled {
                        registry
                            .push_event(settlement::MatchEvent::DeleteOrder { order_id: maker_id });
                        level_removed += 1;
                    } else {
                        registry.push_event(settlement::MatchEvent::SaveOrder {
                            order_id: maker_id,
                            order: maker_order.clone(),
                        });
                    }

                    taker_order.filled += fill_qty;
                    taker_order.status = if taker_order.filled >= taker_order.quantity {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };

                    remaining -= fill_qty;
                    if maker_order.status == OrderStatus::PartiallyFilled {
                        new_queue.push(maker_id);
                    }
                }

                close_level(
                    &mut registry,
                    true,
                    bid_price,
                    new_queue,
                    count_old.saturating_sub(level_removed),
                    &mut removed_bids,
                    &mut bid_levels_cleared,
                );
            }
            if bid_levels_cleared {
                // New best bid = max non-removed price in the snapshot (mirrors refresh_best_bid
                // reading the index after the deferred removals).
                let best_bid = bid_prices
                    .iter()
                    .rev()
                    .copied()
                    .find(|p| !removed_bids.contains(p))
                    .unwrap_or(0);
                registry.push_event(settlement::MatchEvent::SaveBest {
                    is_bid: true,
                    price: best_bid,
                });
                if best_bid != old_best_bid {
                    let best_ask = storage::load_best_ask(context, market_id)?;
                    registry
                        .push_event(settlement::MatchEvent::MidPriceSample { best_bid, best_ask });
                }
            }
        }
    }

    // ── commit-only #23 L2b: the walk above performed ZERO storage writes (all effects live in
    // the registry copies + ordered events), so every genuine reject here leaves state untouched.

    // 1. FOK: unfillable → reject with zero writes (previously the whole match committed and the
    //    caller's post-hoc check reverted it via undo).
    if kind.is_fok() && remaining > 0 {
        return Err(perp_err("placeOrder: FOK order cannot be fully filled"));
    }

    // 2. Taker settlement compute: K9 open-into-insolvency / wallet-cover / tier /
    //    checked-arithmetic rejects — all pre-write. The taker joins the registry (a self-match
    //    reuses the evolved copies) and its fill effects are flushed with everyone else's, by the
    //    caller. It no longer takes a `RestReq`: the REST is decided once, by `rest_in_book`, also
    //    above the barrier.
    let taker_plan = taker_settlement.finalize_compute(context, &mut registry, side, market)?;

    // ── THE MATCH IS DECIDED, AND NOTHING IS WRITTEN ──
    // The registry, the taker plan and the last trade price are handed BACK, unflushed. The caller
    // owns the write barrier ([`MatchOutcome::apply`]) — see the type's doc comment for why that
    // is not a cosmetic refactor.
    let rest_basis = match &taker_plan {
        Some(plan) => Some(plan.rest_basis()),
        // Zero fills. The taker may still have a working copy (a self-match maker that the K9 guard
        // or self-trade prevention cancelled); `None` means it never joined, so storage is already
        // its post-flush state.
        None => registry.taker_rest_basis(taker_addr, side, market)?,
    };
    Ok(MatchOutcome {
        remaining,
        stp_taker_expired,
        registry,
        taker_plan,
        last_trade_price,
        rest_basis,
    })
}

/// A DECIDED but UNWRITTEN match: everything the walk and the taker-settlement compute phase
/// produced, with not one byte of it committed yet.
///
/// # Why `match_order` no longer flushes
///
/// It used to end with the APPLY block that is now [`Self::apply`]. For a market/IOC/FOK order or a
/// liquidation close that is fine — there is nothing after the match, so the caller flushes
/// immediately and the behaviour is identical. For a **crossing GTC** it was the val0 leak at block
/// 1,098,719: `rest_in_book` runs after the match, it can REFUSE, and off-trie perp writes are
/// commit-only (#23 — there is no `PerpUndo`) while `context.log` is EVM-journaled and truncates on
/// revert. So a refusal after the flush produced a failed receipt with zero logs over silently
/// mutated positions.
///
/// The previous fix expired the remainder so the call succeeded. This one removes the ordering that
/// created the choice: the rest DECISION is write-free (it always was — `rest_in_book`'s two
/// refusals sit above every write in their arms), so hoisting the barrier past it makes a refusal a
/// clean whole-order reject, which is what Binance does (`-2019`) and what the order-lifecycle
/// contract requires (an `OrderCancelled` with no `OrderRested` before it is not a state a
/// projector can reconstruct).
///
/// # What that puts on the deferred side of the barrier
///
/// * `MatchRegistry::flush` — every maker's position/account/order-list write, the book levels, the
///   price index, the best caches, the insurance-fund legs, and the whole deferred LOG stream.
/// * `settlement::finalize_apply` — the taker's cover cancels, its margin+fee debit, the fee
///   credit, its account snapshot and its `PositionChanged`.
/// * `storage::save_last_traded_price` — and this one is the real prize. It is the single input the
///   old pre-check and `rest_in_book` disagreed on: both resolve the Assuming-Price floor
///   `T = max(⌈lastTraded × 1.0015⌉, mark)`, and this write ran BETWEEN them, so the pre-check was
///   systematically optimistic for a crossing SELL. With the write deferred past the decision there
///   is only one evaluation point left, so the divergence is not fixed, it is **structurally
///   impossible**.
///
/// Dropping a `MatchOutcome` without calling [`Self::apply`] discards the whole match. That is
/// exactly what a clean reject wants, and it is safe because the walk performs zero storage writes
/// (see the "commit-only #23 L2b" note above) — there is nothing outside this value to undo.
#[must_use]
pub(super) struct MatchOutcome {
    /// Unfilled quantity after matching.
    pub(super) remaining: u64,
    /// Self-trade prevention STOPPED the walk at the taker's own resting order (`EXPIRE_TAKER` /
    /// `EXPIRE_BOTH`). Implies `remaining > 0` (a walk only meets a maker while it has quantity
    /// left), and tells the caller the remainder EXPIRES with reason 9 — for a GTC too, which then
    /// never rests.
    pub(super) stp_taker_expired: bool,

    registry: settlement::MatchRegistry,
    taker_plan: Option<settlement::TakerPlan>,
    last_trade_price: Option<u64>,
    /// The taker's post-match position + wallet, for a caller that still has to decide whether the
    /// remainder may rest. `None` = the taker was never touched by this match, so storage already
    /// holds its post-flush state.
    pub(super) rest_basis: Option<settlement::RestBasis>,
}

impl MatchOutcome {
    /// Whether a liquidation close stopped on the distinct-maker cap rather than on the book or
    /// the band. Read BEFORE `apply` consumes the outcome.
    pub(super) fn maker_cap_deferred(&self) -> bool {
        self.registry.maker_cap_deferred()
    }

    /// ── THE WRITE BARRIER (no genuine rejects past this point) ──
    ///
    /// Flushes the buffered `OrderPlaced` FIRST so it precedes this order's own Trade /
    /// PositionChanged events, which the registry flush emits — the log stream is byte-identical to
    /// the pre-barrier code, because this is the same four steps in the same order, just called from
    /// one level up.
    ///
    /// Reaching this line means every genuine reject of the placement has already fired, pre-write:
    /// PostOnly-cross, FOK-unfillable, the taker K9 / wallet-cover / tier rejects in
    /// `finalize_compute`, and — new — BOTH of `rest_in_book`'s refusals, which the GTC caller now
    /// resolves before calling this. What is left past this point is invariant / arithmetic guards,
    /// plus `finalize_apply`'s two residuals: the wallet-cover check (an unreachable invariant — the
    /// compute phase simulated the same cancel loop) and `credit_fee_recipient`'s "fee recipient not
    /// initialised", which needs an UNSET admin and is therefore unreachable on any live chain (a
    /// market cannot be added without a non-zero admin, and neither `initAdmin` nor `transferAdmin`
    /// can set one back to zero). Nothing a user can provoke rejects after this line, so
    /// "emitted ⟺ accepted" holds with no exception.
    pub(super) fn apply<H: PerpHost>(
        self,
        context: &mut H,
        side: Side,
        market: &crate::types::Market,
        pending_placed: &mut Option<PendingOrderPlaced>,
    ) -> Result<(), PerpError> {
        emit_pending_order_placed(context, pending_placed);
        self.registry.flush(context, market)?;
        if let Some(plan) = self.taker_plan {
            // The taker's own `AccountBalanceChanged` is published INSIDE `finalize_apply`,
            // immediately before the taker's `PositionChanged`, so the two form one
            // `ACCOUNT_UPDATE` group. See `settlement::finalize_apply`.
            settlement::finalize_apply(context, plan, side, market)?;
        }
        if let Some(price) = self.last_trade_price {
            storage::save_last_traded_price(context, market.market_id, price)?;
        }
        Ok(())
    }
}
