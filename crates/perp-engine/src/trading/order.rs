use super::*;

// ── Core order logic (shared by direct and signed paths) ─────────────────────

/// `placeOrder`'s `flags` bitfield: bit 0 reduce-only, bits 3:2 the self-trade-prevention mode
/// ([`StpMode`], shared with `placeAlgoOrder`), every other bit reserved.
///
/// Unknown bits are REJECTED, not ignored. Ignoring them would let a caller ask for a modifier this
/// build does not implement and receive a plain order that looks accepted — the failure would then
/// surface as unexpected fills, not as an error. Rejecting makes the version mismatch loud at the
/// call that caused it.
pub(crate) const ORDER_FLAG_REDUCE_ONLY: u8 = 1 << 0;

/// Whether the reduce-only bit is reachable at all.
///
/// # Why this is still `false` even though admission is implemented
///
/// [`crate::trading::reduce_only::admit`] establishes the prefix condition at PLACEMENT, but two
/// things can
/// break it afterwards, and each needs its own evaluation point before the flag is safe to expose:
///
/// ```text
/// a normal same-side order placed AHEAD of a resting reduce-only order
///     long 10, reduce-only SELL 10 resting at 105; a normal SELL 10 goes in at 101.
///     Nothing refuses the normal order — it has no reduce-only predicate of its own — and the
///     prefix is now 10 + 10 = 20 > 10. A sweep fills the normal (position -> 0), then the
///     reduce-only OPENS 10 the other way.
///
/// a position that shrinks without its orders being cancelled
///     liquidation cancels every order in the market, so it is covered. ADL does NOT, so an ADL'd
///     position leaves its reduce-only orders over-committed against a smaller |position|.
/// ```
///
/// Both are load-bearing, so the flag stays refused until the eviction pass exists. An order the
/// caller believes can only reduce, which in fact opens and flips their position, is worse than a
/// rejected call — and in a release build the `fill_opening_qty == 0` watchdog is compiled out, so
/// it would be a SILENT wrong position rather than a loud one.
const REDUCE_ONLY_ENABLED: bool = true;
const ORDER_FLAGS_KNOWN: u8 = ORDER_FLAG_REDUCE_ONLY | StpMode::FLAG_MASK;

/// The per-order modifiers a `flags` byte decodes to — what an `Order` / `OrderEntry` stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OrderFlags {
    pub(crate) reduce_only: bool,
    pub(crate) stp: StpMode,
}

impl OrderFlags {
    /// The modifiers an order record carries.
    #[inline]
    pub(crate) fn of(order: &Order) -> Self {
        Self {
            reduce_only: order.reduce_only,
            stp: order.stp,
        }
    }

    /// Their wire byte (see [`encode_order_flags`]).
    #[inline]
    pub(crate) fn encode(self) -> u8 {
        encode_order_flags(self.reduce_only, self.stp)
    }
}

/// The wire `flags` byte for an order's stored modifiers — the inverse of [`decode_order_flags`],
/// and the ONLY encoder: every echo (`OrderPlaced`, `OrderRested`, `getOrder`, `getOpenOrders`)
/// and every protocol-built order (a fired trigger, a liquidation close) goes through here, so no
/// site can hand-assemble a byte that disagrees with the others.
///
/// Exists so the events and the two order views publish the flags rather than leaving them
/// recoverable only by decoding `placeOrder` calldata (which an indexer cannot do: it would have
/// to re-derive each batch item's order id to attribute them).
///
/// ⚠️ `StpMode::ExpireMaker` encodes as `00`, the same bits as "no mode given". An echo site that
/// forgot to pass the stored mode would therefore not fail — it would silently claim EXPIRE_MAKER.
/// Pass the STORED value, never a default.
#[inline]
pub(crate) fn encode_order_flags(reduce_only: bool, stp: StpMode) -> u8 {
    let ro = if reduce_only {
        ORDER_FLAG_REDUCE_ONLY
    } else {
        0
    };
    ro | stp.to_flags()
}

/// Decodes `flags` into the per-order modifiers, rejecting anything unrecognised: unknown bits
/// first, then the reserved STP mode `0b11`. Kept beside the constants so a new bit cannot be added
/// to one without the other.
pub(crate) fn decode_order_flags(flags: u8, sel: &str) -> Result<OrderFlags, PerpError> {
    let unknown = flags & !ORDER_FLAGS_KNOWN;
    if unknown != 0 {
        return Err(perp_err(format!("{sel}: unknown order flags 0x{unknown:02x}")));
    }
    let reduce_only = flags & ORDER_FLAG_REDUCE_ONLY != 0;
    if reduce_only && !REDUCE_ONLY_ENABLED {
        return Err(perp_err(format!("{sel}: reduce-only not yet enabled")));
    }
    let stp = StpMode::from_flags(flags)
        .ok_or_else(|| perp_err(format!("{sel}: invalid self-trade prevention mode")))?;
    Ok(OrderFlags { reduce_only, stp })
}

/// Allocate the next order ID for `account` using the per-user nonce counter.
/// Derives the next order id from the CURRENT nonce WITHOUT bumping it (commit-only #23: the
/// nonce write happens only after the placement fully succeeds — a rejected placement leaves the
/// nonce untouched, so the same id is reused, matching the old revert-rollback behavior).
/// Returns `(order_id, bumped_nonce)`; the caller persists the bump via
/// [`commit_order_nonce`] on the success path.
pub(crate) fn peek_order_id<H: PerpHost>(
    context: &mut H,
    account: Address,
) -> Result<([u8; 32], u64), PerpError> {
    let nonce = storage::load_user_nonce(context, account)?;
    Ok((derive_order_id(account, nonce), nonce + 1))
}

/// The one and only direct-path order-id derivation: `keccak256(account(20) || nonce(8 BE))`.
///
/// Factored out because `batchPlaceOrders` cannot call [`peek_order_id`] per item — the nonce is not
/// persisted until the end of the batch, so every item would peek the SAME id. The batch derives
/// item `k`'s id from `base_nonce + ids_consumed_so_far` instead, and must do so with byte-identical
/// math to the single-order path (`save_order` has no collision guard).
pub(super) fn derive_order_id(account: Address, nonce: u64) -> [u8; 32] {
    let mut buf = [0u8; 28];
    buf[..20].copy_from_slice(account.as_slice());
    buf[20..28].copy_from_slice(&nonce.to_be_bytes());
    keccak256(&buf).0
}

/// Persists the nonce bump reserved by [`peek_order_id`]. Call ONLY after the placement
/// succeeded (all genuine rejects passed).
pub(crate) fn commit_order_nonce<H: PerpHost>(
    context: &mut H,
    account: Address,
    bumped_nonce: u64,
) -> Result<(), PerpError> {
    storage::save_user_nonce(context, account, bumped_nonce)
}

