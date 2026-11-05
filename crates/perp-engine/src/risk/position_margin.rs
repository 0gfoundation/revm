//! Positions: `getPosition` and the isolated-margin transfers `addPositionMargin` /
//! `removePositionMargin`.

use super::*;

// ── Positions ─────────────────────────────────────────────────────────────────

/// `getPosition(address user, uint64 marketId) returns (int64 amount, int64 vQuoteBalance, int64 margin, uint64 openOrderMargin, uint64 leverage)`
///
/// `openOrderMargin` is DERIVED (`AccountPosition`'s `openOrderInitialMargin`), not stored: the
/// escrow field that used to occupy this slot is gone. It needs the market's mark price and
/// decimals, hence the extra `load_market_ref`; an unknown market yields `0` rather than a revert,
/// preserving this view's "reads back all zeros for a user with nothing here" behaviour.
pub fn run_get_position<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getPositionCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getPosition: invalid calldata"))?;

    let pos = storage::load_position_ref(context, args.user, args.marketId)?;
    let open_order_margin = match storage::load_market_ref(context, args.marketId)? {
        Some(market) => crate::margin::view::position_open_order_margin(&market, &pos)?,
        None => 0,
    };
    Ok(Bytes::from(getPositionCall::abi_encode_returns(
        &getPositionReturn {
            amount: pos.amount,
            vQuoteBalance: pos.v_quote_balance,
            margin: pos.margin,
            openOrderMargin: open_order_margin,
            leverage: pos.leverage,
        },
    )))
}

/// `addPositionMargin` on a market CROSSED in force — Binance's `-4049`.
pub(crate) const ADD_MARGIN_CROSSED_REJECT: &str =
    "addPositionMargin: -4049 Add margin only support for isolated position.";
/// `removePositionMargin` on a market CROSSED in force — Binance's `-4049`, from the remove side.
pub(crate) const REMOVE_MARGIN_CROSSED_REJECT: &str =
    "removePositionMargin: -4049 Remove margin only support for isolated position.";
/// `addPositionMargin` beyond the withdrawable balance `X` — Binance's `-4050`.
pub(crate) const ADD_MARGIN_CAP_REJECT: &str =
    "addPositionMargin: -4050 Cross balance insufficient.";
/// `removePositionMargin`'s four cap refusals (`run_remove_position_margin`, G1–G4): Binance's
/// `-4051 Isolated balance insufficient.`, which Binance answers every over-request with, then the
/// term that refused. G1: the amount exceeds the silo.
pub(crate) const REMOVE_MARGIN_G1_REJECT: &str =
    "removePositionMargin: -4051 Isolated balance insufficient. The amount exceeds the position \
     margin";
/// G2: the silo left would not cover maintenance (`iw − MM`).
pub(crate) const REMOVE_MARGIN_G2_REJECT: &str =
    "removePositionMargin: -4051 Isolated balance insufficient. The resulting margin would be \
     below maintenance margin";
/// G3: the equity left would not cover the position's initial margin (`iw + uPnL − PIM`).
pub(crate) const REMOVE_MARGIN_G3_REJECT: &str =
    "removePositionMargin: -4051 Isolated balance insufficient. The resulting equity would be \
     below the initial margin requirement";
/// G4: the position left would be liquidatable at this mark.
pub(crate) const REMOVE_MARGIN_G4_REJECT: &str =
    "removePositionMargin: -4051 Isolated balance insufficient. The resulting position would be \
     below maintenance margin";

