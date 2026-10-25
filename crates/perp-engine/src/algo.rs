//! Conditional (algo) orders — stop-loss / take-profit.
//!
//! # Why this is its own selector family and not flags on `placeOrder`
//!
//! Binance moved the whole conditional family OFF `/fapi/v1/order` onto the Algo Order API:
//! `STOP`, `TAKE_PROFIT`, `STOP_MARKET` and `TRAILING_STOP_MARKET` all answer `-4120` on the old
//! endpoint ("Please use the Algo Order API endpoints instead"), measured four-for-four on a live
//! account. The precompile analogue of "a different endpoint" is a different SELECTOR.
//!
//! # The shape of a conditional order's life
//!
//! ```text
//! placeAlgoOrder  ->  lives ONLY in the per-market trigger index
//!                     not in the book, no price level, no queue position,
//!                     no open-order requirement, no margin  (Binance: "no margin
//!                     check when placing conditional orders")
//!        |
//!     trigger fires on the MARK
//!        v
//!                 ->  becomes an ORDINARY order and is routed through the ordinary
//!                     placement path, where reduce-only admission, the fill-time price
//!                     band and the fee schedule all apply exactly as they always do
//! ```
//!
//! Triggering itself is NOT in this module — it belongs to the price-driven sweep next to the band
//! GC and the liquidation sweep, and lands separately.
//!
//! # Phase scope
//!
//! Only reduce-only / closePosition conditionals are accepted. Binance performs no margin check
//! before trigger, which leaves "triggered but cannot afford to open" as a case its own
//! documentation does not describe; a reduce-only conditional has a zero open-order requirement
//! before AND after trigger, so the case cannot arise here at all.
//!
//! # closePosition (Binance parity)
//!
//! * `reduceOnly` and `closePosition` are EXCLUSIVE (Binance: reduceOnly "cannot be sent with
//!   closePosition=true"); a closePosition order is reduce-only by construction.
//! * It is BOUND TO THE POSITION open when it is placed — Binance's `GTE_GTC`. Placement needs an
//!   open position with the order on its closing side, and records that position's
//!   `PerpPosition::generation`. Once the position has gone flat or flipped, the order is spent
//!   when its trigger fires (`AlgoOrderTriggered`, then `AlgoOrderCancelled(PositionClosed = 11)`),
//!   even if a new position has been opened the same way since. The expiry is LAZY: closing a
//!   position touches no order, so until it would fire the dead order still lists in
//!   `getOpenAlgoOrders` and still counts toward the caps — cancel it to free the slot sooner.
//! * There is NO one-per-direction limit (Binance's `-4130` is unconfirmed outside `GTE_GTC`, and
//!   the 32-per-market cap already bounds the list).
//! * There is NO one-shot "close position now" selector, and none is needed: a reduce-only MARKET
//!   `placeOrder` with `quantity ≥ |position|` is admitted and truncated to `|position|` (an
//!   immediate order is bounded by the position alone, `reduce_only::closing_capacity`), which is
//!   Binance's own recipe. Pinned by
//!   `trading::tests::reduce_only_e2e::a_market_reduce_only_order_is_still_truncated_to_the_position`.
//!
//! # Trailing stops
//!
//! `TRAILING_STOP_MARKET` (`orderType = 4`) shares this selector, the record, both caps, the owner's
//! list, every cancel path and `fire()` — but not the trigger index. Its trigger MOVES with the best
//! mark since activation, so it lives in the per-market structure in [`crate::trailing`], which
//! documents the semantics, the invariants and the per-tick work bound. `triggerPrice` is the
//! activation price (0 = activate now, at the stored mark), `price` the callback rate in basis
//! points (10..=1000, step 10). Reduce-only is REQUIRED and closePosition REFUSED (Binance offers
//! it only on the `_MARKET` stop pair). It is position-bound like a closePosition order: placement
//! needs an open position on its closing side, and it is spent with `PositionClosed` if that
//! position ended — checked when it activates and when it fires.
//!
//! # Every conditional order is refused if it would fire NOW
//!
//! Binance `-2021` "Order would immediately trigger": at placement, the sweep's own predicate
//! (`trigger_fires`, inclusive) is evaluated on the STORED mark, and a trigger it is already
//! satisfied by is refused (`placeAlgoOrder: order would immediately trigger`). Before the first
//! oracle tick that mark is the market's `initialMarkPrice`; a zero mark — unreachable through
//! the ABI — has no reference and skips the check, as the price band does. The refusal is
//! TRANSIENT (the next tick may lift it), so on the signed path it comes after the signature is
//! burned, like every other refusal that reads the mark or the owner's state.

use crate::host::PerpHost;
use crate::interface::IPerpDex::{
    self, cancelAlgoOrderCall, cancelAlgoOrderSignedCall, getAlgoOrderCall, getOpenAlgoOrdersCall,
    placeAlgoOrderCall, placeAlgoOrderSignedCall,
};
use crate::types::{
    AlgoOrderType, CancelReason, Order, OrderKind, OrderStatus, OrderType, Side, StpMode,
    TimeInForce, WorkingType, ALGO_TYPE_CONDITIONAL,
};
use crate::{errors::perp_err, storage, PerpError, PERP_DEX_ADDRESS};
use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use primitives::{Address, Bytes, FixedBytes, Log};

/// Open orders one user may hold in one market: resting book orders on BOTH sides plus untriggered
/// conditional orders. Binance USDⓈ-M's `MAX_NUM_ORDERS` filter is 200 too (`-2025` "Reach max open
/// order limit.").
///
/// # Why it exists
///
/// Every per-user walk over a market's orders is bounded by it: the reduce-only prefix and eviction
/// passes, the cover-cancel loop in taker settlement, and liquidation's / ADL's
/// `cancel_all_orders_for_market`, which runs inside an already-capped sweep. It also bounds how
/// many of a user's OWN orders a single taker can meet on the opposite side of the book, and so
/// how many self-trade prevention can cancel, since STP has no per-order cancel limit: at most 200
/// for one `EXPIRE_MAKER` taker, and 199 for a fired conditional, which left its own count before
/// it was placed. The trigger leg of one oracle tick fires at most `MAX_SWEEP_ACTIONS_PER_UPDATE`
/// = 50 of them, so STP adds at most 50 × 199 cancels to `updateIndexPrice` (the gas note there,
/// in `call.rs`, says why that is not priced).
///
/// # Where it is enforced
///
/// At placement only, and only for an order that can REST (GTC, PostOnly) or for a conditional
/// order; nothing already open is ever cancelled by it. The count is taken BEFORE the match, so at
/// the cap a GTC that would have filled completely is refused too — the same "filter at submission"
/// shape as Binance. IOC / FOK / market orders and liquidation closes are never refused by it, so a
/// user at the cap can always close. A triggered conditional order is not re-checked: it left the
/// conditional list before it was placed, so it converts one counted order into another and cannot
/// raise the count.
///
/// `MAX_ALGO_ORDERS_PER_MARKET` is a sub-cap INSIDE this one, not on top of it.
pub const MAX_OPEN_ORDERS_PER_MARKET: usize = 200;