struct ValidatedOrder {
    market: crate::types::Market,
    side: Side,
    /// The wire's `(orderType, tif)` pair, already collapsed into the ONE internal value
    /// ([`OrderKind`]). Past this struct there is no such thing as a market order with a
    /// time-in-force — `validate_place_order` is the only place the illegal pairs can even be
    /// spelled, and it rejects them.
    kind: OrderKind,
    /// `flags` bit 0, decoded once here so nothing downstream re-parses the bitfield. Reaches the
    /// stored `Order` via `announce_new_order` and the `OrderEntry` via `rest_in_book`.
    reduce_only: bool,
    /// `flags` bits 3:2, decoded by the same call and threaded the same way.
    stp: StpMode,
}

/// Where an ordinary placement comes from. It decides one thing only: whether the per-market
/// open-order cap ([`crate::trading::algo::MAX_OPEN_ORDERS_PER_MARKET`]) is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlacementOrigin {
    /// A user submission: `placeOrder`, `placeOrderSigned`, a batch item (also via `asSubAccount`).
    Submitted,
    /// A conditional order the trigger sweep fired (`algo::fire`), placed inside an ORACLE
    /// transaction on the owner's behalf.
    Triggered,
}

/// Validate → build in memory → execute → ONE final persist. Returns the taker order's FINAL status,
/// which is the whole outcome the callers need: `Open`/`PartiallyFilled` = it rests, and anything
/// `is_terminal()` = it fully filled or its IOC/FOK/market remainder expired, leaving no record.
/// (`batchPlaceOrders` maps that split onto the `Accepted` / `Filled` status tags. The single-order
/// entry points ignore it — they answer with the order id and let the caller read the logs.)
pub(crate) fn place_order_core<H: PerpHost>(
    account: Address,
    order_id: [u8; 32],
    market_id: u64,
    side_u8: u8,
    price: u64,
    quantity: u64,
    order_type_u8: u8,
    tif_u8: u8,
    client_order_id: [u8; 16],
    flags: u8,
    origin: PlacementOrigin,
    context: &mut H,
) -> Result<OrderStatus, PerpError> {
    let validated = validate_place_order(
        context,
        market_id,
        side_u8,
        price,
        quantity,
        order_type_u8,
        tif_u8,
        flags,
    )?;
    // ── -2023: an account IN LIQUIDATION places only reduce-only orders (owner decision Q11) ──
    // A fired closePosition / conditional order arrives here reduce-only, so it passes too. See
    // `refuse_unless_reducing_in_liquidation`.
    refuse_unless_reducing_in_liquidation(
        context,
        account,
        validated.reduce_only,
        IN_LIQUIDATION_REJECT,
    )?;
    // Per-user market-index cap. This is the ONLY way into a market the user is not already
    // active in — a position can only appear through a fill of an order they placed, and every
    // other position writer (maker fill, liquidation, ADL, funding) acts on a market where the
    // user already holds a position or a resting order. Placed with the other genuine rejects,
    // BEFORE any write, so the commit-only "no reject after a write" rule holds.
    storage::ensure_user_market_admission(context, account, market_id, "placeOrder")?;

    // ── per-market open-order cap ──────────────────────────────────────────────────────────────
    //
    // Only for an order that can REST — GTC and PostOnly, i.e. exactly the kinds with a resting
    // queue position. An IOC / FOK / market order never adds a book entry, so it is never refused
    // here: a user at the cap can still close. The count is the PRE-match one (a GTC that would have
    // filled completely is refused at the cap too — a submission filter, like Binance's). Pre-write,
    // like the market-index cap above.
    if validated.kind.has_resting_queue_position() {
        match origin {
            PlacementOrigin::Submitted => {
                storage::ensure_open_order_capacity(context, account, market_id)?
            }
            // NOT re-checked, so the cap can never refuse inside an oracle transaction. It could not
            // anyway: `fire` removes the conditional from the owner's list BEFORE placing it, so a
            // trigger trades one counted open order for another (conditional −1, book ≤ +1), and
            // every conditional was itself admitted under the cap.
            PlacementOrigin::Triggered => {
                #[cfg(debug_assertions)]
                {
                    let open = storage::book_order_count(context, account, market_id)?
                        + storage::load_user_algo_orders(context, account, market_id)?.len();
                    debug_assert!(
                        open < crate::trading::algo::MAX_OPEN_ORDERS_PER_MARKET,
                        "a triggered order found {account} at the open-order cap ({open}) in \
                         market {market_id}: some path admitted an order without the cap check"
                    );
                }
            }
        }
    }

    // ── reduce-only admission: the ONE place the invariant is enforced ────────────────────────
    //
    // Runs here, after validation and the market-index cap and before ANY write, so a refusal is a
    // clean pre-write reject. `quantity` is rebound because an over-sized request is accepted and
    // SILENTLY TRUNCATED — the accepted size is what the stored order, the match walk and
    // `OrderPlaced` all carry from this point on.
    //
    // An order that clears this gate can never produce an opening fill, which is why the fill path
    // carries only a watchdog and no clamp. See `crate::trading::reduce_only`.
    let quantity = if validated.reduce_only {
        crate::trading::reduce_only::admit(
            context,
            account,
            market_id,
            validated.side,
            // Derived from the ORDER KIND, never from the price alone: a market/IOC/FOK order has no
            // queue position, and a market order's price is not even set.
            crate::trading::reduce_only::QueuePosition::for_kind(validated.kind, price),
            quantity,
            validated.market.step_size,
        )?
    } else {
        quantity
    };

    // commit-only #23: build the Order in memory. The OrderPlaced log is BUFFERED (not emitted):
    // every genuine reject still lies ahead, and the batch selectors catch a per-item error without
    // reverting the frame — so the log is flushed by `emit_pending_order_placed` at the first APPLY
    // point (match flush / rest / the final persist below), keeping its original stream position.
    let (mut taker_order, pending) = announce_new_order(
        account,
        &order_id,
        market_id,
        price,
        quantity,
        client_order_id,
        &validated,
    );
    let mut pending = Some(pending);

    // ── reduce-only watchdog (taker side) ────────────────────────────────────────────────────
    //
    // The taker's own fills are bounded by the admission capacity computed a few statements above,
    // so they can only ever close. Asserted as an OUTCOME rather than per-fill — "|position| did
    // not grow" is mechanism-independent, so it survives a refactor of how the split is computed,
    // and it needs nothing threaded through `finalize_core`.
    #[cfg(debug_assertions)]
    let abs_position_before = if validated.reduce_only {
        storage::load_position_ref(context, account, market_id)?
            .amount
            .unsigned_abs()
    } else {
        0
    };

    // The ONE dispatch on what the order is. `OrderKind` makes it total: there is no third case to
    // forget, and the limit arm gets its TIF by destructuring (not by re-reading a field that a
    // market order also has). `Some(reason)` = the remainder expired (and why).
    let expiry = match validated.kind {
        OrderKind::Limit(tif) => execute_limit_order(
            context,
            account,
            order_id,
            market_id,
            price,
            quantity,
            client_order_id,
            validated.side,
            tif,
            &validated.market,
            &mut taker_order,
            &mut pending,
        )?,
        OrderKind::Market => execute_market_order(
            context,
            account,
            order_id,
            market_id,
            price,
            quantity,
            validated.side,
            &validated.market,
            &mut taker_order,
            &mut pending,
        )?,
    };
    #[cfg(debug_assertions)]
    if validated.reduce_only {
        let after = storage::load_position_ref(context, account, market_id)?
            .amount
            .unsigned_abs();
        debug_assert!(
            after <= abs_position_before,
            "reduce-only taker order grew the position from {abs_position_before} to {after}"
        );
    }

    // Fallback flush: the order is accepted (every genuine reject returned above) but reached no
    // other apply site — an IOC/market order that expired against an empty book matches nothing and
    // rests nothing, yet legitimately emits OrderPlaced. No-op when an apply already flushed it.
    emit_pending_order_placed(context, &mut pending);
    // ── THE EXPIRY'S TERMINAL EVENT ──────────────────────────────────────────────────────────────
    // `OrderStatus::Expired` is set in exactly one place (`cancel_unfilled_remainder`, and only when
    // `remaining > 0`), which also hands back the reason — so `expiry` here is the same predicate
    // as "this order discarded a remainder" (an IOC or market order, or any STP-applicable order
    // whose walk self-trade prevention stopped) without re-deriving which TIFs rest, which is
    // precisely the reasoning this event exists to stop a CONSUMER from having to do. FOK reverts
    // instead (`ensure_fok_filled`), a refused GTC rest reverts (`raise_rest_refusal`), and a full
    // fill has no remainder, so none of them land here.
    //
    // ⚠️ THIS SITE, NOT THE `cancel_unfilled_remainder` CALL SITES. All of those sit before the
    // `emit_pending_order_placed` fallback above, and an IOC that matches NOTHING reaches no other
    // apply site — so emitting there would put the terminal event BEFORE this order's own
    // `OrderPlaced`. Here it is after `OrderPlaced` and after every `Trade` the match flushed, which
    // is also the measured Binance frame order (R15: the last fill still says `PARTIALLY_FILLED`,
    // the terminal state arrives as a separate fill-less frame).
    //
    // It is emitted BEFORE the persist below only because the persist is a delete; nothing here
    // reads the store.
    debug_assert_eq!(
        expiry.is_some(),
        taker_order.status == OrderStatus::Expired,
        "an expiry reason is returned exactly when the remainder expired"
    );
    if let Some(reason) = expiry {
        // Derived from the two quantities the order itself carries, so `filled + expired == quantity`
        // holds by construction rather than by a second count of the `Trade` logs.
        let expired_quantity = taker_order.quantity.saturating_sub(taker_order.filled);
        debug_assert!(
            expired_quantity > 0,
            "Expired implies a discarded remainder: cancel_unfilled_remainder sets this status \
             only when remaining > 0"
        );
        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::OrderExpired {
                user: account,
                marketId: market_id,
                orderId: FixedBytes(order_id),
                filledQuantity: taker_order.filled,
                expiredQuantity: expired_quantity,
                reason: reason as u8,
            }
            .to_log_data(),
        });
    }
    // Single final persist of the taker order. delete-on-terminal: a Filled/Expired taker leaves
    // NO record (it fully filled or its IOC/FOK/market remainder expired — never resting); an
    // Open/PartiallyFilled taker rested, so it is saved live (its book entry / level FIFO / live
    // count were already written by `rest_in_book`).
    let status = taker_order.status;
    if status.is_terminal() {
        storage::delete_order(context, &order_id)?;
    } else {
        // taker_order is owned here — move it in instead of cloning through save_order(&Order).
        storage::save_order_owned(context, &order_id, taker_order)?;
    }
    Ok(status)
}

