//! Event payloads whose fields are DERIVED rather than stored.
//!
//! Exactly one lives here today: [`PositionChanged`]. It has SEVEN emit sites (the taker
//! settlement, the deferred maker settlement replayed at match flush, the liquidation residual
//! close, both ADL legs, `addPositionMargin` / `removePositionMargin`, and the funding settle),
//! and every one of them must emit its `AccountBalanceChanged` header IMMEDIATELY BEFORE the row —
//! see "THE `ACCOUNT_UPDATE` GROUP INVARIANT" at the bottom of this file, which is enforced at emit
//! time in `debug_assertions` builds.
//!
//! Two of its fields — `entryPrice` and `unrealizedProfit` — are DERIVED from
//! `(amount, vQuoteBalance, mark)` rather than read off the position. Seven hand-rolled copies of
//! an entry-price division is the drift this codebase keeps deleting, so the derivation exists
//! once, here, and every site calls [`emit_position_changed`].
//!
//! Nothing in this module writes storage. Both derived fields come from `perp_core::math`
//! (`calc_entry_price`, `calc_value_i64`) so the log agrees digit-for-digit with what
//! `getPosition` / `getMarginInfo` report for the same state — there is no second definition of
//! either quantity in the engine.
//!
//! [`PositionChanged`]: crate::interface::IPerpDex::PositionChanged

use alloy_primitives::IntoLogData;
use primitives::{Address, Log};

use crate::{
    errors::perp_err,
    host::PerpHost,
    interface::IPerpDex,
    math::{calc_entry_price, calc_value_i64},
    types::{Market, PerpPosition},
    PerpError, PERP_DEX_ADDRESS,
};

/// Emit `PositionChanged` for `pos`, valued at `market`'s CURRENT mark.
///
/// Every caller that holds the threaded `Market` uses this form. `market.mark_price` IS the mark
/// each of those sites is operating at (the settlement paths read `let mark = market.mark_price`
/// verbatim, and `run_update_index_price` re-loads the `Market` after `save_mark_price` precisely
/// so the sweep's band centre and maintenance check are the same number — see the
/// `debug_assert_eq!(market.mark_price, mark_price)` there), so this never re-reads a mark that
/// could differ from the one the caller decided against.
#[inline]
pub(crate) fn emit_position_changed<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    pos: &PerpPosition,
    realized_pnl: i64,
    closed_quantity: u64,
) -> Result<(), PerpError> {
    emit_position_changed_at_mark(
        context,
        user,
        market.market_id,
        pos,
        market.mark_price,
        market.base_decimals,
        market.price_decimals,
        realized_pnl,
        closed_quantity,
    )
}

/// [`emit_position_changed`] for the one caller that does not hold a `Market`:
/// `funding::apply_funding_settlement`, which has already resolved the mark for its own
/// `FundingSettled` payload and passes THAT one, so both logs it emits agree.
///
/// # The two derived fields
///
/// * `entryPrice` = `-vQuoteBalance / amount`, in the market's `priceDecimals` fixed-point units
///   (the same scale every `price` field on this ABI uses), and **0 when flat** as the
///   `ACCOUNT_UPDATE.a.P[].ep` spec requires. `math::calc_entry_price` is the exact algebraic
///   inverse of `calc_value` — `v_quote_balance` is accumulated as `-calc_value(fill, qty)` at
///   fill time, so dividing it back out recovers the size-weighted AVERAGE entry, never the last
///   fill price. Its `amount == 0` early return is what makes the flat case a hard 0 rather than
///   a division.
/// * `unrealizedProfit` = `signedNotional + vQuoteBalance` — the SAME definition, byte for byte,
///   that `margin_view::position_margin_info` reports as `unrealizedProfit` on `getMarginInfo`
///   (see its "── unrealizedProfit ──" block: Binance's `positionAmt × (markPrice − entryPrice)`
///   with the only rounding being the truncation already inside `signedNotional`). It is NOT
///   re-derived from `entryPrice`, which would round twice. Sign follows from that: a long
///   (`amount > 0`) with `mark` above entry has `signedNotional > |vQuoteBalance|` and reports
///   positive; a short (`amount < 0`) has a negative `signedNotional` shrinking in magnitude as
///   the mark falls, so a falling mark reports positive there too.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_position_changed_at_mark<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    pos: &PerpPosition,
    mark_price: u64,
    base_decimals: u32,
    price_decimals: u32,
    realized_pnl: i64,
    closed_quantity: u64,
) -> Result<(), PerpError> {
    let entry_price = calc_entry_price(
        pos.amount,
        pos.v_quote_balance,
        base_decimals,
        price_decimals,
    )?;
    let unrealized_profit = calc_value_i64(mark_price, pos.amount, base_decimals, price_decimals)?
        .checked_add(pos.v_quote_balance)
        .ok_or_else(|| perp_err("PositionChanged: unrealized profit overflow"))?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionChanged {
            user,
            marketId: market_id,
            amount: pos.amount,
            vQuoteBalance: pos.v_quote_balance,
            margin: pos.margin,
            leverage: pos.leverage,
            realizedPnl: realized_pnl,
            closedQuantity: closed_quantity,
            entryPrice: entry_price,
            unrealizedProfit: unrealized_profit,
            // PLACEHOLDERS — always zero. See the ABI comment on `PositionChanged`; do not read
            // these as data and do not populate them from here without adding the state they need.
            cumulativeRealizedPnl: 0,
            breakevenPrice: 0,
        }
        .to_log_data(),
    });
    Ok(())
}

