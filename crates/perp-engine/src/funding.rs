//! Lazy per-position funding settlement.
//!
//! A per-market cumulative funding index (`FundingState.cumulative_funding_index`)
//! accumulates `mark_price * funding_rate` at each funding-epoch boundary (see
//! `risk::run_update_index_price`). Each position records the index value at its
//! last settlement (`PerpPosition.last_funding_index`).
//!
//! Funding owed since then is `calc_funding_payment(amount, index_delta, …)` and
//! is settled **lazily**: every operation that touches a position (trade fills,
//! margin add/remove, liquidation, and each counterparty an ADL fill closes) settles
//! it on the loaded position *before* mutating it — [`settle_position_funding`], or
//! [`compute_funding_settlement`] + [`apply_funding_settlement`] where the caller can
//! still refuse — so the entire accrual is realised against the `amount` that was
//! actually held over the period.
//!
//! Settlement direction (matches `calc_funding_payment`'s sign): for an ISOLATED position
//! funding settles against the POSITION, never the account-global perp wallet (a CROSSED one is
//! the opposite — see the section below). The signed payment is
//! added to the position's isolated `margin`, WHOLE, in both directions: a credit
//! raises it, a charge lowers it — through zero if it has to. **`margin` may go
//! NEGATIVE.** A negative margin is a debt the position owes; nothing else pays it:
//!
//! * it lowers equity (`notional + vQuote + margin`), so the maintenance check sees it
//!   and a position that keeps paying funding is eventually liquidated, even one held
//!   up by unrealized profit;
//! * a close nets it against the realized PnL (`settlement::apply_position_fill`
//!   releases margin SIGNED, so the closed slice carries its share of the debt);
//! * a shortfall at close or liquidation goes through the ordinary bad-debt path: an
//!   ADL residual's is absorbed by opposite holders at the bankruptcy price, and only
//!   what no representable price can place (`run_adl`'s `p_b == 0` regime, the
//!   fallback tier) or a book close cannot pay reaches the insurance fund.
//!
//! It replaces a rule that took a charge from `margin` only down to 0 and drew the
//! remainder from the insurance fund on the spot. Past that point funding no longer
//! lowered equity, so a position solvent on unrealized profit was never liquidated,
//! the fund paid its funding every epoch, and the close paid the user the whole profit
//! without recovering any of it.
//!
//! # A position CROSSED in force (cross margin P2)
//!
//! Everything above describes an ISOLATED position. A CROSSED one has no silo (`pos.margin == 0`,
//! asserted at `storage::save_position`), so its funding settles into the account's CROSS WALLET
//! (`perp_wallet_balance`), either sign, through zero if it must — Binance's cross model, where
//! FUNDING FEE moves `crossWalletBalance` and nothing else. Until it is settled the account fold
//! already counts it (`cWB = W + Σ_C PF`, `margin_view::cross_fold`), so settling moves no
//! account-level quantity: it only turns a pending amount into a booked one.
//!
//! Its events are Binance's B-only push (`ACCOUNT_UPDATE m=FUNDING_FEE` "without any position P
//! message", cross report F32): `FundingSettled`, then the `AccountBalanceChanged(FUNDING_FEE)`
//! header carrying the moved wallet — and NO `PositionChanged`, because no field of the position
//! row moved. A path that can settle funding on a cross position must therefore hand
//! [`compute_funding_settlement`] the account it is going to persist.
//!
//! This is what makes isolated margin actually isolated, and it is Binance's
//! measured behaviour: funding hits the position's isolated wallet directly and
//! leaves the cross/free wallet at `0E-8`. Because `margin` is exactly the term the
//! maintenance check reads, funding now MOVES the liquidation price by the verified
//! closed form `dLP = funding / (qty * (MMR - 1))`, i.e. a losing funding stream can
//! push a position into liquidation instead of being subsidised indefinitely by a
//! well-funded wallet that also backs every OTHER market's orders.
//!
//! ⚠️ That `dLP` form is a MEASURED closed form (`binance-margin-verified-model.md` §1.3: predicted
//! `3.7878012048…` vs observed `3.78780120`, error 4.8e-9 — sub-ulp) and is unaffected by §3.7's
//! "component form only" rule, which voids the FLIP-GAP closed forms (`x*` and friends), not this
//! one. It is quoted here only to explain the mechanism: nothing in this module computes an `LP`.
//! Its DENOMINATOR SIGN on a SHORT position is extrapolated, like everything short-side
//! (§6, 「空头侧符号」 — a blocking open item since docs commit `8d179c0`); no code depends on it.
//!
//! ⚠️ Letting `margin` run NEGATIVE is OUR choice, not a measured Binance behaviour. Binance's one
//! funding measurement charged 0.06% of `isolatedWallet`, nowhere near zero, and no evidence covers
//! a charge larger than the silo. What it is consistent with: the closed form is affine in `iw`
//! with no floor, so it stays linear through `iw = 0`. The clamp this replaced made `dLP/dF = 0`
//! below zero.