/// `addPositionMargin(uint64 marketId, uint64 amount)`
pub fn run_add_position_margin<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = addPositionMarginCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("addPositionMargin: invalid calldata"))?;
    if args.amount == 0 {
        return Err(perp_err("addPositionMargin: amount must be > 0"));
    }
    let amount = checked_u64_to_i64(args.amount, "addPositionMargin: amount")?;
    let market = storage::load_market_ref(context, args.marketId)?
        .ok_or_else(|| perp_err("addPositionMargin: unknown market"))?;

    let mut pos = storage::load_position(context, caller, args.marketId)?;
    // A CROSSED position has no silo to add to (`CROSSED ⇒ margin == 0`): its margin IS the cross
    // wallet. Binance `-4049`, ahead of the position check — no amount of position makes a cross
    // market's margin addable.
    if crate::margin::margin_type::effective_margin_type(&pos)
        == crate::types::MarginType::Crossed
    {
        return Err(perp_err(ADD_MARGIN_CROSSED_REJECT));
    }
    if pos.amount == 0 {
        return Err(perp_err("addPositionMargin: no open position"));
    }
    let mut account = storage::load_account(context, caller)?;
    // commit-only #23: compute funding IN MEMORY; its events are emitted only by
    // `apply_funding_settlement` after the checks, so a reject below leaves no write and no event.
    // The credit/charge lands on the in-memory `pos.margin` of an ISOLATED position (it never
    // touches the account-global wallet), or on this `account` copy's wallet for a CROSSED one.
    let pending_funding =
        compute_funding_settlement(context, caller, &market, &mut pos, Some(&mut account))?;
    // Derived money-out gate. A CASH move (wallet → position margin), not an open-order
    // requirement: it changes neither `Bid`/`Ask` nor `N` nor `L`, so Σ ooIM is unchanged and the
    // requirement is exactly `amount`. It must come out of the WITHDRAWABLE balance, not the raw
    // wallet — otherwise a user could park their whole balance in position margin out from under
    // their resting orders — and not `A` either: for a cross account `A` counts cross unrealized
    // profit, which must never become isolated collateral (J6 — this is Binance's add-margin cap,
    // `max(0, min(cWB − Σ_I ooIM − Σ_C MM, A))`, cross report F5, with our maintenance floor
    // `E − Σ_I ooIM − Σ_C MM` inside the `min`: cash parked in a silo leaves the cross account as
    // surely as a transfer does). For an account with no cross position the two decide identically
    // for a positive amount.
    //
    // Refused with Binance's `-4050 Cross balance insufficient.` — the add-margin cap IS the cross
    // wallet's withdrawable balance (F5). For an account with no cross position the decision is
    // the one it always was (`X ≥ amount ⇔ A ≥ amount` for a positive amount); only the string
    // gained the code.
    let available = crate::margin::view::derived_withdrawable_balance(context, caller)?;
    if !crate::margin::view::derived_can_afford(available, args.amount as i128) {
        return Err(perp_err(ADD_MARGIN_CAP_REJECT));
    }
    account.debit_perp(args.amount)?;
    pos.margin = pos
        .margin
        .checked_add(amount)
        .ok_or_else(|| perp_err("addPositionMargin: margin overflow"))?;

    // ── APPLY (all rejects passed) ──
    if let Some(p) = pending_funding {
        apply_funding_settlement(context, p)?;
    }
    // Position BEFORE account: the account write emits the account-level `AccountBalanceChanged`
    // roll-up, whose `totalWalletBalance` is `cross + Σ isolatedWallet`. Written the other way round
    // the one event this call emits would report the debited wallet against the OLD silo and
    // under-state the gross wallet by exactly `amount` — an intermediate snapshot for no reason,
    // since both writes are unconditional here. Delta order is irrelevant to the commitment (the
    // block delta is a net key→value map), and `pos.amount` is unchanged so no registry/index hook
    // behaves differently.
    // `MarginTransfer` is Binance's value for exactly this operation — their *Modify Isolated
    // Position Margin*, money moving between the futures wallet and one position's isolated margin.
    // It is reserved for it: `transferToPerp`/`transferFromPerp` (wallet ↔ wallet) take
    // `AssetTransfer`.
    storage::save_position(
        context,
        caller,
        args.marketId,
        &pos,
        AccountUpdateReason::MarginTransfer,
    )?;
    storage::save_account(context, caller, account, AccountUpdateReason::MarginTransfer)?;
    emit_position_margin_adjusted(context, caller, args.marketId, amount, &pos);
    // The `ACCOUNT_UPDATE` group for this margin move: header immediately before its one position
    // row (`crate::events`). Off the SETTLED store — both writes above have landed and nothing
    // after this line touches the account — so `publish_account_snapshot_now` also clears the mark
    // those writes set and the end-of-call drain adds nothing.
    //
    // A funding settle earlier in this call published its OWN group (header + row) from
    // `apply_funding_settlement`. Two groups for two economic events is the intent; the
    // `PositionMarginAdjusted` row above sits between them and terminates the first.
    storage::publish_account_snapshot_now(context, caller, AccountUpdateReason::MarginTransfer)?;
    // `market` is the Arc loaded at the top of this call; nothing here writes the mark, so
    // `market.mark_price` is the live mark for this transaction.
    emit_position_changed(context, caller, &market, &pos, 0, 0)?;
    Ok(Bytes::new())
}