/// `placeOrder`'s refusal for an account in liquidation (owner decision Q11) — Binance's `-2023`.
pub(crate) const IN_LIQUIDATION_REJECT: &str =
    "placeOrder: -2023 User in liquidation mode now. Only reduce-only and closePosition orders \
     are accepted";

/// Owner decision Q11 / Binance `-2023 User in liquidation mode now.`: while the account's
/// liquidation flag is set ([`crate::types::UserAccount::in_liquidation`] — set and cleared by
/// cross liquidation, P3) only a REDUCING order is accepted — reduce-only, or closePosition (which
/// is reduce-only by construction). `reducing` is the caller's verdict on the order; `reject` its
/// entrypoint's string.
///
/// The ONE rule every placement entrypoint applies: `placeOrder`, `placeOrderSigned` and both
/// batch selectors through [`place_order_core`] (a fired conditional reaches that too, always
/// reduce-only), and `placeAlgoOrder` / `placeAlgoOrderSigned` — conditional and trailing alike —
/// through `algo::place_core`. A cancel is never refused: it only lightens what the liquidation
/// has to unwind.
///
/// A reducing order costs nothing here. Any other loads the account (`_ref`, cache-fill: every
/// placement path reads it again for the fee rates). Pre-write; on a signed entrypoint after the
/// signature burn, with the other refusals that can lift later.
pub(crate) fn refuse_unless_reducing_in_liquidation<H: PerpHost>(
    context: &mut H,
    account: Address,
    reducing: bool,
    reject: &'static str,
) -> Result<(), PerpError> {
    if !reducing && storage::load_account_ref(context, account)?.in_liquidation() {
        return Err(perp_err(reject));
    }
    Ok(())
}