/// Untriggered conditional orders one user may hold in one market.
///
/// Not cosmetic: liquidation and ADL both call `cancel_all_orders_for_market`, which must clear
/// conditional orders too (they are not in the book, so walking the book would miss them), and
/// that runs INSIDE an already-capped sweep. An uncapped per-user list would put unbounded work
/// on the liquidation path — the same shape as every other cap in this engine.
///
/// Binance's limit is 200 per ACCOUNT across all symbols; this is per (user, market), which is
/// the unit the cancel-all walk actually pays for.
pub const MAX_ALGO_ORDERS_PER_MARKET: usize = 32;

const FLAG_REDUCE_ONLY: u8 = 1 << 0;
const FLAG_CLOSE_POSITION: u8 = 1 << 1;
/// Bits 3:2 are the self-trade-prevention mode, decoded by the SAME `StpMode::from_flags` as
/// `placeOrder`'s — the mode set here is the one the triggered order matches with.
const KNOWN_FLAGS: u8 = FLAG_REDUCE_ONLY | FLAG_CLOSE_POSITION | StpMode::FLAG_MASK;

/// Everything `placeAlgoOrder` and its signed twin agree on before anything is written.
struct ValidatedAlgo {
    side: Side,
    kind: AlgoOrderType,
    trigger_price: u64,
    trigger_up: bool,
    price: u64,
    quantity: u64,
    tif: TimeInForce,
    reduce_only: bool,
    close_position: bool,
    /// Inherited by the ordinary order the trigger produces.
    stp: StpMode,
    /// `TRAILING_STOP_MARKET`'s callback rate in bps; 0 for every other type. For a trailing order
    /// `trigger_price` is the activation price as SENT — 0 means "activate now at the stored mark".
    trail_bps: u16,
}

#[allow(clippy::too_many_arguments)]
fn validate(
    algo_type: u8,
    side: u8,
    order_type: u8,
    trigger_price: u64,
    price: u64,
    quantity: u64,
    tif: u8,
    working_type: u8,
    flags: u8,
    market: &crate::types::Market,
) -> Result<ValidatedAlgo, PerpError> {
    if algo_type != ALGO_TYPE_CONDITIONAL {
        return Err(perp_err("placeAlgoOrder: algoType must be CONDITIONAL"));
    }
    // ⚠️ Rejected rather than substituted. Binance's DEFAULT working type is CONTRACT_PRICE, so a
    // client ported from there which omits the field is relying on last-trade semantics; quietly
    // triggering it off the mark instead would be the worst of both worlds.
    if working_type != WorkingType::MarkPrice as u8 {
        return Err(perp_err(
            "placeAlgoOrder: workingType must be MARK_PRICE (CONTRACT_PRICE is not supported)",
        ));
    }
    let side = Side::from_u8(side).ok_or_else(|| perp_err("placeAlgoOrder: invalid side"))?;
    let kind = AlgoOrderType::from_u8(order_type)
        .ok_or_else(|| perp_err("placeAlgoOrder: invalid orderType"))?;
    let trailing = kind == AlgoOrderType::TrailingStopMarket;
    // A trailing stop's `triggerPrice` is its ACTIVATION price, and 0 is meaningful there:
    // "activate now, at the stored mark" (Binance's default activatePrice = the latest price).
    if trigger_price == 0 && !trailing {
        return Err(perp_err("placeAlgoOrder: triggerPrice must be non-zero"));
    }
    if trigger_price % market.tick_size != 0 {
        return Err(perp_err("placeAlgoOrder: triggerPrice must be a multiple of tickSize"));
    }
    if flags & !KNOWN_FLAGS != 0 {
        return Err(perp_err("placeAlgoOrder: unknown flag bits"));
    }
    let stp = StpMode::from_flags(flags)
        .ok_or_else(|| perp_err("placeAlgoOrder: invalid self-trade prevention mode"))?;
    let reduce_only = flags & FLAG_REDUCE_ONLY != 0;
    let close_position = flags & FLAG_CLOSE_POSITION != 0;

    // Binance: `reduceOnly` "cannot be sent with closePosition=true". The pair is not a stronger
    // order, only an ambiguous one: a closePosition order is reduce-only by construction (it
    // closes `|position|` at trigger and the trigger places it reduce-only), so accepting both
    // bits would give one order two spellings and the record a flag that says nothing.
    if reduce_only && close_position {
        return Err(perp_err(
            "placeAlgoOrder: reduceOnly cannot be sent with closePosition",
        ));
    }

    // TRAILING_STOP_MARKET: reduceOnly REQUIRED, closePosition REFUSED — Binance offers
    // closePosition only on STOP_MARKET / TAKE_PROFIT_MARKET. Relaxing either later is a change
    // here alone: `fire` already sizes a closePosition order from the position.
    if trailing {
        if close_position {
            return Err(perp_err(
                "placeAlgoOrder: closePosition is not supported for TRAILING_STOP_MARKET",
            ));
        }
        if !reduce_only {
            return Err(perp_err("placeAlgoOrder: TRAILING_STOP_MARKET requires reduceOnly"));
        }
    }

    // Phase scope — see the module docs for why this restriction removes a whole failure class
    // rather than merely deferring it.
    if !reduce_only && !close_position {
        return Err(perp_err(
            "placeAlgoOrder: this build accepts only reduceOnly or closePosition conditionals",
        ));
    }

    let is_market = kind.is_market();
    if close_position {
        if !is_market {
            return Err(perp_err(
                "placeAlgoOrder: closePosition requires STOP_MARKET or TAKE_PROFIT_MARKET",
            ));
        }
        if quantity != 0 {
            return Err(perp_err("placeAlgoOrder: closePosition and quantity are exclusive"));
        }
    } else if quantity == 0 {
        return Err(perp_err("placeAlgoOrder: quantity must be non-zero"));
    }

    if trailing {
        // `price` carries the CALLBACK RATE in basis points (Binance's `callbackRate` × 100):
        // 0.1 %–10 % in 0.1 % steps, the grid that bounds the bucket directory at 100 entries.
        if !crate::trailing::valid_callback(price) {
            return Err(perp_err(
                "placeAlgoOrder: callbackRate (price) must be 10..=1000 bps in steps of 10",
            ));
        }
        if tif != 0 {
            return Err(perp_err("placeAlgoOrder: tif is unused for a *_MARKET type, send 0"));
        }
    } else if is_market {
        if price != 0 {
            return Err(perp_err("placeAlgoOrder: price must be 0 for a *_MARKET type"));
        }
        if tif != 0 {
            return Err(perp_err("placeAlgoOrder: tif is unused for a *_MARKET type, send 0"));
        }
    } else {
        if price == 0 {
            return Err(perp_err("placeAlgoOrder: price is required for a limit type"));
        }
        if price % market.tick_size != 0 {
            return Err(perp_err("placeAlgoOrder: price must be a multiple of tickSize"));
        }
    }
    let tif = TimeInForce::from_u8(tif).ok_or_else(|| perp_err("placeAlgoOrder: invalid tif"))?;

    if quantity != 0 {
        if quantity % market.step_size != 0 {
            return Err(perp_err("placeAlgoOrder: quantity must be a multiple of stepSize"));
        }
        if quantity > market.max_quantity {
            return Err(perp_err("placeAlgoOrder: quantity exceeds maxQuantity"));
        }
        // ⚠️ No minQuantity check: a reduce-only order is exempt from the minimum, measured on
        // Binance (R16) — a single step against a 5-unit floor was accepted. The whole point is to
        // be able to close a dust remainder.
    }

    // The "would immediately trigger" refusal and the activate-now mark check read the STORED
    // MARK, so they are not here: see `place_core`.
    let trigger_up = kind.trigger_up(side);

    Ok(ValidatedAlgo {
        side,
        kind,
        trigger_price,
        trigger_up,
        price,
        quantity,
        tif,
        reduce_only,
        close_position,
        stp,
        trail_bps: if trailing { price as u16 } else { 0 },
    })
}