use alloy_primitives::IntoLogData;
use crate::host::PerpHost;
use primitives::{Address, Log};

use crate::{
        errors::{perp_err, perp_invariant_err},
    interface::IPerpDex,
    math::calc_funding_payment,
    storage,
    types::{AccountUpdateReason, Market, PerpPosition},
    PERP_DEX_ADDRESS,
    PerpError,
};

/// A funding settlement computed in memory but NOT yet written (commit-only #23). The margin
/// credit/charge has already been applied to the caller's in-memory `pos`; the `FundingSettled`
/// payload is carried here for a later [`apply_funding_settlement`], which only LOGS — funding
/// writes nothing of its own. The split lets a caller REJECT between the computation and the
/// commit with no event emitted, and places the funding group exactly where the caller wants it in
/// the event stream.
pub(crate) struct PendingFunding {
    payment: i64,
    funding_rate: i64,
    user: Address,
    market_id: u64,
    /// The position AFTER the funding credit/charge landed on `margin`, snapshotted at compute
    /// time. Carried so [`apply_funding_settlement`] can emit the `PositionChanged` that pairs
    /// with `FundingSettled` — see the note there for why funding needs one at all. All-scalar
    /// (`PerpPosition` is 9 integers), and only built when `payment != 0`.
    pos: PerpPosition,
    /// `market.{base_decimals, price_decimals}` from the compute-time `Market`, so the apply half
    /// can value `unrealizedProfit` without a second market load. Immutable per market.
    base_decimals: u32,
    price_decimals: u32,
    /// The position is CROSSED in force: the payment landed on the caller's account wallet, not
    /// on `pos.margin`, and [`apply_funding_settlement`] publishes the group WITHOUT a position row
    /// (Binance's B-only funding push — see the module docs).
    cross: bool,
    /// The `AccountBalanceChanged` HEADER for the funding group, captured at compute time.
    ///
    /// # Why it has to be carried rather than read at apply time
    ///
    /// The funding `PositionChanged` needs an account header IMMEDIATELY before it (the
    /// `ACCOUNT_UPDATE` group invariant — see `crate::events`), and at the instant
    /// [`apply_funding_settlement`] runs, the store does NOT hold the post-funding position on any
    /// caller's path: the margin credit/charge landed on the caller's in-memory `pos` and the
    /// `save_position` that persists it comes later (later still on the match path, where the
    /// funding replays out of `MatchRegistry::flush` before the flush writes anything). A header
    /// folded from storage there would report the PRE-funding silo against a post-funding position
    /// row — the exact half-updated snapshot this event's contract forbids.
    ///
    /// So it is derived where the post-funding state actually exists, from
    /// `margin_view::wallet_balances_from_parts` on the compute-time account plus the post-funding
    /// `pos.margin`. An ISOLATED settle touches NO wallet leg (it settles against `pos.margin`
    /// only), so the account half of the payload is the caller's unmodified account and only the
    /// Σ leg moves; a CROSSED settle is the mirror image — the Σ leg is untouched and the cross
    /// wallet carries the payment, on the caller's account, which is the copy it persists.
    snapshot: crate::margin_view::AccountWalletBalances,
}