fn validate_place_order<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side_u8: u8,
    price: u64,
    quantity: u64,
    order_type_u8: u8,
    tif_u8: u8,
    flags: u8,
) -> Result<ValidatedOrder, PerpError> {
    let OrderFlags { reduce_only, stp } = decode_order_flags(flags, "placeOrder")?;
    let market = storage::load_market(context, market_id)?
        .ok_or_else(|| perp_err("placeOrder: unknown market"))?;
    if !market.active {
        return Err(perp_err("placeOrder: market not active"));
    }

    let side = Side::from_u8(side_u8).ok_or_else(|| perp_err("placeOrder: invalid side"))?;
    let order_type = OrderType::from_u8(order_type_u8)
        .ok_or_else(|| perp_err("placeOrder: invalid orderType"))?;
    let tif = TimeInForce::from_u8(tif_u8).ok_or_else(|| perp_err("placeOrder: invalid tif"))?;
    // THE BOUNDARY. The ABI keeps its two independent `uint8`s (callers and the OrderPlaced /
    // OrderRested events are unchanged), but their product is not a product: `Market + Fok`
    // (all-or-nothing at market — no exchange sells it), `Market + PostOnly` ("never take
    // liquidity" on the one order type that only takes it). Both die here; past this line the
    // engine holds ONE `OrderKind` and the illegal states do not exist to be handled.
    let kind = OrderKind::from_parts(order_type, tif)
        .ok_or_else(|| perp_err("placeOrder: tif not allowed for market order"))?;

    // reduce-only is EXEMPT from the minimum size. MEASURED (§1.6): a one-step reduce-only order,
    // 0.0037 USDT against a 5 USDT floor, was accepted. It has to be: silent truncation can shrink a
    // request to whatever is left of the position, which is routinely below any sane minimum, and an
    // order that closes the last sliver of a position is exactly the one a trader most needs.
    if !reduce_only && quantity < market.min_quantity {
        return Err(perp_err("placeOrder: quantity below minimum"));
    }
    if quantity > market.max_quantity {
        return Err(perp_err("placeOrder: quantity exceeds maximum"));
    }
    if market.step_size > 0 && quantity % market.step_size != 0 {
        return Err(perp_err("placeOrder: quantity not multiple of step_size"));
    }
    if kind.is_limit() {
        if price == 0 {
            return Err(perp_err("placeOrder: limit order price must be > 0"));
        }
        if price > market.max_price {
            return Err(perp_err("placeOrder: price exceeds maximum"));
        }
        if market.tick_size > 0 && price % market.tick_size != 0 {
            return Err(perp_err("placeOrder: price not multiple of tick_size"));
        }
        // No placement-time price band: an out-of-band limit order is allowed to REST
        // (a deep passive order is harmless — it can only ever fill if the mark legitimately
        // reaches it). The off-mark guard lives at FILL time in `match_order` /
        // `check_fok_feasibility` (see `math::mark_band_bounds`), which is immune to
        // post-placement mark drift and also bounds market-order slippage. The upper price
        // sanity cap (`price > market.max_price`) above still prevents absurd book pollution.
    }

    Ok(ValidatedOrder { market, side, kind, reduce_only, stp })
}

/// The `OrderPlaced` log of a placement that is not yet known-accepted.
///
/// `announce_new_order` runs BEFORE every genuine reject (PostOnly-would-cross, FOK-unfillable,
/// the taker K9 / wallet-cover / fills+rest rejects in `finalize_compute`, `rest_in_book`'s margin
/// rejects), so it may not emit: today a per-call `Err` reverts the frame and truncates the logs,
/// but the batch selectors CATCH a per-item error and return `Ok` overall — the frame does not
/// revert, and an `OrderPlaced` for an order that never existed would survive in the receipts.
///
/// The EVM journal is append-only (a log cannot be un-emitted), so the log is BUFFERED here and
/// flushed by [`emit_pending_order_placed`] at the first APPLY point the place path reaches. That
/// preserves the original log-stream position: nothing between the announce and the first apply
/// emits a log (the match walk is write- and log-free; every `Trade` / `PositionChanged` /
/// `OrderCancelled` / `InsuranceFund*` is deferred into `MatchRegistry::flush`), so `OrderPlaced`
/// still comes first for its order.
pub(super) struct PendingOrderPlaced {
    user: Address,
    market_id: u64,
    order_id: [u8; 32],
    side: u8,
    price: u64,
    quantity: u64,
    order_type: u8,
    tif: u8,
    client_order_id: [u8; 16],
    flags: u8,
}

/// Flushes the buffered `OrderPlaced` — **exactly once**: the `take()` makes every later call a
/// no-op, so all apply sites can call it unconditionally. Also a no-op for callers that have no
/// pending log (the liquidation close emits its own `OrderPlaced` and passes `&mut None`).
pub(super) fn emit_pending_order_placed<H: PerpHost>(
    context: &mut H,
    pending: &mut Option<PendingOrderPlaced>,
) {
    let Some(p) = pending.take() else {
        return;
    };
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderPlaced {
            user: p.user,
            marketId: p.market_id,
            orderId: FixedBytes(p.order_id),
            side: p.side,
            price: p.price,
            quantity: p.quantity,
            orderType: p.order_type,
            tif: p.tif,
            clientOrderId: FixedBytes(p.client_order_id),
            flags: p.flags,
        }
        .to_log_data(),
    });
}

/// Builds the taker `Order` in memory plus its not-yet-emitted `OrderPlaced`
/// ([`PendingOrderPlaced`]). Read-only: no storage write, and — unlike before — no log either.
fn announce_new_order(
    account: Address,
    order_id: &[u8; 32],
    market_id: u64,
    price: u64,
    quantity: u64,
    client_order_id: [u8; 16],
    validated: &ValidatedOrder,
) -> (Order, PendingOrderPlaced) {
    // The record and the event keep the ABI's two independent fields, both DERIVED from the single
    // `OrderKind` (`OrderKind::order_type` / `OrderKind::tif`) — so the pair they carry is
    // reachable-by-construction, not assembled from two free-floating inputs. A market order
    // reports `tif = Ioc`, which is what it is.
    let order = Order {
        owner: account.0 .0,
        market_id,
        side: validated.side,
        price,
        quantity,
        filled: 0,
        order_type: validated.kind.order_type(),
        tif: validated.kind.tif(),
        status: OrderStatus::Open,
        reduce_only: validated.reduce_only,
        // Not conditional: this is the ordinary placement path. A triggered algo order reaches the
        // book through here too, but by then it is an ordinary order — the trigger already fired.
        trigger_price: 0,
        trigger_up: false,
        close_position: false,
        stp: validated.stp,
        // An ordinary order is bound to no position (only a conditional one can be).
        position_generation: 0,
        trail_bps: 0,
    };

    let pending = PendingOrderPlaced {
        flags: encode_order_flags(validated.reduce_only, validated.stp),
        user: account,
        market_id,
        order_id: *order_id,
        side: validated.side as u8,
        price,
        quantity,
        order_type: validated.kind.order_type() as u8,
        tif: validated.kind.tif() as u8,
        client_order_id,
    };

    (order, pending)
}