/// `placeAlgoOrder`'s refusal for an account in liquidation (owner decision Q11) — Binance's
/// `-2023`. Unreachable while `validate` admits only reducing conditionals; see `place_core`.
const ALGO_IN_LIQUIDATION_REJECT: &str =
    "placeAlgoOrder: -2023 User in liquidation mode now. Only reduceOnly and closePosition \
     conditionals are accepted";

/// Shared tail: write the record, index it, emit. `algo_id` is already derived and, on the signed
/// path, its signature already burned.
#[allow(clippy::too_many_arguments)]
fn place_core<H: PerpHost>(
    context: &mut H,
    account: Address,
    market_id: u64,
    market: &crate::types::Market,
    algo_id: [u8; 32],
    v: ValidatedAlgo,
    client_algo_id: FixedBytes<16>,
    flags: u8,
) -> Result<Bytes, PerpError> {
    // ── -2023: an account in liquidation places only reducing orders (owner decision Q11) ────────
    //
    // The shared rule (`trading::refuse_unless_reducing_in_liquidation`). Every conditional this
    // build accepts IS reducing — `validate` admits only reduceOnly or closePosition, and a
    // trailing stop must be reduceOnly — so it passes all of them today, at no cost; it is here so
    // that relaxing `validate` cannot open a back door past the freeze. Transient, so after the
    // signed path's burn, like the `-2021` check below.
    crate::trading::refuse_unless_reducing_in_liquidation(
        context,
        account,
        v.reduce_only || v.close_position,
        ALGO_IN_LIQUIDATION_REJECT,
    )?;

    // ── Would it fire on the stored mark right now? Then refuse it (Binance `-2021`) ─────────────
    //
    // Evaluated with THE SWEEP'S OWN PREDICATE (`trigger_fires`) on the mark the next sweep would
    // see first, so "would immediately trigger" means exactly "the next oracle tick fires it if the
    // mark does not move" — including equality, where the sweep fires. Accepting it instead would
    // not execute it now (triggers run only inside the oracle sweep), it would execute it at
    // whatever the NEXT mark is: a stop the caller may have meant as a limit-style "only below
    // here" is filled at a price nobody chose. Binance answers "Order would immediately trigger".
    //
    // ⚠️ HERE and not in `validate`, because it is TRANSIENT: the mark moves every tick, so the
    // same request can be refused now and accepted one tick later. The signed path runs `validate`
    // BEFORE it burns the signature and this function AFTER, and a transient refusal must burn
    // (`trading::run_place_order_signed`): otherwise a refused signature stays live for the rest of
    // its recv window, and an observer can land it once the mark moves back — at a moment the
    // owner believed it had failed. Same for the activate-now check below. Still pre-write on both
    // paths.
    //
    // `mark_price == 0` skips the check: there is no reference to compare against, the same
    // convention as the price band (`math::price_band` — "mark == 0: no band"). It is unreachable
    // through the ABI — `addMarket` refuses `initialMarkPrice == 0` and the oracle path floors every
    // mark component at 1 — so before the first oracle tick the reference is simply the market's
    // `initialMarkPrice`, and only a hand-built fixture can carry 0.
    //
    // For a trailing stop the same predicate reads the ACTIVATION price in the activation
    // direction (`AlgoOrderType::trigger_up`): a SELL whose activation price the mark already
    // reaches is refused, which is Binance's `-2021` for an activatePrice on the wrong side. Send 0
    // to activate now instead.
    let trailing = v.trail_bps != 0;
    if v.trigger_price != 0
        && market.mark_price != 0
        && trigger_fires(v.trigger_up, v.trigger_price, market.mark_price)
    {
        return Err(perp_err("placeAlgoOrder: order would immediately trigger"));
    }
    // Activating now needs a mark to seed the peak with. Unreachable through the ABI, as above.
    if trailing && v.trigger_price == 0 && market.mark_price == 0 {
        return Err(perp_err(
            "placeAlgoOrder: no mark price to activate a trailing stop at",
        ));
    }

    let mut live = storage::load_user_algo_orders(context, account, market_id)?;
    if live.len() >= MAX_ALGO_ORDERS_PER_MARKET {
        return Err(perp_err(
            "placeAlgoOrder: too many open conditional orders in this market",
        ));
    }
    // The combined cap: an untriggered conditional is an open order too.
    if storage::book_order_count(context, account, market_id)? + live.len()
        >= MAX_OPEN_ORDERS_PER_MARKET
    {
        return Err(perp_err("placeAlgoOrder: max open orders reached"));
    }

    // A trailing stop with activation price 0 activates NOW, at the stored mark and its seq
    // (`IndexPriceState.timestamp`, 0 before the first oracle tick). Its record carries that mark
    // as the effective activation price, so `trigger_price` is never 0 for a conditional.
    let activate_at = if trailing && v.trigger_price == 0 {
        Some(storage::load_index_price_state(context, market_id)?.timestamp)
    } else {
        None
    };
    let trigger_price = if activate_at.is_some() {
        market.mark_price
    } else {
        v.trigger_price
    };

    let mut order = Order {
        owner: account.0 .0,
        market_id,
        side: v.side,
        // A trailing stop's `price` slot carried its callback rate, which lives in `trail_bps`;
        // what it becomes at trigger is a market order, whose record price is 0.
        price: if trailing { 0 } else { v.price },
        quantity: v.quantity,
        filled: 0,
        // The ORDINARY type the trigger will produce — a conditional order is not a third kind of
        // order, it is one of these two with a condition in front of it.
        order_type: if v.kind.is_market() {
            OrderType::Market
        } else {
            OrderType::Limit
        },
        tif: v.tif,
        status: OrderStatus::Open,
        reduce_only: v.reduce_only,
        trigger_price,
        trigger_up: v.trigger_up,
        close_position: v.close_position,
        stp: v.stp,
        // Set below, for a position-bound order only.
        position_generation: 0,
        trail_bps: v.trail_bps,
    };

    // ── Position binding (Binance `GTE_GTC`) ─────────────────────────────────────────────────────
    //
    // A position-bound order (`Order::is_position_bound` — a closePosition one or a trailing
    // stop) belongs to the position that is open NOW: it is refused unless one is open and the
    // order is on its closing side, and it records which life of the position that is. When it
    // fires, `fire` compares that against the position's current `generation`; if the position
    // went flat or flipped in between — even if a new one has since been opened the same way —
    // the order is spent with `PositionClosed` instead of closing a position it was never placed
    // against. That comparison is the whole expiry, so closing a position costs nothing extra. A
    // trailing stop is compared once more, when it activates (`trailing::run_activation_sweep`).
    //
    // Still pre-write: the position is only read.
    if order.is_position_bound() {
        let pos = storage::load_position_ref(context, account, market_id)?;
        if crate::reduce_only::closing_side(&pos) != Some(v.side) {
            return Err(perp_err(if trailing {
                "placeAlgoOrder: TRAILING_STOP_MARKET requires an open position on the closing side"
            } else {
                "placeAlgoOrder: closePosition requires an open position on the closing side"
            }));
        }
        order.position_generation = pos.generation;
    }

    storage::save_order(context, &algo_id, &order)?;
    if !trailing {
        storage::algo_index_insert(context, market_id, v.trigger_up, v.trigger_price, algo_id)?;
    }
    live.push(algo_id);
    storage::save_user_algo_orders(context, account, market_id, &live)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AlgoOrderPlaced {
            user: account,
            marketId: market_id,
            algoId: FixedBytes(algo_id),
            side: v.side as u8,
            orderType: v.kind as u8,
            // The EFFECTIVE activation price for a trailing stop sent with 0 (the stored mark it
            // activated at), so the event and `getAlgoOrder` agree.
            triggerPrice: trigger_price,
            price: v.price,
            quantity: v.quantity,
            tif: v.tif as u8,
            clientAlgoId: client_algo_id,
            flags,
        }
        .to_log_data(),
    });
    // After `AlgoOrderPlaced`, so an immediate activation's `AlgoOrderActivated` follows it.
    if trailing {
        crate::trailing::place(context, market_id, market, &algo_id, &order, activate_at)?;
    }
    Ok(Bytes::from(FixedBytes::<32>(algo_id).to_vec()))
}

