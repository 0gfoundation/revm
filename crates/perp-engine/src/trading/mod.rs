//! Trading engine: place/cancel/query orders with on-chain order-book matching.

use alloy_primitives::IntoLogData;
use alloy_sol_types::SolCall;
use crate::host::PerpHost;
use primitives::{keccak256, Address, Bytes, FixedBytes, Log};

pub mod algo;
mod liquidation_close;
pub mod reduce_only;
mod settlement;

pub(crate) use liquidation_close::execute_liquidation_market_order;
pub(crate) use settlement::{
    apply_position_fill, OpeningMarginFunding, CROSS_MAINTENANCE_REJECT, ISOLATED_SILO_CASH_REJECT,
};
use settlement::{MakerFillOutcome, TakerSettlement};

/// Maximum distinct maker accounts a single liquidation close may touch.
///
/// Any unfilled quantity after reaching the cap follows the existing residual/ADL path. This keeps
/// the balance after-image count bounded independently of order-book size.
pub(crate) const MAX_LIQUIDATION_MAKER_ACCOUNTS: usize = 128;

/// Q1: maximum LIVE resting maker orders one non-liquidation taker walk may touch — filled, STP
/// cancelled or stopped at, or cancelled by the maker K9 / frozen-account guard. Stale lazy-queue ids
/// are not counted (see `match_order`).
///
/// `placeOrder` gas is flat (`PLACE_ORDER_GAS`), so without this one call could walk the whole book
/// — every maker a distinct account to load, settle and flush. At the cap a submitted GTC or FOK is
/// REJECTED write-clean with [`TOUCH_CAP_REJECT`] ("split the order"); an IOC or market order fills
/// up to the cap and its remainder EXPIRES (`OrderExpired`, reason `Tif`); a fired conditional fills
/// up to the cap and stays armed for the rest (`algo::fire`). A liquidation close is exempt — it has
/// its own cap, [`MAX_LIQUIDATION_MAKER_ACCOUNTS`]. Same value as `MAX_BATCH_PLACE` and
/// `MAX_BAND_EXPIRIES_PER_UPDATE`; real flow sits far below it (HL replay: p99 6 live makers).
pub(crate) const MAX_TAKER_MAKER_TOUCHES: u32 = 64;

/// The reject a submitted GTC / FOK raises at [`MAX_TAKER_MAKER_TOUCHES`]. Not in the batch reason
/// table, so a batch item reports it as `Other` (255). The number is pinned to the constant by test.
pub(crate) const TOUCH_CAP_REJECT: &str =
    "placeOrder: order would match more than 64 resting orders; split it into smaller orders";

use crate::{
    account::signing::{
        check_api_key_expiry, check_recv_window, gc_seen_buckets_best_effort, put, verify_ed25519,
        write_signed_header,
    },
        batch::{self, PerpBatchTag},
    errors::{perp_err, perp_invariant_err},
    interface::IPerpDex::{
        self, batchCancelOrdersCall, batchCancelOrdersSignedCall, batchPlaceOrdersCall,
        batchPlaceOrdersSignedCall, cancelOrderCall, cancelOrderSignedCall, getBookLevelCall,
        getBookPricesCall, getMarketFeeTotalCall, getOpenOrdersCall, getOpenOrdersReturn,
        getOrderCall, getOrderReturn, placeOrderCall, placeOrderSignedCall, PlaceItem,
    },
    math::calc_value,
    risk::record_mid_price_sample_for_best_quote_change,
    storage,
    types::{
        CancelReason, ExpiryReason, Order, OrderEntry, OrderKind, OrderStatus, OrderType,
        Side, StpMode, TimeInForce,
    },
    PERP_DEX_ADDRESS,
    PerpError,
};

mod entry;
pub use entry::*;

mod batch_ops;
pub use batch_ops::*;

mod query;
pub use query::*;

mod order;
pub(crate) use order::*;

mod matching;
use matching::*;

mod resting;
pub(crate) use resting::*;

mod cancel;
pub(crate) use cancel::*;

mod fok;
use fok::*;

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests;