/// Binance-style expiry of a taker remainder that will never rest → Expired, not Cancelled (which
/// is reserved for orders that rested). In-memory only; the caller performs the final persist.
///
/// Two causes, told apart by `reason` and published as `OrderExpired.reason`:
/// * [`ExpiryReason::Tif`]: an IOC or market remainder the walk did not fill (liquidity, the limit
///   price or the fill-time band stopped it);
/// * [`ExpiryReason::SelfTradePrevention`]: the walk was stopped at the taker's own resting order
///   (`EXPIRE_TAKER` / `EXPIRE_BOTH`), for ANY kind STP applies to, GTC included.
///
/// Returns the reason exactly when it expired something (`remaining > 0`) — the value
/// `place_order_core` keys its one `OrderExpired` emission on.
fn cancel_unfilled_remainder(
    taker_order: &mut Order,
    remaining: u64,
    reason: ExpiryReason,
) -> Option<ExpiryReason> {
    if remaining > 0 {
        taker_order.status = OrderStatus::Expired;
        Some(reason)
    } else {
        None
    }
}

/// The expiry reason for an IOC or market remainder: STP if the walk was stopped at the taker's own
/// order, the TIF otherwise. An `EXPIRE_MAKER` walk that cancelled its owner's orders and then ran
/// out of book is a plain TIF expiry (`0`) — STP did not stop it (Binance: a market order that
/// empties the book after an STP cancel is `EXPIRED`, not `EXPIRED_IN_MATCH`).
#[inline]
fn immediate_expiry_reason(stp_taker_expired: bool) -> ExpiryReason {
    if stp_taker_expired {
        ExpiryReason::SelfTradePrevention
    } else {
        ExpiryReason::Tif
    }
}

fn ensure_fok_filled(remaining: u64) -> Result<(), PerpError> {
    if remaining == 0 {
        Ok(())
    } else {
        Err(perp_err("placeOrder: FOK order cannot be fully filled"))
    }
}

/// PostOnly cross check. Reads the BBO ONCE (both sides, one MarketHot probe) and RETURNS it so the
/// caller can thread it into `rest_in_book` (no match runs on the PostOnly path, so the BBO stays
/// current from here to the rest).
fn ensure_post_only_does_not_cross<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    side: Side,
    price: u64,
) -> Result<(u64, u64), PerpError> {
    let h = storage::load_market_hot(context, market_id)?;
    let (best_bid, best_ask) = (h.best_bid, h.best_ask);
    match side {
        Side::Buy => {
            if best_ask != 0 && best_ask <= price {
                return Err(perp_err("placeOrder: PostOnly order would match"));
            }
        }
        Side::Sell => {
            if best_bid != 0 && best_bid >= price {
                return Err(perp_err("placeOrder: PostOnly order would match"));
            }
        }
    }

    Ok((best_bid, best_ask))
}

/// #B: index of `order_id` in a PRICE-SORTED list, found via binary-search to the price-run
/// (O(log n)) then a scan of that small same-price run — instead of an O(n) id scan. The order's
/// `price` is always known at the call site (the loaded Order / the matched level). `buy_side`
/// picks the sort direction (buys DESC, sells ASC). Returns None if absent (price not in the list,
/// or no entry in the price-run carries `order_id`).
pub(super) fn find_entry_by_price_id(
    entries: &std::collections::VecDeque<OrderEntry>,
    order_id: &[u8; 32],
    price: u64,
    buy_side: bool,
) -> Option<usize> {
    let search = || -> Option<usize> {
        let probe = if buy_side {
            entries.binary_search_by(|e| price.cmp(&e.price)) // DESC: monotone Less→Eq→Greater
        } else {
            entries.binary_search_by(|e| e.price.cmp(&price)) // ASC
        };
        let anchor = probe.ok()?;
        // Walk to the start of the equal-price run, then scan it for the order_id (run is tiny:
        // per-price occupancy p50=1 / p90=3).
        let mut start = anchor;
        while start > 0 && entries[start - 1].price == price {
            start -= 1;
        }
        let mut k = start;
        while k < entries.len() && entries[k].price == price {
            if &entries[k].order_id == order_id {
                return Some(k);
            }
            k += 1;
        }
        None
    };
    let result = search();
    // Guard rail: the binary search converts "list happens to be price-sorted" from a perf property
    // into a CORRECTNESS precondition (the old linear id-scan was order-agnostic). order_ids are
    // unique per side, so the binary-search index MUST equal the linear-scan index — assert it in
    // debug builds (zero release cost). This turns any future sortedness regression (buys DESC /
    // sells ASC broken by some later change) into a loud test failure here instead of a silent
    // "entry not found" revert at cancel / fill time.
    debug_assert_eq!(
        result,
        entries.iter().position(|e| &e.order_id == order_id),
        "find_entry_by_price_id disagrees with linear id scan — per-account order list is not \
         price-sorted (buys DESC / sells ASC invariant broken?)"
    );
    result
}

/// #B: remove one entry from a per-account order list — O(log n) via [`find_entry_by_price_id`]
/// instead of an O(n) id scan. Removes the same unique entry the id scan would (order_ids are unique
/// per side), so the result is byte-identical; it is now the ONLY entry-removal routine (the O(n)
/// `remove_order_entry` it replaced on the hot cancel path is gone — every caller knows the price).
pub(super) fn remove_order_entry_by_price(
    entries: &mut std::collections::VecDeque<OrderEntry>,
    order_id: &[u8; 32],
    price: u64,
    buy_side: bool,
    side_label: &str,
) -> Result<OrderEntry, PerpError> {
    let idx = find_entry_by_price_id(entries, order_id, price, buy_side).ok_or_else(|| {
        perp_invariant_err(format!(
            "{side_label} entry for order {:?} not found during cancel",
            order_id
        ))
    })?;
    Ok(entries
        .remove(idx)
        .expect("index from find_entry_by_price_id is in bounds"))
}