/// Remove an untriggered conditional order from its trigger index (the fixed one, or the trailing
/// structure) and from the owner's list, and delete it. Idempotent in the index, so cancel /
/// cancel-all / the sweep may all reach the same id.
pub(crate) fn remove_algo_order<H: PerpHost>(
    context: &mut H,
    account: Address,
    market_id: u64,
    algo_id: &[u8; 32],
    order: &Order,
    reason: CancelReason,
) -> Result<(), PerpError> {
    if order.is_trailing() {
        crate::trailing::remove(context, market_id, algo_id, order)?;
    } else {
        storage::algo_index_remove(
            context,
            market_id,
            order.trigger_up,
            order.trigger_price,
            algo_id,
        )?;
    }
    retire_algo_order(context, account, market_id, algo_id, reason)
}

/// The tail every removal of an untriggered conditional shares, once it is out of its trigger
/// index: off the owner's list, record deleted, `AlgoOrderCancelled(reason)`.
pub(crate) fn retire_algo_order<H: PerpHost>(
    context: &mut H,
    account: Address,
    market_id: u64,
    algo_id: &[u8; 32],
    reason: CancelReason,
) -> Result<(), PerpError> {
    let mut live = storage::load_user_algo_orders(context, account, market_id)?;
    live.retain(|id| id != algo_id);
    storage::save_user_algo_orders(context, account, market_id, &live)?;
    storage::delete_order(context, algo_id)?;
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AlgoOrderCancelled {
            user: account,
            marketId: market_id,
            algoId: FixedBytes(*algo_id),
            reason: reason as u8,
        }
        .to_log_data(),
    });
    Ok(())
}

/// `placeAlgoOrder(...)`
pub fn run_place_algo_order<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = placeAlgoOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("placeAlgoOrder: invalid calldata"))?;
    let market = storage::load_market_ref(context, a.marketId)?
        .ok_or_else(|| perp_err("placeAlgoOrder: unknown market"))?;
    if !market.active {
        return Err(perp_err("placeAlgoOrder: market is not active"));
    }
    let v = validate(
        a.algoType,
        a.side,
        a.orderType,
        a.triggerPrice,
        a.price,
        a.quantity,
        a.tif,
        a.workingType,
        a.flags,
        &market,
    )?;
    let (algo_id, bumped) = crate::trading::peek_order_id(context, caller)?;
    let out =
        place_core(context, caller, a.marketId, &market, algo_id, v, a.clientAlgoId, a.flags)?;
    // The nonce bump is persisted only once the placement has succeeded, exactly as on
    // `placeOrder`. It used to be written BEFORE `place_core`, so `place_core`'s two capacity
    // rejects fired after a perp write — a commit-only #23 leak (the nonce advanced under a reverted
    // call) that only went unnoticed because the tests drove this handler below the call shell.
    crate::trading::commit_order_nonce(context, caller, bumped)?;
    Ok(out)
}