impl PendingFunding {
    /// The header payload this settlement will publish. Exposed for `MatchRegistry::get_or_load`,
    /// which records it as the user's `last_published` so the flush's mark-clear gate can tell that
    /// the end-of-call drain has nothing to add for a user whose ONLY event was this funding group
    /// (an insolvency-rejected maker is exactly that case).
    pub(crate) fn snapshot(&self) -> &crate::margin_view::AccountWalletBalances {
        &self.snapshot
    }
}

/// The funding `pos` owes or is owed at cumulative index `index`, NOT yet settled: the signed
/// payment [`compute_funding_settlement`] would add to `pos.margin`. Zero for a flat position or
/// one already anchored at `index`.
///
/// The ONE place the pending amount is derived. Exposed so a reader that must judge a position's
/// EFFECTIVE equity without settling it — ADL ranking its counterparties — computes exactly the
/// number the settlement will apply.
pub(crate) fn pending_funding_payment(
    index: i128,
    pos: &PerpPosition,
    market: &Market,
) -> Result<i64, PerpError> {
    pending_funding_of(index, pos.amount, pos.last_funding_index, market)
}

/// [`pending_funding_payment`] from the two position fields it reads, `amount` and
/// `last_funding_index` — the form a cross leg on the account carries them in
/// (`types::CrossLegState`, priced by `margin_view::cross_health`, cross P4). The same
/// expression, so the two can never disagree.
pub(crate) fn pending_funding_of(
    index: i128,
    amount: i64,
    last_funding_index: i128,
    market: &Market,
) -> Result<i64, PerpError> {
    if amount == 0 || last_funding_index == index {
        return Ok(0);
    }
    let delta = index
        .checked_sub(last_funding_index)
        .ok_or_else(|| perp_err("funding: index delta overflow"))?;
    calc_funding_payment(amount, delta, market.base_decimals, market.price_decimals)
}

