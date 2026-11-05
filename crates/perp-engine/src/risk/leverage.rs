//! Leverage: `setLeverage` (direct and signed) with its isolated and cross gates, and
//! `getSymbolConfig`.

use super::*;

// ── Leverage ──────────────────────────────────────────────────────────────────

/// `setLeverage(uint64 marketId, uint64 leverage)`
pub fn run_set_leverage<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setLeverageCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setLeverage: invalid calldata"))?;
    set_leverage_core(context, caller, args.marketId, args.leverage)
}

/// `setLeverageSigned(address account, uint64 marketId, uint64 leverage, uint64 timestamp, uint64 recvWindow, uint8 keyId, bytes signature)`
pub fn run_set_leverage_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = setLeverageSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("setLeverageSigned: invalid calldata"))?;

    let api_key = storage::load_api_key(context, args.account, args.keyId)?
        .ok_or_else(|| perp_err("setLeverageSigned: no api key registered for account"))?;

    check_recv_window(context, args.timestamp, args.recvWindow)
        .map_err(|e| perp_err(&format!("setLeverageSigned: {e}")))?;

    check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("setLeverageSigned: {e}")))?;

    // Canonical fixed-layout message; the array literal and the closing assert carry the length.
    //   "perpdex_v1_leverage" || chainId(8) || account(20) || marketId(8) || leverage(8)
    //   || timestamp(8) || recvWindow(8) || keyId(1)
    let mut msg = [0u8; 80];
    let mut c = crate::account::signing::write_signed_header(
        &mut msg,
        b"perpdex_v1_leverage",
        context.chain_id(),
    );
    c = crate::account::signing::put(&mut msg, c, args.account.as_slice());
    c = crate::account::signing::put(&mut msg, c, &args.marketId.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &args.leverage.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &args.timestamp.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &args.recvWindow.to_be_bytes());
    c = crate::account::signing::put(&mut msg, c, &[args.keyId]);
    debug_assert_eq!(c, msg.len(), "setLeverageSigned message layout");

    verify_ed25519(&api_key.pubkey, &msg, &args.signature)
        .map_err(|e| perp_err(&format!("setLeverageSigned: {e}")))?;

    // ── Replay guard ──────────────────────────────────────────────────────────────────────────
    //
    // This entrypoint had NONE. It was the only signed selector with neither an explicit guard nor
    // a state-based one: `placeOrderSigned` / `batchPlaceOrdersSigned` / `batchCancelOrdersSigned`
    // all burn the signature, and `cancelOrderSigned` is covered by terminal-status idempotence (a
    // cancelled order cannot be cancelled twice). `set_leverage_core` offers no such backstop —
    // re-applying the same leverage simply succeeds again.
    //
    // `check_recv_window` alone only bounds the exposure to ≤65s (60s cap + 5s skew). Inside that
    // window the signature was a bearer token any observer could resubmit without limit, and the
    // harm is ORDERING, not accumulation:
    //
    //     t0  owner signs A = "leverage 10", it lands           -> 10
    //     t1  owner signs B = "leverage 3"  (de-risking), lands -> 3
    //     t2  anyone replays A, still in window                 -> 10, silently
    //
    // The next position then opens at a leverage the owner had deliberately moved away from, with
    // a nearer liquidation price. The ceiling is bounded and worth stating: an attacker can only
    // replay a value the OWNER signed in the last ~65s, and `set_leverage_core` refuses to LOWER
    // leverage on an open ISOLATED position, so against a live one a replay can only push leverage
    // up (a CROSSED position may be lowered too, but only as far as the available balance carries
    // it — `-2028`). No fund path — this is risk-control integrity.
    //
    // Burned unconditionally, after verification and BEFORE `set_leverage_core` — a signed
    // authorization is spent by submission, not by success. See the long note at
    // `trading::run_place_order_signed`'s burn site for the full argument and for why the reject
    // has to carry the measured write count.
    //
    // ⚠️ NOT closed by this: ORDERING. Two in-flight leverage signatures still have no defined
    // order, because a timestamp is a freshness bound and not a sequence. Only a per-key nonce
    // would let the owner say "this supersedes that".
    let sig_hash: [u8; 32] = keccak256(args.signature.as_ref()).0;
    if storage::is_signature_seen(context, &sig_hash)? {
        return Err(perp_err(
            "setLeverageSigned: duplicate signature (already submitted)",
        ));
    }

    // ── last pre-write fault has passed; the first write happens here ──
    let block_ts: u64 = context.timestamp();
    let writes_before_burn = context.perp_write_count();
    storage::mark_signature_seen(context, &sig_hash, args.timestamp)?;
    gc_seen_buckets_best_effort(context, block_ts)?;
    let burn_writes = context.perp_write_count().saturating_sub(writes_before_burn);

    set_leverage_core(context, args.account, args.marketId, args.leverage)
        .map_err(|e| e.after_retained_writes(burn_writes))
}