/// `cancelAlgoOrder(bytes32 algoId, uint64 marketId)`
pub fn run_cancel_algo_order<H: PerpHost>(
    input_bytes: &[u8],
    caller: Address,
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = cancelAlgoOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("cancelAlgoOrder: invalid calldata"))?;
    cancel_core(context, caller, a.algoId.0, a.marketId)?;
    Ok(Bytes::new())
}

fn cancel_core<H: PerpHost>(
    context: &mut H,
    account: Address,
    algo_id: [u8; 32],
    market_id: u64,
) -> Result<(), PerpError> {
    let order = storage::load_order(context, &algo_id)?
        .ok_or_else(|| perp_err("cancelAlgoOrder: not found"))?;
    if Address::from(order.owner) != account {
        return Err(perp_err("cancelAlgoOrder: not the owner"));
    }
    if order.trigger_price == 0 {
        return Err(perp_err(
            "cancelAlgoOrder: not a conditional order (already triggered — use cancelOrder)",
        ));
    }
    if order.market_id != market_id {
        return Err(perp_err("cancelAlgoOrder: marketId mismatch"));
    }
    remove_algo_order(context, account, market_id, &algo_id, &order, CancelReason::User)
}

/// `getAlgoOrder(bytes32 algoId)`
pub fn run_get_algo_order<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = getAlgoOrderCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getAlgoOrder: invalid calldata"))?;
    let o = storage::load_order(context, &a.algoId.0)?
        .ok_or_else(|| perp_err("getAlgoOrder: not found"))?;
    if o.trigger_price == 0 {
        return Err(perp_err("getAlgoOrder: not a conditional order"));
    }
    // Branch on the trailing marker FIRST: by `(order_type, trigger_up)` alone a trailing SELL
    // (market, activates upward) would read as a TAKE_PROFIT_MARKET.
    let trail = if o.is_trailing() {
        let tick = storage::load_market_ref(context, o.market_id)?
            .map(|m| m.tick_size)
            .unwrap_or(0);
        crate::trailing::view(context, o.market_id, &a.algoId.0, &o, tick)?
    } else {
        crate::trailing::TrailView::default()
    };
    let kind = if o.is_trailing() {
        AlgoOrderType::TrailingStopMarket
    } else if o.order_type == OrderType::Market {
        if o.trigger_up == matches!(o.side, Side::Buy) {
            AlgoOrderType::StopMarket
        } else {
            AlgoOrderType::TakeProfitMarket
        }
    } else if o.trigger_up == matches!(o.side, Side::Buy) {
        AlgoOrderType::Stop
    } else {
        AlgoOrderType::TakeProfit
    };
    Ok(Bytes::from(getAlgoOrderCall::abi_encode_returns(
        &crate::interface::IPerpDex::getAlgoOrderReturn {
            owner: Address::from(o.owner),
            marketId: o.market_id,
            side: o.side as u8,
            orderType: kind as u8,
            triggerPrice: o.trigger_price,
            // As placed: a trailing stop's `price` slot is its callback rate.
            price: if o.is_trailing() { o.trail_bps as u64 } else { o.price },
            quantity: o.quantity,
            tif: o.tif as u8,
            status: o.status as u8,
            reduceOnly: o.reduce_only,
            closePosition: o.close_position,
            selfTradePreventionMode: o.stp as u8,
            callbackRateBps: o.trail_bps,
            activated: trail.activated,
            peakPrice: trail.peak,
            currentStop: trail.stop,
        },
    )))
}

/// `getOpenAlgoOrders(address user, uint64 marketId)`
pub fn run_get_open_algo_orders<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = getOpenAlgoOrdersCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("getOpenAlgoOrders: invalid calldata"))?;
    let ids = storage::load_user_algo_orders(context, a.user, a.marketId)?;
    let out: Vec<FixedBytes<32>> = ids.into_iter().map(FixedBytes).collect();
    Ok(Bytes::from(getOpenAlgoOrdersCall::abi_encode_returns(&out)))
}

// ── Signed (API-key) variants ─────────────────────────────────────────────────────────────────
//
// The path real traders use: sign locally with a registered ed25519 key, let anyone relay and pay
// the gas. Binance's analogue is `X-MBX-APIKEY` + an HMAC over the query string; `keyId` names the
// key slot and the signature covers a fixed-layout canonical message including the CHAIN ID, so a
// signature minted for one chain cannot be replayed onto another.
//
// Validation order is the same on all signed entrypoints and is not arbitrary:
//   load_api_key -> check_recv_window -> check_api_key_expiry -> verify_ed25519 -> burn -> core