/// Pure funding computation: reads funding state, settles the signed funding payment IN MEMORY,
/// re-anchors `last_funding_index`, and returns the log payload (`None` if there was no funding
/// event). Performs NO storage writes, so it is safe to call before a validation reject.
///
/// Where the payment lands depends on the position's margin type IN FORCE:
///
/// * ISOLATED — on the in-memory `pos.margin` (which a charge may drive negative — see the module
///   docs). The caller's `UserAccount` is untouched, and `account` below is only READ.
/// * CROSSED — on `account.perp_wallet_balance`, the cross wallet, either sign; `pos.margin` stays
///   0. `account` is then REQUIRED: it is the copy the caller persists, so the payment is saved
///   with it. A cross settle without one is refused as an `[INVARIANT]` (it would drop the payment
///   on the floor); every path that can reach a cross position passes its account — the match
///   registry, ADL winners, `add`/`removePositionMargin`, and the cross liquidation episode
///   (`risk::cross_liquidation`), which settles each market before closing it — and the ISOLATED
///   liquidation path never sees one (both its callers route a cross position to the episode).
///
/// `account` is the caller's already-loaded copy, used to build the group header
/// ([`PendingFunding::snapshot`]) without a second load; for an ISOLATED position pass `None` and
/// it is loaded lazily, and only when funding actually moved money (`payment != 0`), so a
/// write-free scan such as the liquidation sweep's healthy-candidate path still touches no account
/// blob. It must be the state the caller is going to PERSIST for this user (the registry path folds
/// its pending fee credits in before calling), and `pos.margin` on entry must be this market's
/// STORED margin — both hold on every call site, because funding is always the first thing a path
/// settles.
pub(crate) fn compute_funding_settlement<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    pos: &mut PerpPosition,
    account: Option<&mut crate::types::UserAccount>,
) -> Result<Option<PendingFunding>, PerpError> {
    let funding = storage::load_funding_state(context, market.market_id)?;
    let index = funding.cumulative_funding_index;
    // The index's MIRROR on the stored `Market` (what the account folds price pending funding from,
    // without loading `FundingState`) must equal the source of truth read just above. Compared
    // against a FRESH read of the stored blob, not the caller's `market`, which may be a copy taken
    // earlier in the call. `storage::save_funding_state` is the one writer of both.
    #[cfg(debug_assertions)]
    if let Some(stored) = storage::load_market_ref(context, market.market_id)? {
        debug_assert_eq!(
            stored.cumulative_funding_index, index,
            "funding: market {} mirrors cumulative funding index {} but FundingState holds {index}",
            market.market_id, stored.cumulative_funding_index
        );
    }

    let mut pending = None;
    let payment = pending_funding_payment(index, pos, market)?;
    if payment != 0 {
        // `pos.margin` BEFORE funding moves it — the anchor for the header's Σ leg. Pre-funding is
        // the right anchor precisely because the stored aggregate is pre-funding too.
        let margin_before = pos.margin;
        let cross = crate::margin_type::effective_margin_type(pos)
            == crate::types::MarginType::Crossed;
        let snapshot = if cross {
            // CROSSED: the whole payment, either sign, lands on the cross wallet — through zero if
            // it must, exactly like a realized loss (`settlement::apply_position_fill`). The
            // position has no silo to put it in.
            let Some(account) = account else {
                return Err(perp_invariant_err(
                    "funding: a CROSSED position settled without its account (the payment \
                     belongs to the cross wallet)",
                ));
            };
            account.perp_wallet_balance = account
                .perp_wallet_balance
                .checked_add(payment)
                .ok_or_else(|| perp_err("funding: cross wallet overflow"))?;
            crate::margin_view::wallet_balances_from_parts(
                account,
                account
                    .total_position_margin
                    .checked_sub(margin_before)
                    .ok_or_else(|| perp_err("funding: Σ position margin underflow"))?,
                pos.margin,
            )?
        } else {
            // ISOLATED: the WHOLE payment, either sign, lands on the position's own margin. A
            // credit makes the receiver safer; a charge bites the position that owes it — through
            // zero if it must, because a debt that stopped lowering equity at zero is precisely the
            // subsidy this rule removed (module docs). No wallet leg, no insurance-fund leg.
            pos.margin = pos
                .margin
                .checked_add(payment)
                .ok_or_else(|| perp_err("funding: position margin overflow"))?;
            // The group header, off the post-funding position. Lazy account load: only reached
            // when funding really moved money.
            let loaded;
            let account: &crate::types::UserAccount = match account {
                Some(a) => a,
                None => {
                    loaded = storage::load_account_ref(context, user)?;
                    &loaded
                }
            };
            crate::margin_view::wallet_balances_from_parts(
                account,
                account
                    .total_position_margin
                    .checked_sub(margin_before)
                    .ok_or_else(|| perp_err("funding: Σ position margin underflow"))?,
                pos.margin,
            )?
        };
        pending = Some(PendingFunding {
            payment,
            funding_rate: funding.last_funding_rate,
            user,
            market_id: market.market_id,
            snapshot,
            // Post-funding snapshot. `last_funding_index` is re-anchored below and is not a field
            // of `PositionChanged`, so taking it here loses nothing the log reports.
            pos: pos.clone(),
            base_decimals: market.base_decimals,
            price_decimals: market.price_decimals,
            cross,
        });
    }

    // Always re-anchor — covers flat positions and freshly opened legs so they
    // do not retroactively accrue funding from before they existed.
    pos.last_funding_index = index;
    Ok(pending)
}