fn set_leverage_core<H: PerpHost>(
    context: &mut H,
    account: Address,
    market_id: u64,
    leverage: u64,
) -> Result<Bytes, PerpError> {
    let market = storage::load_market_ref(context, market_id)?
        .ok_or_else(|| perp_err("setLeverage: unknown market"))?;

    let mut pos = storage::load_position(context, account, market_id)?;

    // The cap comes from the market's own risk table — the single source of truth, so the
    // cap and the maintenance rate can never disagree (mmr(n) is the tier's own column, else
    // 1/(2*max_leverage(n)); `setMarginTiers` keeps it below 1/max_leverage(n); the legacy 1/6
    // rate IS L₀ = 3 with the column unset).
    //
    // Look the tier up by the position's CURRENT notional rather than hardcoding tier 0.
    // Tier 0 is merely the value this collapses to in two cases that both happen to hold
    // today — a single-tier table, and a flat position (notional 0 lands in tier 0) — so
    // hardcoding it would bake in a special case. Under a real multi-tier table a position
    // already sitting in a higher bracket must be held to THAT bracket's cap: otherwise a
    // trader in a 2x tier could set 3x here, and `rebalance_order_margin_for_leverage`
    // below would release resting-order margin down to the 3x requirement while the
    // position's bracket demands 2x. (Binance rejects the same call with "exceeded the
    // maximum allowable position at current leverage".) Same quantity and same helper the
    // per-open guard uses at the settlement cores, so the two agree by construction.
    let abs_notional = calc_value_i64(
        storage::load_mark_price(context, market_id)?,
        pos.amount,
        market.base_decimals,
        market.price_decimals,
    )?
    .checked_abs()
    .ok_or_else(|| perp_err("setLeverage: tier notional abs overflow"))?;
    let cap = max_leverage_for_notional(&market.tiers, abs_notional) as u64;
    // `0`: the position is at or past the table's terminal `notionalCap` (reachable only by mark
    // drift or a later `setMarginTiers`), where no leverage is admitted — Binance's `-2027`.
    if cap == 0 {
        return Err(perp_err(
            "setLeverage: -2027 position exceeds the market's maximum notional",
        ));
    }
    if leverage == 0 || leverage > cap {
        return Err(perp_err(format!("setLeverage: leverage must be 1–{cap}")));
    }

    // ── The margin-type split (cross margin P2c, report §3.3 item 7) ──
    //
    // ISOLATED in force: unchanged — no lowering under an open position (its silo was sized at the
    // old leverage; Binance `-4161`), and the order book's `ΔooIM` gated on `A`.
    // CROSSED in force: there is no silo to re-size, the position's initial margin is DERIVED into
    // `A` at `pos.leverage`, so the leverage may move either way under a live position — raising
    // always (the requirement only falls), lowering only if `A` covers what it adds
    // (`cross_leverage_gate`, Binance `-2028`). Maintenance does not depend on the leverage, so the
    // account's cross health (`E` vs `Σ_C MM`) is untouched either way.
    let cross = crate::margin::margin_type::effective_margin_type(&pos)
        == crate::types::MarginType::Crossed;
    let old_leverage = pos.leverage.max(1);
    if !cross && pos.amount != 0 && leverage < old_leverage {
        return Err(perp_err(
            "setLeverage: cannot reduce leverage with open position",
        ));
    }

    // `&pos`, not `&mut pos`: this is a pure GATE. It is why `setLeverage` cannot move `pos.margin`
    // and therefore publishes no account snapshot — see `storage::save_position_leverage_only`.
    if cross {
        cross_leverage_gate(context, account, &market, &pos, leverage)?;
    } else {
        rebalance_order_margin_for_leverage(context, account, &market, &pos, leverage)?;
    }
    pos.leverage = leverage;
    // ── `setLeverage` publishes NO `AccountBalanceChanged`, on purpose ──────────────────────────
    //
    // The gate above writes nothing (it takes `&PerpPosition`), so `pos.margin` is NOT reallocated
    // by a leverage change on a live position, and no account blob is written at all — so none of
    // the three balances this event publishes can move, and `leverage` is not one of its fields
    // (nor one of Binance's `a.P[]` fields). Routing this through `save_position` therefore
    // published a row byte-identical to the previous state. Full argument on
    // `storage::save_position_leverage_only`.
    storage::save_position_leverage_only(context, account, market_id, pos)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::LeverageChanged {
            user: account,
            marketId: market_id,
            leverage,
        }
        .to_log_data(),
    });

    Ok(Bytes::new())
}