/// `removePositionMargin(uint64 marketId, uint64 amount)`
pub fn run_remove_position_margin<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = removePositionMarginCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("removePositionMargin: invalid calldata"))?;
    if args.amount == 0 {
        return Err(perp_err("removePositionMargin: amount must be > 0"));
    }
    let amount = checked_u64_to_i64(args.amount, "removePositionMargin: amount")?;
    let market = storage::load_market_ref(context, args.marketId)?
        .ok_or_else(|| perp_err("removePositionMargin: unknown market"))?;
    let mark_price = storage::load_mark_price(context, args.marketId)?;
    if mark_price == 0 {
        return Err(perp_err("removePositionMargin: mark price unavailable"));
    }

    let mut pos = storage::load_position(context, caller, args.marketId)?;
    // A CROSSED position has no silo to remove from — see `addPositionMargin` (`-4049`).
    if crate::margin::margin_type::effective_margin_type(&pos)
        == crate::types::MarginType::Crossed
    {
        return Err(perp_err(REMOVE_MARGIN_CROSSED_REJECT));
    }
    if pos.amount == 0 {
        return Err(perp_err("removePositionMargin: no open position"));
    }
    let mut account = storage::load_account(context, caller)?;
    // commit-only #23: compute funding IN MEMORY first so the checks below see post-funding margin
    // (possibly a debt — then G1 refuses), and a reject leaves no write and no funding event.
    let pending_funding =
        compute_funding_settlement(context, caller, &market, &mut pos, Some(&mut account))?;
    // ── The withdrawable cap (roadmap 2b — Binance's isolated `REMOVE` rule) ──
    //
    //   maxRemovable = max(0, min( iw − MM,  iw + uPnL − PIM ))      amount > maxRemovable ⇒ reject
    //
    // `iw` is `pos.margin` AFTER the in-memory funding settle above; `uPnL`, `PIM` and `MM` are at
    // the LIVE mark. The request is accepted or refused WHOLE — never clipped to the cap, never
    // partially honoured: Binance answered every over-request with `-4051` and an unchanged
    // `isolatedWallet` (59/59, R21 + R21b), and debited exactly the amount sent on all 50 it took.
    //
    // What each term does:
    //
    // * `iw + uPnL − PIM` — EQUITY after the withdrawal must still cover the position's initial
    //   margin at the mark. This is the term 2b exists for: a position in profit can release cash
    //   down to a BALANCE below PIM, the difference carried by its unrealized profit (R21 accepted a
    //   withdrawal leaving 8.0952 against PIM 8.3676). A loss counts too, and lowers the cap.
    //   MEASURED: long ×10 in profit (R21, boundary at this term ±0.0012); short ×10 both sides, short
    //   ×1 at a loss, long ×1 both sides (R21b) — including that a short's PIM is released with the
    //   mark, not frozen at the worse of entry and mark. EXTRAPOLATED: a long above ×1 at a loss (on
    //   a ×1 long R21b cannot tell the two readings apart), a short ×1 in profit, and any position
    //   with resting orders (only the POSITION term `PIM` enters, never `IM` or `ooIM`). Long and
    //   short run through this one formula — no per-side special case.
    // * `iw − MM` — the BALANCE alone, unrealized profit excluded, must still cover maintenance. It
    //   binds once `uPnL > PIM − MM`, and keeps the balance non-negative. Binance's documented
    //   formula; NEVER MEASURED (at ×10 and Binance's 0.4% MMR it needs a ~10% move). EXTRAPOLATED —
    //   and on OUR tables it is the common case, not a corner: `MM = |N|/(2·maxLev)`, so at a single
    //   3× tier it binds from `mark ≥ 1.2·entry` for an L = 3 long. Past that point a long can
    //   withdraw LESS as its profit grows. That is the formula, not a bug.
    //
    // Consequences worth knowing before "fixing" either one:
    //
    // * A freshly opened position can withdraw NOTHING, at any leverage: the opening fee is paid out
    //   of the silo, so at `mark ≈ entry` the equity sits one fee BELOW `PIM` and the cap is ≈ −fee.
    //   Binance refuses a long with no margin added the same way — MEASURED on ×1 (R21b: `0.01`
    //   refused `-4051` at a profit mark and at a loss mark); on ×10 only the reading `W0 < PIM` at
    //   open exists (R8, R21), no REMOVE was sent. The short is derived. `addPositionMargin(E)` then
    //   lifts the cap to `E − fee`, so a full add-then-remove round trip of `E` is refused too. This
    //   deliberately re-locks what B2 (`4409a033`) had unlocked.
    // * A ×1 long's EQUITY term is EXACTLY `iw + v_quote_balance`, independent of the mark: at
    //   `L = 1`, `PIM = N` cancels the notional inside `uPnL` to the unit. It is the cap until the
    //   `iw − MM` term takes over (`uPnL > N − MM`: from `mark ≈ 6·entry` on a single 3× tier). With
    //   no margin added that is `−fee` plus net funding — nothing to withdraw while funding received
    //   ≤ the fee.
    //
    // It replaces a MAINTENANCE-only rule (`amount ≤ iw` and `iw − amount + uPnL ≥ MM`, both kept, as
    // G1 and G4) whose accept set is a strict superset of this one: it let equity fall to `MM`
    // instead of `PIM`, and once `uPnL ≥ MM` let the balance go to zero. B2 (`4409a033`) had removed
    // a PnL-blind `iw − amount ≥ ⌊N/L⌋` gate that sat beside it and never released an unrealized
    // profit. Neither was Binance's — R21 measured the equity term directly.
    //
    // ARITHMETIC. `uPnL`, `PIM` and `MM` come off `margin_info_of` — the one function
    // `getPositionRisk` reports them from — so with no funding pending the accept boundary is
    // `isolatedWallet`, `unrealizedProfit`, `positionInitialMargin` and `maintMargin` from that row
    // (with funding pending, `iw` is the post-settle `pos.margin`, which the view does not settle),
    // recombined with no rounding of its own: PIM rounds UP, MM truncates, and
    // `uPnL = trunc0(N) + vQuote` (the notional's truncation is its only rounding), each as the
    // engine already does it. The comparisons are taken in `i128` with no division anywhere, so
    // "equity ≥ the PIM a client reads, balance ≥ the MM a client reads" holds bit-exactly.
    //
    // Gate order is fixed and each refusal names its own term: G1 the balance, G2 `iw − MM`, G3
    // `iw + uPnL − PIM`, G4 the liquidation predicate itself.
    //
    // Each of the four carries Binance's `-4051 Isolated balance insufficient.` — which Binance
    // answers every over-request with — followed by the term that refused it
    // (`REMOVE_MARGIN_G1_REJECT` … `G4`). Cross margin left the REMOVE rule exactly as it was
    // (cross report §3.3 item 8); the code arrived with the cross flip (R8). Through P2 these four
    // kept their uncoded strings, because the P2 ground rule kept every isolated outcome
    // byte-identical while cross margin was gated off.
    if pos.margin < amount {
        return Err(perp_err(REMOVE_MARGIN_G1_REJECT));
    }
    let new_margin = pos.margin - amount;
    let info = crate::margin::view::margin_info_of(&market, &pos)?;
    debug_assert_eq!(
        info.mark_price, mark_price,
        "removePositionMargin: the cap and the G4 predicate must price at one mark"
    );
    if (new_margin as i128) < info.maint_margin as i128 {
        return Err(perp_err(REMOVE_MARGIN_G2_REJECT));
    }
    if (new_margin as i128) + (info.unrealized_profit as i128)
        < info.position_initial_margin as i128
    {
        return Err(perp_err(REMOVE_MARGIN_G3_REJECT));
    }
    // G4 — the SAFETY NET, kept from the old rule: never leave a position that `liquidate()` would
    // take at this same mark. It is the exact predicate the liquidation path and the sweep enforce,
    // not a restatement of it.
    //
    // G4 is the gate that fails only when `uPnL < 0` AND `MM > PIM` (it is looser than G2 whenever
    // `uPnL ≥ 0`, and than G3 whenever `PIM ≥ MM`). `PIM ≥ MM` holds whenever the position's
    // leverage is at most `2 × maxLev` of the tier its notional sits in — a sufficient condition,
    // and on a single 3× tier with `L ≤ 3` it gives `PIM ≥ 2·MM`. It is NOT an identity, though, and
    // two routes break it: `setMarginTiers` installing a lower-leverage table under a live position
    // (nothing re-checks open positions), and a notional growing into a lower-leverage tier as the
    // mark rises — which G4 can only catch on a short, since a long that grew that way is in profit.
    // Without this line a withdrawal could then leave the position liquidatable on the spot. That
    // is the one corner where we refuse what Binance's formula would allow.
    if !is_above_maintenance_margin(
        &market.tiers,
        mark_price,
        pos.amount,
        pos.v_quote_balance,
        new_margin,
        market.base_decimals,
        market.price_decimals,
    )? {
        return Err(perp_err(REMOVE_MARGIN_G4_REJECT));
    }

    account.credit_perp(args.amount)?;
    pos.margin = new_margin;

    // ── APPLY (all rejects passed) ──
    if let Some(p) = pending_funding {
        apply_funding_settlement(context, p)?;
    }
    // Position BEFORE account — see `run_add_position_margin` for why.
    // `MarginTransfer` — see `run_add_position_margin`; this is the same operation, other direction.
    storage::save_position(
        context,
        caller,
        args.marketId,
        &pos,
        AccountUpdateReason::MarginTransfer,
    )?;
    storage::save_account(context, caller, account, AccountUpdateReason::MarginTransfer)?;
    emit_position_margin_adjusted(context, caller, args.marketId, -amount, &pos);
    // Group header — see `run_add_position_margin` for why it sits exactly here.
    storage::publish_account_snapshot_now(context, caller, AccountUpdateReason::MarginTransfer)?;
    // `market.mark_price` IS the `mark_price` this call's maintenance gate used — both come from
    // the same `load_market_ref` Arc (`load_mark_price` is `load_market_ref(..).mark_price`).
    emit_position_changed(context, caller, &market, &pos, 0, 0)?;
    Ok(Bytes::new())
}

// ── Internal helpers ──────────────────────────────────────────────────────────

// `emit_position_changed` lives in `crate::events` — one derivation for all seven emit sites.

fn emit_position_margin_adjusted<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    delta: i64,
    pos: &crate::types::PerpPosition,
) {
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::PositionMarginAdjusted {
            user,
            marketId: market_id,
            delta,
            margin: pos.margin,
        }
        .to_log_data(),
    });
}