// ── THE `ACCOUNT_UPDATE` GROUP INVARIANT (debug builds only) ─────────────────────────────────────
//
// An indexer assembles a Binance `ACCOUNT_UPDATE` push out of this log stream by ADJACENCY, with no
// ABI fusion: an `AccountBalanceChanged` is the header (`a.B[]`), and the `PositionChanged` rows
// IMMEDIATELY following it are that push's `a.P[]`. Three rules:
//
// 1. every `AccountBalanceChanged` is exactly one push;
// 2. a header is followed by 0..N `PositionChanged` rows belonging to it;
// 3. those rows are CONTIGUOUS, terminated by the first non-`PositionChanged` event.
//
// The two corollaries are the load-bearing part, and they are what this guard enforces:
//
// * every `PositionChanged` in a group carries the header's `user`;
// * there are NO ORPHAN `PositionChanged` rows — every one sits inside some group.
//
// The failure this exists to prevent is silent and worse than a lost event: an orphan
// `PositionChanged(Y)` trailing user X's group makes an indexer attribute Y's position to X's
// account update. A 0-position group is legitimate (a deposit, a transfer, the fee recipient).
//
// ## Why the check is a STREAMING one at the emit site rather than a slice walk at the drain
//
// All three rules plus both corollaries reduce to ONE piece of state — "which user's group is
// currently open" — so the whole invariant is decidable incrementally as each log is emitted:
//
// | emitted                   | effect                                        |
// |---------------------------|-----------------------------------------------|
// | `AccountBalanceChanged(u)`| open a group for `u`                          |
// | `PositionChanged(u)`      | ASSERT a group for `u` is open                |
// | anything else (this address) | close the open group                       |
//
// Hooking that into [`crate::host::PerpHost::log`] — the single choke point every engine log goes
// through, on BOTH host impls — is what makes the coverage total. The alternative the design asked
// for, a slice walk where the drain runs, only sees calls that actually reach
// `storage::flush_account_snapshots`: the shell always does, but most of the 483 tests drive an
// engine handler DIRECTLY (`run_place_order`, `liquidate_position`, `run_add_position_margin`) and
// never open or close a call, so exactly the liquidation / ADL / funding streams being restructured
// here would have gone unchecked — the same blind spot that made the hand-attached
// `assert_snapshots_close_pending_rows` too weak. Emitting is the one event no path can skip.
//
// ## Log truncation is not a hazard
//
// `checkpoint_revert` truncates a SUFFIX of the log vector, so a `PositionChanged` that survives a
// revert keeps the header immediately before it (headers are adjacent to their rows by construction).
// The only way stale state could produce a FALSE failure is a reverted range that both opens/closes a
// group and is followed by more logs inside the same group — and the one production `checkpoint_revert`
// (`risk::run_liquidation_sweep`) only reverts `AboveMaintenance` / `NoPosition` candidates, which
// return before `apply_funding_settlement` and therefore emit ZERO logs. Belt-and-braces,
// `storage::begin_perp_call` resets the state, so no group can ever span two calls; that is strictly
// STRICTER (a call whose first `PositionChanged` is an orphan still fails).
//
// Non-PerpDEX logs are ignored entirely rather than treated as terminators: an indexer filters this
// stream by address, so a foreign log cannot end a group it cannot see.