/// A **rough, conservative, pre-walk** filter that refuses limit orders which cannot POSSIBLY be
/// afforded — so the node does not walk a large book to arrive at a certain rejection.
///
/// # Why it exists
///
/// `placeOrder` is charged a FLAT `PLACE_ORDER_GAS` and the match walk has no cap — no `MAX_WALK`,
/// no `MAX_FILLS`, no `MAX_LEVELS`. An attacker therefore pays a fixed price for unbounded CPU by
/// crossing a deep book with an order they can never fund. The authoritative gates all sit AFTER the
/// walk, by design (`finalize_compute` prices realised fills, `rest_in_book` prices the real rest),
/// and that is right — this does not replace them.
///
/// # It is a FILTER, not a DECISION
///
/// The one thing it must satisfy is
///
/// ```text
///     bound <= true_requirement            (whenever every unit either fills or rests)
/// ```
///
/// because then `available < bound  ⟹  available < true_requirement`, and the early reject can
/// never be wrong. Being LOOSE is harmless: a bound of 0 simply means no filtering. Nothing may ever
/// treat this as authoritative — accepting here says nothing at all.
///
/// ⚠️ The condition is not "unconditionally". A unit that EXPIRES needs no margin at all, so for an
/// order whose remainder is discarded — an IOC or market order that runs out of book, or any order
/// that self-trade prevention STOPS (`EXPIRE_TAKER` / `EXPIRE_BOTH`) at its owner's own order — the
/// true requirement can be below the bound, and this filter refuses an order the full path would
/// have accepted (and expired). That is deliberate. Where the filter applies (guards 1–2 below) it
/// acts like Binance's full-size admission, which checks margin against the order's FULL size
/// before matching; where it does not, only what fills is margin-checked and the remainder simply
/// expires. Narrowing the filter to "units that will rest" would need the walk it exists to avoid —
/// skipping it for such orders would reopen the deep-walk DoS. Under the default `EXPIRE_MAKER` it
/// stays exact: cancelling the owner's own opposite-side orders can only shrink `max(Bid, Ask)`,
/// never grow the true requirement past the bound (pinned by `stp_matching::t23_*` and `t23b_*`).
///
/// # ⚠️ THE TRAP: a REDUCING order's true requirement can be zero or NEGATIVE
///
/// A sell that closes a long frees margin and realises PnL into the wallet, so its requirement is
/// ≤ 0 and `derived_can_afford` admits it at any balance (the B1 invariant). A positive lower bound
/// applied to such an order would falsely reject a POSITION-CLOSING order — far worse than a wasted
/// walk, because it blocks someone from reducing risk exactly when they most need to. Worse, the
/// magnitude of what a close frees is `pos.margin`, which `addPositionMargin` can make arbitrarily
/// large, so there is no fixed credit that bounds it.
///
/// So the filter simply DOES NOT APPLY unless the order is unambiguously exposure-increasing.
/// Two guards, both cheap, both read off the position blob:
///
/// 1. **The position must not offset the order** (`buy ⟹ amount ≥ 0`, `sell ⟹ amount ≤ 0`). No fill
///    can then have a closing leg, so nothing is released, `apply_position_fill`'s `closing_qty` is
///    0, and the moved `ooIM` branch is monotone increasing.
/// 2. **No resting order of the user's own on the order's side** in this market. The cover loop in
///    `finalize_compute` cancels SAME-SIDE orders only, so with none to cancel it cannot shrink the
///    baseline this bound is measured against — and if cover were ever needed the loop would run out
///    of orders and reject anyway.
///
/// # The bound
///
/// Every unit of the order must be funded either as fill margin or as `ooIM`, and both are priced
/// off a notional. `p_min` is a per-unit price that is `≤` every price any unit can be valued at:
///
/// * **SELL @ P** — fills land on bids `≥ P`; the remainder rests at `max(P, T) ≥ P`. So `≥ P`.
/// * **BUY @ P** — fills land on asks `≤ P` but `≥ best_ask`; the remainder rests at `P ≥ best_ask`.
///   So `≥ best_ask`, or `P` when the book is empty / `best_ask > P`.
/// * both are then clamped to `≤ mark`, because the FILL leg's contribution to the position notional
///   `N` is valued at the MARK while its cash cost is valued at the fill price, and the bound has to
///   be below both.
///
/// The bound itself is not hand-rolled: it is [`crate::margin::view::derived_requirement_delta`] —
/// the production `ooIM` delta — evaluated on a hypothetical where the WHOLE order rests at `p_min`.
/// Under guards 1+2 the moved branch is monotone in that notional, so the real requirement (any
/// fill/rest split) is at least this. Using the real function is deliberate: it inherits the `max()`
/// hedging rule and the leverage/tier handling for free, so a hedged order that is genuinely FREE
/// gets a bound of 0 and is not filtered.
///
/// `slack` covers integer flooring, which is the only place the inequality is tight: `calc_value`
/// floors once per FILL (`opening_value` is a per-fill sum, so ≤ one quantum lost per fill, and the
/// fill count is bounded by `quantity / min_quantity`), the fill/rest split floors once more, and
/// `ooIM`'s `round_up_div` plus `opening_margin`'s truncating division contribute a constant.
///
/// # A CROSS order: its own fill can CREDIT the available balance (cross margin P2c)
///
/// The bound above prices the ISOLATED requirement of the order's fills — their silo,
/// `opening_value / L` at the FILL price. A position CROSSED in force moves no silo: its fill adds
/// `PIM` at the MARK to the requirement inside `A`, and its `uPnL` — `N − opening_value`, the fill
/// priced against the mark — to `A` itself. Per fill that is `(N − value)/L − (N − value)` more
/// than the isolated requirement, i.e. the requirement is LOWER by `(1 − 1/L)` of the order's open
/// GAIN (a buy filled below the mark, a sell above it). An open loss only raises it. So
///
/// ```text
///     cross requirement  ≥  isolated requirement − open_gain,
///     open_gain ≤ q × (mark − best_ask)   (buy: every fill is at ≥ the touch it starts from)
///     open_gain ≤ q × (best_bid − mark)   (sell: every fill is at ≤ the best bid)
/// ```
///
/// and the bound subtracts that (each per-unit gain floored at 0, valued once — plus the slack
/// again for its own flooring). It stays a sound filter: refused here ⟹ `A < cross requirement`,
/// which the fills gate (`settlement::fills_are_covered`, no B1 escape for a cross open) refuses
/// too. Where the book offers no gain the cross bound is the isolated one, so the filter keeps its
/// teeth on the deep-walk orders it exists for.
///
/// The Open Loss `rest_in_book` charges a cross order's resting OPENING part (Binance F8) is left
/// out of the bound on purpose: a filled unit's loss against the mark is already inside the
/// fold's `A`, and the bound only has to stay below the true requirement, so omitting a cost term
/// can only loosen the filter, never make it refuse what the full path admits.
///
/// The cross report (§3.3 item 5) proposed one of two other fixes; neither is the right one now:
/// * "add `Σ_C max(0, uPnL_k)` to the upper bound on `A`" — moot since P2a: `available` here IS
///   the fold's `A`, which already holds every cross leg's `uPnL`, and the gate behind this filter
///   reads the same `A`. What the bound was missing is the ORDER's own gain, which no leg holds.
/// * "skip the filter for an account with cross positions" — misses a flat account's first cross
///   order (the gain is this order's own), and after the P3 flip every position is CROSSED by
///   default, so it would switch the deep-walk DoS filter off for every account.
///
/// # Cost
///
/// One `MarketHot` read (buy side, and a CROSSED sell's best bid; already warm), the position blob,
/// and — only when the bound comes out positive — one `derived_available_balance` fold over the
/// per-user market index, bounded by `MAX_USER_MARKETS = 16`. Never the book.
pub(crate) fn reject_provably_unaffordable_limit_order<H: PerpHost>(
    context: &mut H,
    user: Address,
    market_id: u64,
    side: Side,
    price: u64,
    quantity: u64,
    market: &crate::types::Market,
) -> Result<(), PerpError> {
    let pos = storage::load_position_ref(context, user, market_id)?;

    // ── Guard 1: the order must not be offset by the position (see ⚠️ above). ──
    // ── Guard 2: nor may the user hold resting orders on this side of this market. ──
    let inapplicable = match side {
        Side::Buy => pos.amount < 0 || pos.total_buy_notional != 0,
        Side::Sell => pos.amount > 0 || pos.total_sell_notional != 0,
    };
    if inapplicable {
        return Ok(());
    }

    // `p_min`: a per-unit price no unit of this order can be valued below.
    let side_min = match side {
        Side::Sell => price,
        Side::Buy => {
            let best_ask = storage::load_market_hot(context, market_id)?.best_ask;
            if best_ask != 0 && best_ask < price {
                best_ask
            } else {
                price
            }
        }
    };
    let p_min = side_min.min(market.mark_price);

    // Every arithmetic failure below means "cannot bound it" and therefore "do not filter". The
    // authoritative gates will raise the real error if there is one; turning an overflow in a
    // heuristic into a user-visible reject would be exactly the over-reach this function must not
    // commit.
    let Ok(min_notional) = calc_value(p_min, quantity, market.base_decimals, market.price_decimals)
    else {
        return Ok(());
    };
    let mut hypothetical = (*pos).clone();
    let own_notional = match side {
        Side::Buy => &mut hypothetical.total_buy_notional,
        Side::Sell => &mut hypothetical.total_sell_notional,
    };
    let Some(sum) = own_notional.checked_add(min_notional) else {
        return Ok(());
    };
    *own_notional = sum;
    let Ok(raw_bound) = crate::margin::view::derived_requirement_delta(market, &pos, &hypothetical)
    else {
        return Ok(());
    };

    // Flooring slack (see the doc comment). `fills_max` bounds the per-fill `calc_value` losses;
    // the `+ 4` covers the split floor plus the round-up/truncating divisions. Unscaled by leverage
    // on purpose — `L >= 1`, so the unscaled figure is never smaller than the scaled one.
    let fills_max = quantity / market.min_quantity.max(1);
    let slack = (fills_max as i128).saturating_add(4);
    let mut bound = raw_bound.saturating_sub(slack);

    // A CROSSED order's own open gain lowers its requirement (see "A CROSS order" above): credit
    // the most it can be, plus the slack once more for that term's own flooring. An ISOLATED
    // order's bound is exactly what it was.
    let cross = crate::margin::margin_type::effective_margin_type(&pos) == crate::types::MarginType::Crossed;
    if cross {
        let gain_per_unit = match side {
            Side::Buy => market.mark_price.saturating_sub(side_min),
            Side::Sell => storage::load_market_hot(context, market_id)?
                .best_bid
                .saturating_sub(market.mark_price),
        };
        let Ok(open_gain) =
            calc_value(gain_per_unit, quantity, market.base_decimals, market.price_decimals)
        else {
            return Ok(());
        };
        bound = bound.saturating_sub(open_gain as i128).saturating_sub(slack);
    }
    if bound <= 0 {
        return Ok(());
    }

    let available = crate::margin::view::derived_available_balance(context, user)?;
    if available < bound {
        // The rest gate's and the fills gate's own string for the same cause — the cross one for an
        // order on a position CROSSED in force (`insufficient_margin_reject`).
        return Err(perp_err(insufficient_margin_reject(cross)));
    }
    Ok(())
}