/// `placeAlgoOrderSigned(...)`
pub fn run_place_algo_order_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = placeAlgoOrderSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("placeAlgoOrderSigned: invalid calldata"))?;

    let api_key = storage::load_api_key(context, a.account, a.keyId)?
        .ok_or_else(|| perp_err("placeAlgoOrderSigned: no api key registered for account"))?;
    crate::trading::check_recv_window(context, a.timestamp, a.recvWindow)
        .map_err(|e| perp_err(&format!("placeAlgoOrderSigned: {e}")))?;
    crate::trading::check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("placeAlgoOrderSigned: {e}")))?;

    // Canonical fixed-layout message. ⚠️ No byte total is quoted: the array literal IS the length
    // and the closing `debug_assert_eq!` enforces it. A number in a comment here went stale three
    // times on the sibling entrypoints before it was removed.
    //   "perpdex_v1_algo" || chainId(8) || account(20) || algoType(1) || marketId(8) || side(1)
    //   || orderType(1) || triggerPrice(8) || price(8) || quantity(8) || tif(1) || workingType(1)
    //   || clientAlgoId(16) || flags(1) || timestamp(8) || recvWindow(8) || keyId(1)
    let mut msg = [0u8; 114];
    let mut c = crate::trading::write_signed_header(&mut msg, b"perpdex_v1_algo", context.chain_id());
    c = crate::trading::put(&mut msg, c, a.account.as_slice());
    c = crate::trading::put(&mut msg, c, &[a.algoType]);
    c = crate::trading::put(&mut msg, c, &a.marketId.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[a.side, a.orderType]);
    c = crate::trading::put(&mut msg, c, &a.triggerPrice.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &a.price.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &a.quantity.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[a.tif, a.workingType]);
    c = crate::trading::put(&mut msg, c, &a.clientAlgoId.0);
    c = crate::trading::put(&mut msg, c, &[a.flags]);
    c = crate::trading::put(&mut msg, c, &a.timestamp.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &a.recvWindow.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[a.keyId]);
    debug_assert_eq!(c, msg.len(), "placeAlgoOrderSigned message layout");

    crate::trading::verify_ed25519(&api_key.pubkey, &msg, &a.signature)
        .map_err(|e| perp_err(&format!("placeAlgoOrderSigned: {e}")))?;

    // algoId = keccak256(signature); the SAME hash keys the seen-signature replay set, exactly as
    // on `placeOrderSigned`.
    let algo_id: [u8; 32] = primitives::keccak256(a.signature.as_ref()).0;
    if storage::is_signature_seen(context, &algo_id)? {
        return Err(perp_err(
            "placeAlgoOrderSigned: duplicate signature (already submitted)",
        ));
    }

    // Validate BEFORE the burn, so a malformed request costs no write at all. Only refusals that
    // are PERMANENT for this message belong there; everything that reads the mark or the owner's
    // state (would-immediately-trigger, the position binding, the caps) is in `place_core`, after
    // the burn, because it can lift a tick later. Everything after this point either succeeds or
    // is a sanctioned reject-after-write.
    let market = storage::load_market_ref(context, a.marketId)?
        .ok_or_else(|| perp_err("placeAlgoOrderSigned: unknown market"))?;
    if !market.active {
        return Err(perp_err("placeAlgoOrderSigned: market is not active"));
    }
    let v = validate(
        a.algoType,
        a.side,
        a.orderType,
        a.triggerPrice,
        a.price,
        a.quantity,
        a.tif,
        a.workingType,
        a.flags,
        &market,
    )
    .map_err(|e| perp_err(&format!("placeAlgoOrderSigned: {e}")))?;

    // ── the signature is burned UNCONDITIONALLY, BEFORE placement ──
    // Same rule as every other signed entrypoint: a one-shot authorization is consumed by
    // SUBMISSION, not by success, the way an Ethereum nonce is spent by a failed transaction.
    // Burning only on success leaves a REJECTED signature replayable for the rest of its recv
    // window, which is reachable whenever the rejection was transient.
    let writes_before_burn = context.perp_write_count();
    storage::mark_signature_seen(context, &algo_id, a.timestamp)?;
    // Best-effort by design: this runs AFTER a committed burn, so surfacing its `Err` would
    // revert a call whose signature is already spent.
    let _ = crate::trading::gc_seen_buckets_best_effort(context, a.timestamp);
    let burn_writes = context.perp_write_count() - writes_before_burn;

    place_core(context, a.account, a.marketId, &market, algo_id, v, a.clientAlgoId, a.flags)
        .map_err(|e| match e {
            PerpError::Reject(msg) => PerpError::RejectAfterRetainedWrite {
                message: msg,
                sanctioned_writes: burn_writes,
            },
            other => other,
        })
}

/// `cancelAlgoOrderSigned(...)`
pub fn run_cancel_algo_order_signed<H: PerpHost>(
    input_bytes: &[u8],
    context: &mut H,
) -> Result<Bytes, PerpError> {
    let a = cancelAlgoOrderSignedCall::abi_decode_validate(input_bytes)
        .map_err(|_| perp_err("cancelAlgoOrderSigned: invalid calldata"))?;

    let api_key = storage::load_api_key(context, a.account, a.keyId)?
        .ok_or_else(|| perp_err("cancelAlgoOrderSigned: no api key registered for account"))?;
    crate::trading::check_recv_window(context, a.timestamp, a.recvWindow)
        .map_err(|e| perp_err(&format!("cancelAlgoOrderSigned: {e}")))?;
    crate::trading::check_api_key_expiry(context, &api_key)
        .map_err(|e| perp_err(&format!("cancelAlgoOrderSigned: {e}")))?;

    //   "perpdex_v1_algocancel" || chainId(8) || account(20) || algoId(32) || marketId(8)
    //   || timestamp(8) || recvWindow(8) || keyId(1)
    let mut msg = [0u8; 106];
    let mut c =
        crate::trading::write_signed_header(&mut msg, b"perpdex_v1_algocancel", context.chain_id());
    c = crate::trading::put(&mut msg, c, a.account.as_slice());
    c = crate::trading::put(&mut msg, c, &a.algoId.0);
    c = crate::trading::put(&mut msg, c, &a.marketId.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &a.timestamp.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &a.recvWindow.to_be_bytes());
    c = crate::trading::put(&mut msg, c, &[a.keyId]);
    debug_assert_eq!(c, msg.len(), "cancelAlgoOrderSigned message layout");

    crate::trading::verify_ed25519(&api_key.pubkey, &msg, &a.signature)
        .map_err(|e| perp_err(&format!("cancelAlgoOrderSigned: {e}")))?;

    // Burned unconditionally, after verification and before the cancel — exactly as
    // `cancelOrderSigned` does (see `run_place_order_signed`'s burn site for why SUBMISSION, not
    // success, spends a signed authorization).
    //
    // This adds no safety the deletion did not already give: a cancelled conditional order is
    // DELETED and algo ids are never reused (a signed one is `keccak256(signature)`, a direct one
    // comes off the owner's monotonic nonce), so a replayed cancel could only ever find nothing.
    // It was the ONE signed selector that skipped the burn, on that reasoning; it now matches the
    // other eight, so "a signature is spent on submission" holds for every signed entrypoint with
    // no exception to remember, and a replay is refused as what it is — a duplicate.
    let sig_hash: [u8; 32] = primitives::keccak256(a.signature.as_ref()).0;
    if storage::is_signature_seen(context, &sig_hash)? {
        return Err(perp_err(
            "cancelAlgoOrderSigned: duplicate signature (already submitted)",
        ));
    }

    // ── last pre-write fault has passed; the first write happens here ──
    let block_ts: u64 = context.timestamp();
    let writes_before_burn = context.perp_write_count();
    storage::mark_signature_seen(context, &sig_hash, a.timestamp)?;
    crate::trading::gc_seen_buckets_best_effort(context, block_ts)?;
    let burn_writes = context.perp_write_count().saturating_sub(writes_before_burn);

    cancel_core(context, a.account, a.algoId.0, a.marketId)
        .map_err(|e| e.after_retained_writes(burn_writes))?;
    Ok(Bytes::new())
}