/// Resets the group-invariant guard at a call boundary. Called by `storage::begin_perp_call`.
#[inline]
pub(crate) fn reset_log_group_guard() {
    #[cfg(debug_assertions)]
    group_guard::reset();
}

/// Feeds one emitted log to the group-invariant guard. Called by both `PerpHost::log` impls.
///
/// `_log` is deliberately underscored: with `debug_assertions` off this is an empty `#[inline]`
/// function and the parameter is genuinely unused, which must not add a warning to a release build.
#[inline]
pub(crate) fn observe_log_for_group_invariant(_log: &Log) {
    #[cfg(debug_assertions)]
    group_guard::observe(_log);
}

#[cfg(debug_assertions)]
mod group_guard {
    use alloy_sol_types::SolEvent;
    use primitives::{Address, Log};

    use crate::{interface::IPerpDex, PERP_DEX_ADDRESS};

    std::thread_local! {
        /// The user whose `ACCOUNT_UPDATE` group is currently open, if any. Thread-local rather
        /// than threaded through the host: the state is per LOG STREAM, and a log stream belongs to
        /// one transaction executing on one thread.
        static OPEN_GROUP: core::cell::Cell<Option<Address>> =
            const { core::cell::Cell::new(None) };
    }

    pub(super) fn reset() {
        OPEN_GROUP.with(|g| g.set(None));
    }

    pub(super) fn observe(log: &Log) {
        if log.address != PERP_DEX_ADDRESS {
            return;
        }
        let topics = log.data.topics();
        let Some(topic0) = topics.first().copied() else {
            OPEN_GROUP.with(|g| g.set(None));
            return;
        };
        // Both events declare `address indexed user` first, so the subject is topic[1] with no
        // ABI decode of the data half.
        let subject = || {
            Address::from_word(
                *topics
                    .get(1)
                    .expect("AccountBalanceChanged / PositionChanged declare `indexed user`"),
            )
        };
        if topic0 == IPerpDex::AccountBalanceChanged::SIGNATURE_HASH {
            OPEN_GROUP.with(|g| g.set(Some(subject())));
        } else if topic0 == IPerpDex::PositionChanged::SIGNATURE_HASH {
            let user = subject();
            let open = OPEN_GROUP.with(|g| g.get());
            match open {
                Some(header) if header == user => {}
                Some(header) => panic!(
                    "ACCOUNT_UPDATE group invariant: PositionChanged({user}) inside the group \
                     opened by AccountBalanceChanged({header}) — an indexer would attribute this \
                     position to the WRONG account. Every PositionChanged in a group must carry \
                     the header's user."
                ),
                None => panic!(
                    "ACCOUNT_UPDATE group invariant: ORPHAN PositionChanged({user}) — no \
                     AccountBalanceChanged({user}) immediately precedes it, so this row belongs to \
                     no ACCOUNT_UPDATE push (or, worse, trails an unrelated user's group). Emit the \
                     account header FIRST, immediately before the position rows it owns."
                ),
            }
        } else {
            OPEN_GROUP.with(|g| g.set(None));
        }
    }
}

/// Shared test support for the group invariant: the slice-form assertion and the `(event, subject)`
/// stream projection the ordered-stream tests pin. Lives here rather than in one test module because
/// both `trading::tests` and `risk::tests` assert on it — `risk::tests` owns the liquidation and ADL
/// fixtures.
#[cfg(test)]
pub(crate) mod stream_test_support {
    use alloy_sol_types::SolEvent;
    use primitives::{Address, U256};