/// The four TIFs of a LIMIT order — the only order type that has one.
///
/// Returns the expiry reason when the order's remainder EXPIRED (`OrderStatus::Expired`), `None`
/// when it filled completely or rested — see [`cancel_unfilled_remainder`].
fn execute_limit_order<H: PerpHost>(
    context: &mut H,
    account: Address,
    order_id: [u8; 32],
    market_id: u64,
    price: u64,
    quantity: u64,
    client_order_id: [u8; 16],
    side: Side,
    tif: TimeInForce,
    market: &crate::types::Market,
    taker_order: &mut Order,
    pending_placed: &mut Option<PendingOrderPlaced>,
) -> Result<Option<ExpiryReason>, PerpError> {
    // BEFORE the walk, and before `check_fok_feasibility`'s walk too — that is the entire point.
    // Applies to all four TIFs: PostOnly does not walk, but the filter is sound for it as well and
    // raises the identical reject its own gate would, so scoping it would only add a case to reason
    // about.
    reject_provably_unaffordable_limit_order(
        context, account, market_id, side, price, quantity, market,
    )?;
    let kind = OrderKind::Limit(tif);
    match tif {
        TimeInForce::PostOnly => {
            // No match runs → the do-not-cross BBO is still current at rest; thread it in.
            let bbo = ensure_post_only_does_not_cross(context, market_id, side, price)?;
            raise_rest_refusal(rest_in_book(
                context,
                account,
                &order_id,
                market_id,
                side,
                price,
                quantity,
                tif,
                client_order_id,
                OrderFlags::of(taker_order),
                market,
                Some(bbo),
                // PostOnly never matches, so there is nothing deferred to flush.
                None,
                pending_placed,
            )?)
            .map(|()| None)
        }
        TimeInForce::Gtc => {
            // ── ATOMIC MATCH + REST ──────────────────────────────────────────────────────────
            // GTC is the ONE kind that rests its remainder, so it is the one kind with a decision
            // AFTER the match. `match_order` therefore writes nothing: the decided-but-unwritten
            // `MatchOutcome` is handed to `rest_in_book`, which resolves the rest FIRST and flushes
            // it only once the answer is "rested". Either both happen or neither does, and a
            // refusal is a clean whole-order reject with zero writes — see [`MatchOutcome`].
            let outcome = match_order(
                context,
                account,
                &order_id,
                market_id,
                side,
                price,
                quantity,
                kind,
                market,
                false,
                taker_order,
            )?;
            if outcome.remaining == 0 {
                // Fully filled: no rest phase, so the barrier is right here.
                outcome.apply(context, side, market, pending_placed)?;
                return Ok(None);
            }
            // ── STP stopped the walk: the remainder EXPIRES, it never rests ─────────────────
            //
            // `EXPIRE_TAKER` / `EXPIRE_BOTH` met the taker's own resting order. Resting the rest
            // here would put this order on the book BEHIND an order of its own owner that it
            // crosses — exactly the self-cross STP exists to refuse — so like Binance
            // (`EXPIRED_IN_MATCH`) the remainder is discarded and the fills before the stop stand.
            // No rest phase, so the barrier is immediate, as on the IOC arm.
            if outcome.stp_taker_expired {
                let expiry = cancel_unfilled_remainder(
                    taker_order,
                    outcome.remaining,
                    ExpiryReason::SelfTradePrevention,
                );
                outcome.apply(context, side, market, pending_placed)?;
                return Ok(expiry);
            }
            let remaining = outcome.remaining;
            raise_rest_refusal(rest_in_book(
                context,
                account,
                &order_id,
                market_id,
                side,
                price,
                remaining,
                tif,
                client_order_id,
                OrderFlags::of(taker_order),
                market,
                // GTC: matching ran, so the BBO is read inside `rest_in_book` — pre-flush for the
                // band gate, and again post-flush for the cache write. See the notes there.
                None,
                Some(outcome),
                pending_placed,
            )?)
            .map(|()| None)
        }
        TimeInForce::Ioc => {
            let outcome = match_order(
                context,
                account,
                &order_id,
                market_id,
                side,
                price,
                quantity,
                kind, // IOC: unmatched remainder is dropped, never rested
                market,
                false,
                taker_order,
            )?;
            // No rest phase → nothing can refuse after the match, so the barrier is immediate.
            let expiry = cancel_unfilled_remainder(
                taker_order,
                outcome.remaining,
                immediate_expiry_reason(outcome.stp_taker_expired),
            );
            outcome.apply(context, side, market, pending_placed)?;
            Ok(expiry)
        }
        TimeInForce::Fok => {
            check_fok_feasibility(context, market_id, side, price, quantity, market)?;
            let outcome = match_order(
                context,
                account,
                &order_id,
                market_id,
                side,
                price,
                quantity,
                kind, // FOK: fully filled or rejected — never rests
                market,
                false,
                taker_order,
            )?;
            // BEFORE the barrier, not after: `match_order` already rejects an unfillable FOK
            // pre-write, so this is a belt-and-braces restatement — and a restatement of a REJECT
            // belongs on the write-clean side of the barrier, not behind it.
            ensure_fok_filled(outcome.remaining)?;
            // STP never applies to a FOK taker (`OrderKind::stp_applies`), so nothing stopped it.
            debug_assert!(!outcome.stp_taker_expired, "STP stopped a FOK walk");
            outcome.apply(context, side, market, pending_placed)?;
            Ok(None)
        }
    }
}