/// `getSymbolConfig(address user, uint64 marketId) returns (uint64 leverage, uint64 maxNotionalValue, uint8 marginType)`
///
/// `leverage` is `pos.leverage.max(1)` — the SAME floor `margin::view::margin_info_of` and
/// `math::open_order_margin` apply, so this selector reports the leverage that is actually in
/// force rather than a raw blob field that may still read 0 on a defaulted position.
///
/// `maxNotionalValue` is [`max_notional_for_leverage`] — the tier table read backwards, `0` for
/// unbounded. Kept in `math::` next to the forward lookup it inverts so the two cannot disagree
/// about the band structure; see that function for the `setMarginTiers`-under-a-live-position
/// state its assertion covers, and the ABI doc for why this is a selector at all.
///
/// `marginType` is the type IN FORCE, through
/// [`crate::margin::margin_type::effective_margin_type`] — the
/// same gate the `AccountPosition` row and `PositionChanged` read, never the raw stored byte.
///
/// Reverts on an unknown market: the tier table is a required input, and fabricating an empty one
/// would report `0` — "unbounded" — for a market that does not exist.
pub fn run_get_symbol_config<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let args = getSymbolConfigCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getSymbolConfig: invalid calldata"))?;

    let market = storage::load_market_ref(context, args.marketId)?
        .ok_or_else(|| perp_err("getSymbolConfig: unknown market"))?;
    let pos = storage::load_position_ref(context, args.user, args.marketId)?;

    let leverage = pos.leverage.max(1);
    Ok(Bytes::from(getSymbolConfigCall::abi_encode_returns(
        &getSymbolConfigReturn {
            leverage,
            maxNotionalValue: max_notional_for_leverage(&market.tiers, leverage),
            marginType: crate::margin::margin_type::effective_margin_type(&pos).code(),
        },
    )))
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Gate a leverage change on the DERIVED open-order requirement it would create.
///
/// Under the escrow this function MOVED MONEY: the reservation was `c_notional / leverage`, so
/// raising leverage released wallet cash and lowering it debited more. Nothing is escrowed any
/// more, so nothing moves here — but the requirement still changes, because `ooIM` divides by
/// `L`. Lowering leverage raises `Σ ooIM` and must be refused when the account cannot carry it;
/// raising leverage lowers it and is always free.
///
/// `Bid`/`Ask`/`N` are untouched by `setLeverage` (it moves neither the position nor the book), so
/// "before" is the stored position and "after" is the same position at `new_leverage` — the only
/// input that differs is the divisor. It does **not** maintain the aggregates and must not: an
/// order's Assuming Price is frozen at PLACEMENT, and a leverage change is not a placement, so
/// re-freezing here would silently re-price the whole resting book at the current mark. Pure read +
/// reject: zero writes on either outcome, so this may precede every write on the path
/// (commit-only).
fn rebalance_order_margin_for_leverage<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &crate::types::Market,
    pos: &crate::types::PerpPosition,
    new_leverage: u64,
) -> Result<(), PerpError> {
    let mut after = pos.clone();
    after.leverage = new_leverage;
    let delta = crate::margin::view::derived_requirement_delta(market, pos, &after)?;
    // The available is measured on the CURRENT (pre-change) state, which is what `delta` is the
    // increment to — with the caller's in-memory `pos` overriding storage for this market, since
    // it is not written until after this gate. A non-positive delta is free (`derived_can_afford`).
    //
    // ── Why this gate keeps the EXPLICIT delta shape (and `rest_in_book` does not) ──
    // The post-state/lazy restatement `trading::rest_in_book` uses (walk at `after`, delta consulted
    // only when `available(after) < 0`, and then only for its sign) IS valid here — this is a pure
    // delta, `market`/`pos` are the same values the Σ walk would re-read, and setLeverage runs on no
    // match path so there is no uncommitted registry working copy in play. It is left explicit for
    // two reasons, neither of them compatibility:
    //   * `setLeverage` is a cold selector, so "this market's ooIM once instead of three times" buys
    //     nothing measurable, and
    //   * this is the ONE gate whose delta is routinely NEGATIVE (raising leverage divides the
    //     requirement down and releases headroom). Written out, it is the site where a reader can
    //     see what `derived_can_afford`'s `requirement <= 0` clause is actually for; folded into
    //     `available(after) >= 0` that disappears. `rest_in_book` cannot serve that purpose —
    //     resting can only ever raise `max(|N + Bid|, |N − Ask|)`, so its delta is never negative.
    // Sequencing note if that ever changes: `pos.leverage` is written by the CALLER after this
    // returns, so an override carrying `after` here would be one step ahead of storage.
    let available = crate::margin::view::derived_available_balance_with(
        context,
        user,
        None,
        Some((market.market_id, pos)),
    )?;
    if !crate::margin::view::derived_can_afford(available, delta) {
        return Err(perp_err(
            "setLeverage: insufficient perp wallet for order margin",
        ));
    }
    Ok(())
}