    use crate::interface::IPerpDex::{
        AccountBalanceChanged, FundingSettled, PositionChanged, Trade,
    };

    /// **THE STREAM INVARIANT: every `AccountBalanceChanged` is one `ACCOUNT_UPDATE` push, and the
    /// `PositionChanged` rows immediately after it are that push's `a.P[]`.**
    ///
    /// This replaces `assert_snapshots_close_pending_rows`, whose "a snapshot for X closes X's
    /// pending rows" framing was built on the header coming LAST — a money row made a user pending
    /// and a later snapshot answered it. The header now comes FIRST, immediately before the rows it
    /// owns, so under that old reading every money row is left permanently unanswered. The
    /// pending-row bookkeeping and the `money_row_users` helper it needed are gone with it.
    ///
    /// Two legs survive, and neither is subsumed by the other:
    ///
    /// 1. **The grouping.** A header opens a group; each following `PositionChanged` must carry that
    ///    header's user; the first non-`PositionChanged` event closes the group. This is the same
    ///    property the `#[cfg(debug_assertions)]` guard in `crate::events` enforces at EMIT time for
    ///    the whole suite — asserted again here because a test reading these assertions should be
    ///    able to see the invariant it is testing, and because the message names the log index.
    /// 2. **Duplicate detection**, carried over verbatim in substance: a group with ZERO position
    ///    rows that also repeats the last payload published for that user carries no news, and is the
    ///    duplicate this design has to avoid — a maker who got a per-fill event and then a drain
    ///    event because the mark was never cleared.
    ///
    /// A group that closes nothing is legitimate ON ITS OWN — the fee recipient's wallet moves with
    /// no `Trade` or `PositionChanged` naming them, and deposits / withdrawals / transfers are
    /// 0-position groups by nature — which is exactly why leg 2 is gated on the payload REPEATING
    /// rather than on the group being empty.
    ///
    /// ⚠️ The fee-recipient LOSS branch (the flush's mark-clear gate dropping a later `credit_admin`
    /// credit) is invisible to both legs above: the dropped row carries a fee credit, and a fee
    /// credit to the admin is named by no `Trade` / `PositionChanged` / `FundingSettled`, so nothing
    /// structural notices its absence. Its ONLY coverage is
    /// `a_fee_recipient_who_is_also_a_maker_gets_a_drain_row_for_the_later_fee`. Do not delete that
    /// test believing this generic invariant subsumes it.
    pub(crate) fn assert_account_update_groups(logs: &[primitives::Log]) {
        /// The three BALANCES only — `reason` is deliberately NOT part of the duplicate key, which
        /// keeps leg 2 as strict as it was: a row that repeats a payload is a duplicate whether or
        /// not something relabelled it. (Reasons are asserted separately, per stream, by
        /// [`account_update_reasons`].)
        type Payload = (U256, i64, i64);
        /// One open group: `(user, header log index, its payload, rows attached so far)`.
        type Group = (Address, usize, Payload, usize);

        // The payload of the last COMPLETED group per user — the "previous" a duplicate repeats.
        let mut last: std::collections::BTreeMap<Address, Payload> = Default::default();
        // A group is finalised when the next header, a terminator, or the end of the stream ends it;
        // only then is its row count final, so leg 2 has to run here and nowhere else.
        let close = |g: Option<Group>, last: &mut std::collections::BTreeMap<Address, Payload>| {
            if let Some((user, header, payload, rows)) = g {
                assert!(
                    rows > 0 || last.get(&user) != Some(&payload),
                    "log[{header}]: a group for {user} carries zero position rows AND repeats that \
                     user's previous payload — a duplicate with no news in it (the drain repeating \
                     a direct emit whose mark was not cleared)"
                );
                last.insert(user, payload);
            }
        };

        let mut open: Option<Group> = None;
        for (i, log) in logs.iter().enumerate() {
            let topic = log.data.topics().first().copied();
            if topic == Some(AccountBalanceChanged::SIGNATURE_HASH) {
                let e = AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data)
                    .unwrap();
                close(open.take(), &mut last);
                open = Some((
                    e.user,
                    i,
                    (
                        e.usdcBalance,
                        e.totalWalletBalance,
                        e.totalCrossWalletBalance,
                    ),
                    0,
                ));
            } else if topic == Some(PositionChanged::SIGNATURE_HASH) {
                let row = PositionChanged::decode_raw_log(log.data.topics(), &log.data.data)
                    .unwrap()
                    .user;
                match open.as_mut() {
                    Some((user, _, _, rows)) => {
                        assert_eq!(
                            *user, row,
                            "log[{i}]: PositionChanged({row}) sits inside {user}'s group — an \
                             indexer would attribute this position to the WRONG account"
                        );
                        *rows += 1;
                    }
                    None => panic!(
                        "log[{i}]: ORPHAN PositionChanged({row}) — no AccountBalanceChanged({row}) \
                         immediately precedes it, so it belongs to no ACCOUNT_UPDATE push"
                    ),
                }
            } else {
                close(open.take(), &mut last);
            }
        }
        close(open.take(), &mut last);
    }

    /// The ordered `(subject, reason)` sequence of the `AccountBalanceChanged` headers in a stream —
    /// i.e. every `ACCOUNT_UPDATE` push this call produced and WHY.
    ///
    /// Reasons are asserted on real streams rather than at the encoding layer on purpose: the field
    /// is only worth anything if the value a production path actually emits is the right one, and
    /// the drain's value comes from a mark recorded several functions away from the row it labels
    /// (`storage::mark_account_snapshot_dirty`). `from_code` returning `Option` is load-bearing here
    /// — a code no build defines fails the `expect` instead of being folded onto a neighbour.
    pub(crate) fn account_update_reasons(
        logs: &[primitives::Log],
    ) -> Vec<(Address, crate::types::AccountUpdateReason)> {
        logs.iter()
            .filter(|log| {
                log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH)
            })
            .map(|log| {
                let e =
                    AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data)
                        .unwrap();
                (
                    e.user,
                    crate::types::AccountUpdateReason::from_code(e.reason)
                        .expect("AccountBalanceChanged.reason must be a defined wire code"),
                )
            })
            .collect()
    }

    /// The ordered `(event name, subject)` sequence of the PerpDEX log stream, for the tests that
    /// pin a whole stream rather than a count. `subject` is the first indexed field where it is an
    /// address (`user` / `taker`), `None` where the event has no single natural subject.
    pub(crate) fn stream_shape(logs: &[primitives::Log]) -> Vec<(&'static str, Option<Address>)> {
        use crate::interface::IPerpDex::{
            Adl, FundingRateComputed, IndexPriceUpdated, InsuranceFundChanged,
            InsuranceFundDepleted, Liquidation, MarkPriceUpdated, OrderCancelled, OrderPlaced,
            OrderRested, PositionMarginAdjusted,
        };
        logs.iter()
            .filter_map(|log| {
                let t = log.data.topics();
                let topic = t.first().copied()?;
                let indexed_user = || t.get(1).map(|w| Address::from_word(*w));
                let named = |n: &'static str| Some((n, indexed_user()));
                if topic == AccountBalanceChanged::SIGNATURE_HASH {
                    named("AccountBalanceChanged")
                } else if topic == PositionChanged::SIGNATURE_HASH {
                    named("PositionChanged")
                } else if topic == Trade::SIGNATURE_HASH {
                    // `marketId` is the only indexed field; the taker lives in the data half.
                    let e = Trade::decode_raw_log(t, &log.data.data).unwrap();
                    Some(("Trade", Some(e.taker)))
                } else if topic == FundingSettled::SIGNATURE_HASH {
                    let e = FundingSettled::decode_raw_log(t, &log.data.data).unwrap();
                    Some(("FundingSettled", Some(e.user)))
                } else if topic == Adl::SIGNATURE_HASH {
                    named("Adl")
                } else if topic == Liquidation::SIGNATURE_HASH {
                    named("Liquidation")
                } else if topic == OrderPlaced::SIGNATURE_HASH {
                    named("OrderPlaced")
                } else if topic == OrderRested::SIGNATURE_HASH {
                    named("OrderRested")
                } else if topic == OrderCancelled::SIGNATURE_HASH {
                    named("OrderCancelled")
                } else if topic == PositionMarginAdjusted::SIGNATURE_HASH {
                    named("PositionMarginAdjusted")
                } else if topic == InsuranceFundChanged::SIGNATURE_HASH {
                    Some(("InsuranceFundChanged", None))
                } else if topic == InsuranceFundDepleted::SIGNATURE_HASH {
                    Some(("InsuranceFundDepleted", None))
                } else if topic == FundingRateComputed::SIGNATURE_HASH {
                    Some(("FundingRateComputed", None))
                } else if topic == IndexPriceUpdated::SIGNATURE_HASH {
                    Some(("IndexPriceUpdated", None))
                } else if topic == MarkPriceUpdated::SIGNATURE_HASH {
                    Some(("MarkPriceUpdated", None))
                } else {
                    Some(("other", None))
                }
            })
            .collect()
    }
}