/// Both `RestOutcome` refusals, raised as the clean whole-order reject they now are.
///
/// ONE shape for both, and for every caller. `rest_in_book` decides both write-clean and flushes
/// nothing when it refuses, so there is no longer a caller for which "raise it" is unsafe — the
/// split that used to live here (PostOnly rejects / a crossing GTC expires its remainder) existed
/// only because the GTC path had already written by the time it asked. Keeping one funnel is
/// deliberate: it is what stops the next refusal added to `RestOutcome` from being handled
/// differently by one caller than another.
pub(super) fn raise_rest_refusal(outcome: RestOutcome) -> Result<(), PerpError> {
    match outcome {
        RestOutcome::Rested => Ok(()),
        RestOutcome::RefusedTooGoodBest => Err(perp_err(TOO_GOOD_BEST_REJECT)),
        RestOutcome::RefusedUnaffordable { cross } => {
            Err(perp_err(insufficient_margin_reject(cross)))
        }
        RestOutcome::RefusedUnderCrossMaintenance => Err(perp_err(CROSS_MAINTENANCE_REJECT)),
    }
}

/// A market order: **an immediate-or-cancel with a price band**, and nothing else. It has no TIF to
/// branch on ([`OrderKind::Market`] carries none), so this path is straight-line — match from the
/// best price inward until the order is filled, the band stops the walk, or the book runs out, then
/// discard whatever is left (`Expired`). It never rests, and it has no all-or-nothing mode: the
/// `Market + FOK` branch that used to live here was a product no exchange offers.
///
/// `price` is the ABI's (ignored) price field, threaded through only because `match_order` takes a
/// limit price; `OrderKind::Market` makes it dead on this path — see `is_limit` there.
fn execute_market_order<H: PerpHost>(
    context: &mut H,
    account: Address,
    order_id: [u8; 32],
    market_id: u64,
    price: u64,
    quantity: u64,
    side: Side,
    market: &crate::types::Market,
    taker_order: &mut Order,
    pending_placed: &mut Option<PendingOrderPlaced>,
) -> Result<Option<ExpiryReason>, PerpError> {
    let outcome = match_order(
        context,
        account,
        &order_id,
        market_id,
        side,
        price,
        quantity,
        OrderKind::Market, // never rests, never all-or-nothing
        market,
        false,
        taker_order,
    )?;
    // A market order has NO rest phase, so nothing can refuse after the match: the barrier is
    // immediate and this path is byte-identical to the pre-hoist code.
    let expiry = cancel_unfilled_remainder(
        taker_order,
        outcome.remaining,
        immediate_expiry_reason(outcome.stp_taker_expired),
    );
    outcome.apply(context, side, market, pending_placed)?;
    Ok(expiry)
}

pub(super) fn cancel_order_core<H: PerpHost>(
    account: Address,
    order_id: [u8; 32],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let order = storage::load_order(context, &order_id)?
        .ok_or_else(|| perp_err("cancelOrder: order not found"))?;

    if order.owner != account.0 .0 {
        return Err(perp_err("cancelOrder: not owner"));
    }
    // An UNTRIGGERED conditional order shares this id namespace and says `Open`, but it is in no
    // book level and no per-user entry list — it lives only in a trigger index (or the trailing
    // structure). The book-cancel path below would lower the live count of whatever REAL level
    // sits at its (side, price), maybe drop that price from the index, and only then fail on the
    // missing entry: after writes commit-only #23 keeps, with the record left behind to do it
    // again. Refused here, as a pure read, the mirror of `cancelAlgoOrder`'s "already triggered"
    // guard. Worded as "not cancellable" so a batch item reports `NotCancellable`.
    if order.trigger_price != 0 {
        return Err(perp_err(
            "cancelOrder: order not cancellable (an untriggered conditional order — use \
             cancelAlgoOrder)",
        ));
    }
    if !matches!(
        order.status,
        OrderStatus::Open | OrderStatus::PartiallyFilled
    ) {
        return Err(perp_err("cancelOrder: order not cancellable"));
    }

    let market_id = order.market_id;
    let market = storage::load_market_ref(context, market_id)?
        .ok_or_else(|| perp_err("cancelOrder: unknown market"))?;
    execute_order_cancellation(
        context,
        account,
        market_id,
        order_id,
        order,
        &market,
        // Explicit cancel: no matching ran in this call, so the BBO cache is live.
        remove_from_book_after_cancel,
        CancelReason::User,
    )?;
    Ok(Bytes::new())
}