// ── Trigger sweep ─────────────────────────────────────────────────────────────────────────────

/// THE firing predicate: does a trigger in direction `trigger_up` at `trigger_price` fire on
/// `mark`? Inclusive both ways. One definition for the sweep and for placement's
/// "would immediately trigger" refusal, so the two cannot disagree at the boundary.
#[inline]
pub(crate) fn trigger_fires(trigger_up: bool, trigger_price: u64, mark: u64) -> bool {
    if trigger_up {
        mark >= trigger_price
    } else {
        mark <= trigger_price
    }
}

/// Walk the trigger index and execute whatever the current mark fires.
///
/// # NOT latched — the condition must hold at EXECUTION time
///
/// Unlike the liquidation sweep, whose "under water" condition cannot un-happen, a trigger
/// condition can: mark 100 -> 95 -> 100 leaves a 97 stop satisfied only in the middle tick. This
/// sweep stores nothing and re-derives from the CURRENT mark every run, so a trigger the budget
/// could not reach does NOT fire later at a price that no longer exists. That is the same rule the
/// fill-time band imposes — the market, or our own budget, declined to execute, so it did not
/// execute — and it is deliberately NOT Binance's latching semantics.
///
/// The cost: a budget overflow can MISS a trigger rather than merely delay it, silently. The
/// window is narrow because `drainDeferredWork` is permissionless and **the mark does not change
/// within a block**, so a drain finishes the remaining triggers at the very price that fired them;
/// the exposure is only to the gap between oracle ticks with nobody draining.
///
/// # Walk order
///
/// Both index lists are sorted so the firing end is the FRONT, and the walk stops at the first
/// price that does not fire — so this costs what the mark MOVED, not what the market HOLDS.
pub(crate) fn run_trigger_sweep<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &crate::types::Market,
    mark_price: u64,
    budget: &mut u32,
) -> Result<bool, PerpError> {
    for trigger_up in [true, false] {
        let prices = if trigger_up {
            storage::load_algo_up_prices(context, market_id)?
        } else {
            storage::load_algo_down_prices(context, market_id)?
        };
        for trigger_price in prices {
            if !trigger_fires(trigger_up, trigger_price, mark_price) {
                break; // sorted: nothing further along this list can fire either
            }
            for algo_id in storage::load_algo_level(context, market_id, trigger_up, trigger_price)? {
                if *budget == 0 {
                    // Budget, not the market, stopped us: a later attempt IN THIS BLOCK finishes
                    // it at this same mark, which is what `deferred_work` is for.
                    return Ok(true);
                }
                let Some(order) = storage::load_order(context, &algo_id)? else {
                    // Cancelled out from under the index; drop the stale entry while we are here
                    // rather than paying to walk it again next tick.
                    storage::algo_index_remove(
                        context,
                        market_id,
                        trigger_up,
                        trigger_price,
                        &algo_id,
                    )?;
                    continue;
                };
                if order.trigger_price == 0 {
                    continue; // already triggered; not ours any more
                }
                fire(context, market_id, market, mark_price, &algo_id, &order, FireFrom::Index)?;
                *budget -= 1;
            }
        }
    }
    // The early `return Ok(true)` above is the ONLY way this sweep defers, so reaching here means
    // every firing trigger was executed. Nothing else in here is a deferral: a stale index entry
    // is cleaned up in passing, a trigger that fired but could not fill is EXPIRED by the ordinary
    // market path, and one the matcher REFUSED is cancelled (`TriggerRejected`) — each a final
    // outcome (the Binance-parity one), not a backlog.
    Ok(false)
}

/// Where a fired order came from — which decides who takes it out of its trigger index, and what
/// `AlgoOrderTriggered.triggerPrice` reports.
#[derive(Clone, Copy, Debug)]
enum FireFrom {
    /// The fixed-trigger index: `fire` removes it, and reports the stored trigger price.
    Index,
    /// The trailing structure: the walk has already unlinked it, and `stop` is the tick-rounded
    /// stop that fired (`trailing::display_stop`) — the stored `trigger_price` is the ACTIVATION
    /// price, which is not what fired it.
    Trailing { stop: u64 },
}

/// `fire` for an order the trailing fire walk has just unlinked.
pub(crate) fn fire_trailing<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &crate::types::Market,
    mark_price: u64,
    algo_id: &[u8; 32],
    order: &Order,
    stop: u64,
) -> Result<(), PerpError> {
    debug_assert!(order.is_trailing());
    fire(context, market_id, market, mark_price, algo_id, order, FireFrom::Trailing { stop })
}