/// The guard asserting on ITSELF. Without these, "the invariant is enforced" rests on the assertion
/// being written correctly, and a helper that accepts everything passes every test in the suite —
/// which is exactly how the predecessor helper (`assert_snapshots_close_pending_rows`) ended up blind
/// to the streams it was supposed to cover.
#[cfg(test)]
mod group_invariant_self_tests {
    use alloy_primitives::IntoLogData;
    use primitives::{address, Address, Log};

    use super::stream_test_support::assert_account_update_groups;
    use crate::{interface::IPerpDex, PERP_DEX_ADDRESS};

    const X: Address = address!("1111111111111111111111111111111111111111");
    const Y: Address = address!("2222222222222222222222222222222222222222");

    fn header(user: Address, wb: i64) -> Log {
        Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::AccountBalanceChanged {
                user,
                // Any reason serves here: these self-tests are about the GROUPING, and the reason
                // is not part of the grouping rule.
                reason: crate::types::AccountUpdateReason::Order.code(),
                usdcBalance: primitives::U256::ZERO,
                totalWalletBalance: wb,
                totalCrossWalletBalance: wb,
            }
            .to_log_data(),
        }
    }

    fn row(user: Address) -> Log {
        Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::PositionChanged {
                user,
                marketId: 1,
                amount: 1,
                vQuoteBalance: -1,
                margin: 1,
                leverage: 1,
                realizedPnl: 0,
                closedQuantity: 0,
                entryPrice: 1,
                unrealizedProfit: 0,
                cumulativeRealizedPnl: 0,
                breakevenPrice: 0,
            }
            .to_log_data(),
        }
    }

    fn terminator() -> Log {
        Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::InsuranceFundChanged {
                delta: 1,
                newBalance: 1,
            }
            .to_log_data(),
        }
    }

    /// The shapes that MUST be accepted: a well-formed group, a 0-position group (a deposit, the fee
    /// recipient), and two 0-position groups for the same user whose payloads differ (the liquidation
    /// end state after its close).
    #[test]
    fn the_legitimate_shapes_are_accepted() {
        assert_account_update_groups(&[header(X, 1), row(X), row(X), terminator()]);
        assert_account_update_groups(&[header(X, 1), terminator(), header(Y, 1)]);
        assert_account_update_groups(&[header(X, 1), terminator(), header(X, 2)]);
    }

    #[test]
    #[should_panic(expected = "ORPHAN PositionChanged")]
    fn an_orphan_row_is_rejected() {
        assert_account_update_groups(&[header(X, 1), terminator(), row(X)]);
    }

    /// The failure the whole design exists to prevent: Y's row trailing X's group.
    #[test]
    #[should_panic(expected = "WRONG account")]
    fn a_row_inside_another_users_group_is_rejected() {
        assert_account_update_groups(&[header(X, 1), row(Y)]);
    }

    #[test]
    #[should_panic(expected = "a duplicate")]
    fn a_zero_position_group_repeating_the_previous_payload_is_rejected() {
        assert_account_update_groups(&[header(X, 1), row(X), terminator(), header(X, 1)]);
    }
}
