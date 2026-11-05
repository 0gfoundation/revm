//! The liquidation close: a liquidation's book leg, run as an internal IOC order.

use alloy_primitives::IntoLogData;
use crate::host::PerpHost;
use primitives::{Address, FixedBytes, Log};

use super::match_order;
use crate::{
        errors::perp_invariant_err,
    interface::IPerpDex,
    storage,
    types::{AccountUpdateReason, Market, Order, OrderKind, OrderStatus, Side, StpMode},
    PERP_DEX_ADDRESS,
    PerpError,
};

/// Execute the liquidation close as an internal market IOC order.
///
/// Runs the IOC against the book and returns the unfilled quantity. If the
/// book absorbs the entire position (`remaining == 0`) the position storage is
/// cleaned up here. If the book can only partially fill, the caller is
/// responsible for settling the residual (see `settle_liquidation_residual_at_mark_price`).
/// What a liquidation's book leg left behind.
pub(crate) struct LiquidationCloseOutcome {
    /// Unfilled quantity — the residual that goes to ADL.
    pub(crate) remaining: u64,
    /// The walk stopped on the distinct-maker cap, NOT on the book or the band. The cap is per
    /// match, so retrying gets a fresh allowance and can absorb more from the SAME book: this is
    /// same-block recoverable and must set `Market::deferred_work`.
    pub(crate) maker_cap_deferred: bool,
}