/// Commits a [`PendingFunding`]: emits the `FundingSettled` log and its `ACCOUNT_UPDATE` group.
/// Writes NOTHING — the margin move is persisted by the caller's own `save_position` — but it MUST
/// still run only after the caller has decided to commit (all rejects passed), so a refused call
/// publishes no funding events.
pub(crate) fn apply_funding_settlement<H: PerpHost>(
    context: &mut H,
    p: PendingFunding,
) -> Result<(), PerpError> {
    let mark_price = storage::load_mark_price(context, p.market_id)?;
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::FundingSettled {
            marketId: p.market_id,
            user: p.user,
            fundingRate: p.funding_rate,
            amount: p.payment,
            markPrice: mark_price,
        }
        .to_log_data(),
    });

    // ── Why funding also emits `PositionChanged` ─────────────────────────────────────────────
    // Funding moves `pos.margin`, which is exactly `ACCOUNT_UPDATE.a.P[].iw`. The docs list
    // `FundingSettled` as a source for `ACCOUNT_UPDATE`, and that payload now CARRIES the position
    // array (the `@position` stream was removed), so a funding settle that emitted no position row
    // would publish a `B[]` with no `P[]` entry for the position whose `iw` just changed —
    // `save_position` marks the account snapshot dirty, so the account update fires either way.
    //
    // On most paths a later `PositionChanged` would have covered it (a fill, a margin add/remove,
    // a liquidation close), but NOT on all of them: a maker whose fill is rejected as
    // open-into-insolvency (`MakerFillOutcome::Rejected`) has its funding computed at
    // `MatchRegistry::get_or_load`, flushed to storage with the rest, and no `PositionChanged`
    // pushed for it. Emitting from here — the ONE place that knows funding actually moved money —
    // closes that hole for every caller at once instead of asking each of them to remember.
    //
    // The alternative (let the indexer derive the new `iw` by applying `amount` to its own last
    // known value) would now work — `amount` IS the margin move, exactly, with no clamp — but it
    // would make every consumer track every position's `iw` itself, and would still leave the
    // insolvency-rejected maker above with no row. The after-image costs one log.
    //
    // Cost: one extra log per position per funding epoch (`payment != 0` is the gate, i.e. only
    // the first touch after an epoch boundary), and it lands immediately BEFORE the caller's own
    // `PositionChanged` where there is one. Both are after-images; last-one-wins per
    // `(user, marketId)` is already how this event has to be read.
    //
    // Valued at the same `mark_price` the `FundingSettled` above reports, so the two agree.
    //
    // ── The group HEADER goes here, not before `ApplyFunding` ────────────────────────────────────
    // The `ACCOUNT_UPDATE` group invariant (`crate::events`) is an ADJACENCY rule: the header's
    // position rows are the ones IMMEDIATELY after it, terminated by the first other event. The
    // `FundingSettled` row above is another event, so a header emitted at the top of this function
    // would close with ZERO positions and orphan the row below it. Emitting it right here is what makes the pair a group; the payload
    // was captured at compute time (see `PendingFunding::snapshot`).
    // `FundingFee` — Binance's value for exactly this: a funding settlement moved `pos.margin` (an
    // isolated position) or the cross wallet (a cross one).
    storage::log_account_snapshot(context, p.user, &p.snapshot, AccountUpdateReason::FundingFee);
    // ── A CROSSED settle publishes NO position row ──────────────────────────────────────────────
    //
    // Binance pushes cross funding as `ACCOUNT_UPDATE m=FUNDING_FEE` with the balance only,
    // "without any position P message" (cross report F32), and the reason above is why: every
    // field of a cross position's row — `amount`, `vQuote`, `margin` (0), the entry and the PnL —
    // is exactly what it was, and only the wallet in the header moved. So the group is the header
    // alone, a 0-position group whose payload is new (the wallet moved by `payment != 0`).
    if !p.cross {
        crate::events::emit_position_changed_at_mark(
            context,
            p.user,
            p.market_id,
            &p.pos,
            mark_price,
            p.base_decimals,
            p.price_decimals,
            0,
            0,
        )?;
    }
    Ok(())
}

/// Behaviour-preserving combined settle (compute + immediate apply). For callers that do NOT need
/// to reject between the two (matching settlement, liquidation). Callers that can still refuse
/// (margin add/remove) instead use [`compute_funding_settlement`] + [`apply_funding_settlement`]
/// with their rejects in between, so a refused op publishes no funding events.
///
/// ISOLATED positions only: it holds no account, and a CROSSED position's payment belongs to one
/// (it is refused as an `[INVARIANT]`, see [`compute_funding_settlement`]).
pub(crate) fn settle_position_funding<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    pos: &mut PerpPosition,
) -> Result<(), PerpError> {
    if let Some(pending) = compute_funding_settlement(context, user, market, pos, None)? {
        apply_funding_settlement(context, pending)?;
    }
    Ok(())
}