/// Execute one triggered conditional order.
///
/// ⚠️ VALIDATE-THEN-APPLY is mandatory here, not stylistic: perp writes are commit-only (#23) and
/// a frame revert does NOT roll them back, so anything that can fail must fail BEFORE the first
/// write. Hence the capacity pre-check — `place_order_core` would otherwise reject a reduce-only
/// order it cannot fit, after having written.
///
/// # A placement the matcher refuses is an OUTCOME, not an error
///
/// The capacity pre-check cannot see the matcher's own rejects, and those are reachable from a
/// trigger: a limit conditional may carry any TIF, so a fired FOK may be unfillable, a fired
/// PostOnly may cross, and a fired GTC may be refused at rest (a new best quote outside the band,
/// or an unaffordable rest). By then this function HAS written — the conditional left its indexes
/// and `AlgoOrderTriggered` is logged — so propagating the reject would revert the ORACLE
/// transaction over retained perp writes and truncated logs, skip the rest of this tick's sweep
/// (including liquidations), and orphan the record (out of every index, never deleted).
///
/// So a GENUINE reject — `Reject` with the perp write counter unmoved, the same witness the batch
/// driver uses (`batch::drive_batch`) — is the trigger's terminal outcome: the record is deleted
/// and `AlgoOrderCancelled(TriggerRejected)` follows `AlgoOrderTriggered`, the same two-event shape
/// as the `qty == 0` expiry below (Binance: ALGO_UPDATE `REJECTED`, "denied by the matching
/// engine"). A placement reject is write-clean AND log-clean (its `OrderPlaced` is buffered and
/// never emitted), so nothing of the refused order survives. Anything else — a reject AFTER a
/// write, or a `Fatal` — still propagates: that is a real leak or a halt, not a market outcome.
fn fire<H: PerpHost>(
    context: &mut H,
    market_id: u64,
    market: &crate::types::Market,
    mark_price: u64,
    algo_id: &[u8; 32],
    order: &Order,
    from: FireFrom,
) -> Result<(), PerpError> {
    let owner = Address::from(order.owner);
    let pos = storage::load_position_ref(context, owner, market_id)?;

    // What the trigger produces — derived ONCE, and it feeds both the capacity rule below and the
    // placement at the end, so the two cannot disagree about what kind of order this is. A `_MARKET`
    // conditional becomes a market order (implicitly IOC); a limit one keeps the TIF it was placed
    // with.
    let kind = match order.order_type {
        OrderType::Market => OrderKind::Market,
        OrderType::Limit => OrderKind::Limit(order.tif),
    };

    // How much this order may actually close: THE SAME RULE `placeOrder`'s reduce-only admission
    // applies (`reduce_only::closing_capacity`), keyed on where the fired order sits in its owner's
    // own queue. An immediate one — the `_MARKET` pair, or a limit IOC / FOK — fills before every
    // resting order, so it is bounded by `|position|` alone, and the post-fill eviction pass trims
    // whatever resting reduce-only orders it leaves over-committed (Binance `er=4`). Only a fired
    // order that RESTS (GTC / PostOnly) is held to the prefix condition at its own limit price.
    //
    // `closePosition` means "whatever is open AT TRIGGER TIME" — of the position it was placed
    // against. A position-bound order whose position has ENDED since placement (gone flat or
    // flipped: its `generation` moved, see `place_core`) closes nothing, whatever is open now.
    let bound_position_gone =
        order.is_position_bound() && pos.generation != order.position_generation;
    let qty = if bound_position_gone {
        0
    } else {
        let capacity = match crate::reduce_only::closing_side(&pos) {
            Some(s) if s == order.side => crate::reduce_only::closing_capacity(
                context,
                owner,
                market_id,
                order.side,
                &pos,
                crate::reduce_only::QueuePosition::for_kind(kind, order.price),
            )?,
            // Flat, or the position flipped to the same side this order is on — it can only open
            // now, which a reduce-only order may never do.
            _ => 0,
        };
        let want = if order.close_position {
            pos.amount.unsigned_abs()
        } else {
            order.quantity
        };
        want.min(capacity)
    };
    // A bound order whose generation still matches is on the closing side of a live position (the
    // generation moves on every close and flip, and placement required the closing side), and it
    // fires immediately, so it always has something to place.
    debug_assert!(
        !order.is_position_bound() || bound_position_gone || qty > 0,
        "a position-bound conditional with a live generation found nothing to close"
    );

    // ── APPLY — nothing below may fail for a reason the caller could have caused ──
    //
    // The order leaves the conditional namespace FIRST, so the id is free for the ordinary record
    // `place_order_core` writes under it. Same id on purpose: a consumer stitches the conditional
    // half and the ordinary half of this order's life together without a mapping table.
    let reported_trigger = match from {
        FireFrom::Index => {
            storage::algo_index_remove(
                context,
                market_id,
                order.trigger_up,
                order.trigger_price,
                algo_id,
            )?;
            order.trigger_price
        }
        FireFrom::Trailing { stop } => stop,
    };
    let mut live = storage::load_user_algo_orders(context, owner, market_id)?;
    live.retain(|id| id != algo_id);
    storage::save_user_algo_orders(context, owner, market_id, &live)?;

    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::AlgoOrderTriggered {
            user: owner,
            marketId: market_id,
            algoId: FixedBytes(*algo_id),
            triggerPrice: reported_trigger,
            // The mark that FIRED it, not the trigger — so how far the mark jumped past the
            // trigger in one update is on the wire instead of being something to infer.
            markPrice: mark_price,
        }
        .to_log_data(),
    });

    if qty == 0 {
        // Nothing left to protect: the position closed or flipped, or — for a fired order that
        // would REST — is already fully committed to orders ahead of it. Binance expires a
        // reduce-only order in exactly this case. A position-bound order whose position ended says
        // so with its own code: it may well be on today's closing side (closed, then reopened the
        // same way), so 6's "would open" would send a consumer looking for a flip.
        let reason = if bound_position_gone {
            CancelReason::PositionClosed
        } else {
            CancelReason::ReduceOnlyWouldOpen
        };
        storage::delete_order(context, algo_id)?;
        context.log(Log {
            address: PERP_DEX_ADDRESS,
            data: IPerpDex::AlgoOrderCancelled {
                user: owner,
                marketId: market_id,
                algoId: FixedBytes(*algo_id),
                reason: reason as u8,
            }
            .to_log_data(),
        });
        return Ok(());
    }
    debug_assert!(market.active, "trigger sweep ran on an inactive market");

    // From here it is an ORDINARY order. The ordinary path owns everything that follows: the
    // fill-time band, fees, and — for the `_MARKET` pair — EXPIRING whatever the band would not
    // let through, which is Binance's behaviour for an unfilled market order and which we get for
    // free by not special-casing it.
    let writes_before = context.perp_write_count();
    let placed = crate::trading::place_order_core(
        owner,
        *algo_id,
        market_id,
        order.side as u8,
        if kind.is_limit() { order.price } else { 0 },
        qty,
        kind.order_type() as u8,
        // A market order reports `Ioc` — what it is.
        kind.tif() as u8,
        [0u8; 16],
        // Through the one encoder, so the fired order is reduce-only AND carries the mode set at
        // `placeAlgoOrder` — the trigger inherits it.
        crate::trading::encode_order_flags(true, order.stp),
        crate::trading::PlacementOrigin::Triggered,
        context,
    );
    match placed {
        Ok(_) => Ok(()),
        Err(PerpError::Reject(msg)) if context.perp_write_count() == writes_before => {
            // A write-clean `[INVARIANT]` would be absorbed here exactly as the batch driver absorbs
            // one; make it loud where it can be (tests, the correctness gate), not in an oracle tx.
            debug_assert!(
                !msg.starts_with("[INVARIANT] "),
                "a triggered placement hit an invariant: {msg}"
            );
            storage::delete_order(context, algo_id)?;
            context.log(Log {
                address: PERP_DEX_ADDRESS,
                data: IPerpDex::AlgoOrderCancelled {
                    user: owner,
                    marketId: market_id,
                    algoId: FixedBytes(*algo_id),
                    reason: CancelReason::TriggerRejected as u8,
                }
                .to_log_data(),
            });
            Ok(())
        }
        Err(e) => Err(e),
    }
}