///
/// `limit`: `None` for an ISOLATED liquidation — a MARKET order, bounded by the price band alone.
/// `Some(p)` for a CROSS episode's close (cross P3, owner decision Q4): a LIMIT IOC at `p`, the
/// account's MM-share bankruptcy price clamped into the band — Binance's "one large Immediate or
/// Cancel order" whose "limit price" is the bankruptcy price. Both never rest.
pub(crate) fn execute_liquidation_market_order<H: PerpHost>(
    context: &mut H,
    user: Address,
    market: &Market,
    side: Side,
    quantity: u64,
    limit: Option<u64>,
) -> Result<LiquidationCloseOutcome, PerpError> {
    let (order_id, bumped_nonce) = super::peek_order_id(context, user)?;
    // A liquidation close is immediate-or-cancel, bounded by the price band, never resting: a
    // market order, or (cross) a limit IOC. One `OrderKind` says which, and the record/event's two
    // wire fields are derived from it.
    let (kind, price) = match limit {
        None => (OrderKind::Market, 0),
        Some(p) => (OrderKind::Limit(crate::types::TimeInForce::Ioc), p),
    };
    let mut order = Order {
        owner: user.0 .0,
        market_id: market.market_id,
        side,
        price,
        quantity,
        filled: 0,
        order_type: kind.order_type(),
        tif: kind.tif(),
        status: OrderStatus::Open,
        // A liquidation close is a protocol action, not a user order: it never rests (Market/IOC),
        // so no admission predicate applies and the flag would carry no meaning.
        reduce_only: false,
        // Not a conditional order: a liquidation close is a protocol action, never triggered.
        trigger_price: 0,
        trigger_up: false,
        close_position: false,
        // Self-trade prevention never applies to a liquidation close (the liquidated user's own
        // orders in this market are all cancelled before it runs, so it cannot meet one). The
        // field is the default only because the record has to hold something.
        stp: StpMode::default(),
        // Not a conditional order, so bound to no position.
        position_generation: 0,
        trail_bps: 0,
    };
    // commit-only #23: the close order is persisted ONCE after matching (below); the
    // OrderPlaced log keeps its original position (logs are EVM-journaled).
    context.log(Log {
        address: PERP_DEX_ADDRESS,
        data: IPerpDex::OrderPlaced {
            user,
            marketId: market.market_id,
            orderId: FixedBytes(order_id),
            side: side as u8,
            price,
            quantity,
            orderType: kind.order_type() as u8,
            tif: kind.tif() as u8,
            clientOrderId: FixedBytes::default(),
            // A liquidation close is a protocol action, not a user order — no modifiers apply. The
            // byte still goes through the one encoder; its STP bits (EXPIRE_MAKER, `00`) are
            // MEANINGLESS for this order.
            flags: super::encode_order_flags(false, StpMode::default()),
        }
        .to_log_data(),
    });

    let outcome = match_order(
        context,
        user,
        &order_id,
        market.market_id,
        side,
        price,
        quantity,
        kind, // market order or limit IOC: never rests, never all-or-nothing
        market,
        // Liquidation close: waive the taker trading fee (the liquidated user pays
        // the clearance fee to the IF instead). Also prevents the close from
        // reverting when the underwater user cannot cover a taker fee.
        true,
        &mut order,
    )?;
    // ── ⚠️ A LIQUIDATION MUST NEVER BECOME REJECTABLE ──────────────────────────────────────────
    // A liquidation that cannot complete is a LIVENESS failure, not a user error, so the write
    // barrier is hoisted by exactly ZERO statements here: `apply` is the next thing that runs.
    //
    // Structurally, not by convention: this close is `OrderKind::Market` or a limit IOC, and neither
    // has `rests_remainder() == true`, so `match_order` never builds a `rest_basis`, no caller of this
    // function ever calls `rest_in_book`, and neither `RestOutcome` refusal is reachable from this
    // path at all. The set of rejects between the match and the flush is empty, which is the same
    // set it was before the hoist — the liquidation path's reject surface is unchanged.
    let remaining = outcome.remaining;
    // Read BEFORE `apply` consumes the outcome.
    let maker_cap_deferred = outcome.maker_cap_deferred();
    outcome.apply(context, side, market, &mut None)?;
    // delete-on-terminal: the liquidation close is an IOC that never rests — it exists only to
    // drive the match + emit OrderPlaced/Trade. Its record is dropped (never a live/queryable
    // resting order; any residual is settled at mark price by the caller).
    storage::delete_order(context, &order_id)?;
    super::commit_order_nonce(context, user, bumped_nonce)?;

    if remaining == 0 {
        // Full fill: clean up any rounding residuals left in the position.
        let mut pos = storage::load_position(context, user, market.market_id)?;
        if pos.amount != 0 {
            return Err(perp_invariant_err(
                "liquidation market order: full fill but position not zero",
            ));
        }
        // ⚠️ Only when there is really a residual to clear. The common case — the fill zeroed both
        // legs exactly — used to take this write anyway, and the write MARKS the user, so the
        // end-of-call drain published a second `AccountBalanceChanged` for the liquidated user that
        // repeated the close group's payload with zero position rows: a duplicate with no news in
        // it. (The delta is unaffected: the match flush already wrote this position key with these
        // same bytes, which is why the block commitment does not move.)
        //
        // When a residual IS cleared the write stands, the user stays marked, and the drain
        // publishes a legitimate 0-position group whose `totalWalletBalance` differs by the residual.
        // `margin` is never residue any more, and is deliberately NOT written off:
        // `apply_position_fill` releases it SIGNED, so a full close leaves exactly 0 — including a
        // funding DEBT, which the close netted against its PnL. A non-zero margin here would mean
        // that release regressed, and zeroing it would silently forgive a debt (minting money nobody
        // paid) or destroy collateral. Left alone, it stays visible in `Σ pos.margin`; the assert
        // makes the regression loud in every test build.
        debug_assert_eq!(
            pos.margin, 0,
            "liquidation full fill left margin on a flat position — the signed release regressed"
        );
        if pos.v_quote_balance != 0 {
            // A rounding-residue WRITE-OFF, not a close: the PnL of this liquidation's fills was
            // already realised (and accumulated into `cumulative_realized_pnl`) by
            // `settlement::apply_position_fill` on the taker path. So nothing is added to `cr`
            // here — and, just as importantly, nothing RESETS it: `pos` is the loaded position and
            // only this one field is overwritten.
            pos.v_quote_balance = 0;
            // `Adjustment`: a protocol-side WRITE-OFF, not the order's fill. The fills themselves
            // were published by the taker path (`Order`) and that emit cleared the mark; what is left
            // here is rounding residue going nowhere, so the drain's 0-position group for it is a
            // forced balance change with no order behind it. When the clearance fee also fires this
            // merges with its `InsuranceClear` to `Multiple` — see
            // `storage::mark_account_snapshot_dirty`.
            storage::save_position(
                context,
                user,
                market.market_id,
                &pos,
                AccountUpdateReason::Adjustment,
            )?;
        }
    }

    Ok(LiquidationCloseOutcome {
        remaining,
        maker_cap_deferred,
    })
}