/// `setLeverage`'s refusal for a CROSSED position whose lower leverage the available balance cannot
/// carry — Binance's `-2028`.
pub(crate) const CROSS_LEVERAGE_REJECT: &str =
    "setLeverage: -2028 Leverage is smaller than permitted: insufficient margin balance.";

/// The CROSSED `setLeverage` gate (cross report §3.3 item 7, G12): `derived_can_afford(A, ΔIM)`.
///
/// A cross position's initial margin is not a silo but a term of the account's available balance —
/// `PIM = ROUND_UP(|N| / L)` at the position's own leverage, and its resting orders' `ooIM` on top
/// (`A = cWB + Σ_C (uPnL − PIM) − Σ ooIM`). A leverage change moves exactly this market's joint
/// requirement `IM = PIM + ooIM = ROUND_UP(max(|N + Bid|, |N − Ask|) / L)` and nothing else, so the
/// change is affordable iff the CURRENT `A` covers `ΔIM = IM(new L) − IM(old L)` — `A(after) ≥ 0`
/// in delta form, the identity `rest_in_book` documents, with this market's term the only one that
/// differs. Raising divides the requirement down (`ΔIM ≤ 0`): always admitted, by the
/// non-positive-requirement clause, at any balance. Lowering is refused `-2028` unless `A` covers
/// the increase.
///
/// Pure read + reject, like [`rebalance_order_margin_for_leverage`] (the isolated gate it replaces
/// for a cross position — that one charges only `ΔooIM`, because an isolated position's own
/// requirement is its silo, fixed at open): zero writes either way, the walk the same one fold.
fn cross_leverage_gate<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &crate::types::Market,
    pos: &crate::types::PerpPosition,
    new_leverage: u64,
) -> Result<(), PerpError> {
    let mut after = pos.clone();
    after.leverage = new_leverage;
    let im = |p: &crate::types::PerpPosition| -> Result<i128, PerpError> {
        Ok(crate::margin::view::position_derived_margin(market, p)?.margin.initial_margin as i128)
    };
    let delta = im(&after)? - im(pos)?;
    let available = crate::margin::view::derived_available_balance_with(
        context,
        user,
        None,
        Some((market.market_id, pos)),
    )?;
    if !crate::margin::view::derived_can_afford(available, delta) {
        return Err(perp_err(CROSS_LEVERAGE_REJECT));
    }
    Ok(())
}
