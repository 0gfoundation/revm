//! Pure financial math functions.
//!
//! Prices use each market's configured fixed-point `price_decimals`.
//! Quote amounts use 6-decimal fixed-point (`QUOTE_DECIMALS = 6`).

use crate::{
    error::{perp_err, PerpError},
    types::{MarginTiers, Market, OrderEntry},
};

pub const QUOTE_DECIMALS: u32 = 6;
/// Fixed-point base for funding rates: 1_000_000 = 100%.  Minimum granularity: 0.0001%.
pub const FUNDING_RATE_ONE: i64 = 1_000_000;
pub const MAX_FUNDING_RATE: i64 = 7_500; // +0.75%
pub const MIN_FUNDING_RATE: i64 = -7_500; // -0.75%
pub const CLAMP_UPPER_BOUND: i64 = 500; // +0.05%  (inner clamp for I−P)
pub const CLAMP_LOWER_BOUND: i64 = -500; // -0.05%
/// Trading fee denominator. 1 basis point = 1 / 10_000.
pub const FEE_BPS_DENOMINATOR: u64 = 10_000;

/// Hard ceiling accepted by `setUserFeeRates` for either fee rate: 1_000 bps = 10% of notional.
///
/// Defence in depth, NOT the binding rule. The trading fee is charged out of the margin the fill
/// funds (`fee_from_margin = min(fee, opening_margin)`), so for a max-leverage open to survive the
/// K9 maintenance check the rate must satisfy `f ≤ 1/(2·L_max)` — the tier's maintenance rate,
/// `1/6 ≈ 1_666 bps` at the default `L_max = 3`. K9 enforces exactly that at FILL time, per market
/// and per position size (the tier table can lower `L_max`, raising the real bound). This static
/// cap only removes the absurd end of the range: the previous bound was `FEE_BPS_DENOMINATOR`,
/// i.e. a 100%-of-notional fee.
pub const MAX_USER_FEE_BPS: u64 = 1_000;

/// Default price-band half-width (basis points) used when a market's
/// `price_band_bps` is left at `0`. 1_000 bps = ±10%.
pub const DEFAULT_PRICE_BAND_BPS: u32 = 1_000;

/// Resolve a market's stored `price_band_bps` to the effective value:
/// `0` maps to [`DEFAULT_PRICE_BAND_BPS`], any other value is used verbatim
/// (a large value such as `>= 10_000` effectively disables the band).
#[inline]
pub fn effective_price_band_bps(price_band_bps: u32) -> u32 {
    if price_band_bps == 0 {
        DEFAULT_PRICE_BAND_BPS
    } else {
        price_band_bps
    }
}

/// Inclusive price-band bounds `(upper, lower)` around `mark`, in the u128 price space.
/// A price `P` is IN band iff `lower <= P <= upper`. The band is enforced at FILL time
/// (in the matching loop), not at placement: a resting order may sit anywhere in the
/// book, but a taker/liquidation fill never executes farther than ±band from the CURRENT
/// mark — which is immune to post-placement mark drift and lets harmless deep passive
/// orders rest. `mark == 0` (unset) returns `(u128::MAX, 0)` = no band. `bps >= 10_000`
/// widens the lower bound to `0` (matching [`effective_price_band_bps`] "disabled").
#[inline]
pub fn mark_band_bounds(mark: u64, price_band_bps: u32) -> (u128, u128) {
    if mark == 0 {
        return (u128::MAX, 0);
    }
    let bps = effective_price_band_bps(price_band_bps) as u128;
    let m = mark as u128;
    let upper = m.saturating_mul(10_000 + bps) / 10_000;
    let lower = if bps >= 10_000 {
        0
    } else {
        m * (10_000 - bps) / 10_000
    };
    (upper, lower)
}

#[inline]
fn pow10_u128(exp: u32) -> Result<u128, PerpError> {
    10u128
        .checked_pow(exp)
        .ok_or_else(|| perp_err("math: decimal exponent overflow"))
}

#[inline]
fn pow10_i128(exp: u32) -> Result<i128, PerpError> {
    10i128
        .checked_pow(exp)
        .ok_or_else(|| perp_err("math: decimal exponent overflow"))
}

#[inline]
pub fn checked_u64_to_i64(value: u64, context: &str) -> Result<i64, PerpError> {
    i64::try_from(value).map_err(|_| perp_err(format!("{context}: value exceeds i64::MAX")))
}

/// `price * quantity * 10^QUOTE_DECIMALS / (10^price_decimals * 10^base_decimals)` in quote units.
#[inline]
pub fn calc_value(
    price: u64,
    quantity: u64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<u64, PerpError> {
    let quote_scale = pow10_u128(QUOTE_DECIMALS)?;
    let numerator = (price as u128)
        .checked_mul(quantity as u128)
        .and_then(|v| v.checked_mul(quote_scale))
        .ok_or_else(|| perp_err("math: value numerator overflow"))?;
    let denominator = pow10_u128(price_decimals)?
        .checked_mul(pow10_u128(base_decimals)?)
        .ok_or_else(|| perp_err("math: value denominator overflow"))?;
    let value = numerator / denominator;
    u64::try_from(value).map_err(|_| perp_err("math: value exceeds u64"))
}

/// Trading fee in quote units, rounded down.
#[inline]
pub fn calc_trading_fee(notional: u64, fee_bps: u64) -> Result<u64, PerpError> {
    let fee = (notional as u128)
        .checked_mul(fee_bps as u128)
        .ok_or_else(|| perp_err("math: trading fee overflow"))?
        / FEE_BPS_DENOMINATOR as u128;
    u64::try_from(fee).map_err(|_| perp_err("math: trading fee exceeds u64"))
}

/// Maker fee (quote units) for `qty` of an order resting at `price`, using a
/// pre-snapshotted `maker_fee_bps`. Single source of truth shared by the
/// placement, fill-release, and cancel-release paths so the reserve↔release
/// fee math cannot drift between them.
#[inline]
pub fn calc_maker_fee_for_order_qty_with_bps(
    price: u64,
    qty: u64,
    maker_fee_bps: u64,
    market: &Market,
) -> Result<u64, PerpError> {
    let notional = calc_value(price, qty, market.base_decimals, market.price_decimals)?;
    calc_trading_fee(notional, maker_fee_bps)
}

/// Signed version of `calc_value` for negative quantities.
#[inline]
pub fn calc_value_i64(
    price: u64,
    quantity: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<i64, PerpError> {
    let quote_scale = pow10_i128(QUOTE_DECIMALS)?;
    let numerator = (price as i128)
        .checked_mul(quantity as i128)
        .and_then(|v| v.checked_mul(quote_scale))
        .ok_or_else(|| perp_err("math: signed value numerator overflow"))?;
    let denominator = pow10_i128(price_decimals)?
        .checked_mul(pow10_i128(base_decimals)?)
        .ok_or_else(|| perp_err("math: signed value denominator overflow"))?;
    let value = numerator / denominator;
    i64::try_from(value).map_err(|_| perp_err("math: signed value exceeds i64"))
}

/// Maintenance margin required for a position of `abs_notional` quote units under a
/// market's margin-tier table.
///
/// Spec (`perpdex-docs trading/margin-tiers.md`):
/// `maintenance_margin = notional * mmr(n) - deduction(n)`, `mmr(n) = 1 / (2*L_n)`,
/// `deduction(n) = deduction(n-1) + B_n * (mmr(n) - mmr(n-1))`.
///
/// # Why this is the SLICE form and not the spec's subtractive recursion
///
/// **Do not "correct" this back to `notional*mmr(n) - deduction(n)`.** Over the reals the
/// two are identical; over integers they are not. Evaluating the deduction term as one
/// floored rational `⌊B·(mmr_n − mmr_{n−1})⌋` is DISCONTINUOUS at 53.1% of two-tier
/// boundaries (exhaustive scan, L₀∈2..8, L₁<L₀, B∈1..400) — e.g. `[{0,3},{50e9,2}]` yields
/// 8_333_333_334 at the boundary where the previous tier's own rule yields 8_333_333_333.
/// A +1 step in the *liquidate-a-healthy-position* direction: precisely the discontinuity
/// the deduction exists to remove, and fully deterministic, so every node would agree on
/// the wrong number and it would never surface as a consensus fault.
///
/// The slice (marginal) form below charges each fully-crossed band `(B_{k+1} − B_k)/(2·L_k)`
/// with ONE floor per slice plus one on the partial slice. It is algebraically the same sum
/// over the reals, is 0% discontinuous at boundaries (the terms are shared verbatim by both
/// sides of a boundary — see `tier_boundaries_are_continuous`), and is monotone
/// non-decreasing in notional.
///
/// `abs_notional` must already be non-negative (callers pass `notional.checked_abs()?`);
/// a negative input is clamped to 0 rather than producing a negative requirement.
/// A tier's `max_leverage` is floored at 1 so a corrupt blob cannot divide by zero.
#[inline]
pub fn maintenance_margin(tiers: &MarginTiers, abs_notional: i64) -> Result<i64, PerpError> {
    let n = (abs_notional as i128).max(0);
    let table = tiers.as_slice();
    let mut acc: i128 = 0;
    let mut lo: i128 = 0;
    let mut lev: i128 = (table[0].max_leverage as i128).max(1);
    for tier in &table[1..] {
        let bound = tier.lower_bound_notional as i128;
        if n < bound {
            break;
        }
        acc += (bound - lo) / (2 * lev);
        lo = bound;
        lev = (tier.max_leverage as i128).max(1);
    }
    acc += (n - lo) / (2 * lev);
    i64::try_from(acc).map_err(|_| perp_err("math: maintenance margin exceeds i64"))
}

/// Maximum leverage a market's tier table permits at `abs_notional` quote units: the
/// `max_leverage` of the last tier whose `lower_bound_notional` the notional reaches.
/// At notional 0 this is tier 0's `max_leverage` — the market's `setLeverage` cap.
#[inline]
pub fn max_leverage_for_notional(tiers: &MarginTiers, abs_notional: i64) -> u32 {
    let n = (abs_notional as i128).max(0);
    let table = tiers.as_slice();
    let mut lev = table[0].max_leverage;
    for tier in &table[1..] {
        if n < tier.lower_bound_notional as i128 {
            break;
        }
        lev = tier.max_leverage;
    }
    lev
}

// ── Derived open-order initial margin (ooIM) ─────────────────────────────────────────────────
//
// The requirement a set of RESTING orders imposes, DERIVED from the position and the book
// instead of escrowed at placement. This is the quantity the derived-ooIM migration replaces
// `PerpPosition::margin_reserved` with; Phase 1 only computes it alongside the escrow.
//
//     PIM  = ROUND_UP( |N| / L )                        position initial margin
//     IM   = ROUND_UP( max(|N + Bid|, |N − Ask|) / L )  joint requirement
//     ooIM = IM − PIM
//
// with `N` the SIGNED position notional at MARK, `Bid` = Σ (remaining qty × LIMIT price) over
// resting buys, `Ask` the same over sells, and `L` the position leverage. The two branches are
// the exposure left if every buy fills and if every sell fills.
//
// Formula source: `misc/binance-margin-verified-model.md` §1.1 (formula set) and §2 (rounding),
// plus `misc/binance-v3-account-balance-field-reference.md` §2/§4. Every rounding decision below
// is the one those documents settled against mainnet samples; see the doc comments.

/// `ROUND_UP(numerator / divisor)` — the rounding Binance uses for `initialMargin` and
/// `positionInitialMargin`.
///
/// Settled as ROUND_UP (not `HALF_UP`, not `trunc`, not `floor`) on 14/14 discriminating mainnet
/// samples (`binance-margin-verified-model.md` §2). Note this is the OPPOSITE direction from
/// [`maintenance_margin`], which truncates — the two are different requirements with
/// independently measured rounding, and neither should be "made consistent" with the other.
///
/// `divisor` is floored at 1, so a defaulted or corrupt `pos.leverage` of 0 cannot divide by
/// zero.
#[inline]
pub fn round_up_div(numerator: u128, divisor: u64) -> u128 {
    numerator.div_ceil(divisor.max(1) as u128)
}

/// The three derived margin quantities for one `(position, resting orders)` pair, plus the
/// leverage they were evaluated at. Output of [`open_order_margin`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenOrderMargin {
    /// `ROUND_UP(|N| / L)` — what the POSITION alone requires.
    pub position_initial_margin: u64,
    /// `ROUND_UP(max(|N + Bid|, |N − Ask|) / L)` — position and resting orders JOINTLY.
    pub initial_margin: u64,
    /// `IM − PIM` — the marginal requirement the resting orders add. Zero when there are none.
    pub open_order_initial_margin: u64,
    /// The leverage actually used (input floored at 1, and tier-capped by
    /// [`open_order_margin`]).
    pub effective_leverage: u64,
}

/// **The** open-order margin formula. There is exactly one; every ooIM in the engine comes from
/// here, at the POSITION'S OWN leverage, UNCAPPED by the tier table.
///
/// ```text
/// PIM  = ROUND_UP(|N| / L)
/// IM   = ROUND_UP( max(|N + Bid|, |N − Ask|) / L )
/// ooIM = IM − PIM
/// ```
///
/// # Why no tier cap
///
/// A tier-capped variant briefly existed (leverage clamped to
/// `max_leverage_for_notional(tiers, max(|N + Bid|, |N − Ask|))`, i.e. priced at the notional the
/// position would carry if the whole book filled) so the ADMISSION gate could not be dodged by
/// setting leverage while small and then resting orders past a tier boundary. It was rejected:
/// Binance prices open orders at the position's leverage, full stop, and the capped form is a
/// second, silently different ooIM whose discontinuity at a boundary re-prices the PRE-EXISTING
/// position too (a 10× jump from one extra lot — measured). Tiers still govern MAINTENANCE margin,
/// which is continuous, and `set_leverage` still caps against tier 0; an over-levered position is
/// refused when it next tries to OPEN (`max_leverage_for_notional` in the fill guards). Do not
/// reintroduce a second definition here.
///
/// Pure integer arithmetic over `(signed_notional, bid, ask, leverage)`; no storage, no floats.
/// `signed_notional` is `N` at MARK (negative for a short); `bid`/`ask` are at each order's LIMIT
/// price. Mixing the two bases is deliberate — it is Binance's formula.
///
/// # Rounding
///
/// `ooIM` is the DIFFERENCE OF TWO ROUND_UPs, never a single round-up of a difference: the
/// convenience form `ROUND_UP(max(0, Bid, Ask − 2N) / L)` is NOT equivalent at 1 ulp because
/// `ceil(a) − ceil(b) != ceil(a − b)` (`binance-margin-verified-model.md` §1.1).
///
/// # Overflow
///
/// `N ± Bid|Ask` is evaluated in `i128` and the branches in `u128`, so no intermediate can wrap
/// (`|N| < 2^63`, `Bid, Ask < 2^64` ⟹ each branch `< 2^65`). Only the final narrowing to `u64`
/// can fail, and it does so as a clean error rather than silently.
///
/// # Why `IM >= PIM` (so the subtraction cannot wrap)
///
/// Whichever branch shares `N`'s sign already dominates `|N|` for any `Bid, Ask >= 0`: for
/// `N >= 0`, `|N + Bid| >= N`; for `N < 0`, `|N − Ask| >= |N|`. The `saturating_sub` below is
/// belt-and-braces on that invariant, not a rounding decision.
pub fn open_order_margin(
    signed_notional: i64,
    bid: u64,
    ask: u64,
    leverage: u64,
) -> Result<OpenOrderMargin, PerpError> {
    let lev = leverage.max(1);
    let n = signed_notional as i128;

    let bid_branch = n
        .checked_add(bid as i128)
        .ok_or_else(|| perp_err("math: ooIM bid branch overflow"))?
        .unsigned_abs();
    let ask_branch = n
        .checked_sub(ask as i128)
        .ok_or_else(|| perp_err("math: ooIM ask branch overflow"))?
        .unsigned_abs();

    let position_initial_margin =
        u64::try_from(round_up_div(signed_notional.unsigned_abs() as u128, lev))
            .map_err(|_| perp_err("math: position initial margin exceeds u64"))?;
    let initial_margin = u64::try_from(round_up_div(bid_branch.max(ask_branch), lev))
        .map_err(|_| perp_err("math: initial margin exceeds u64"))?;

    Ok(OpenOrderMargin {
        position_initial_margin,
        initial_margin,
        open_order_initial_margin: initial_margin.saturating_sub(position_initial_margin),
        effective_leverage: lev,
    })
}

/// Returns `true` if the position is above the maintenance-margin threshold.
#[inline]
pub fn is_above_maintenance_margin(
    tiers: &MarginTiers,
    mark_price: u64,
    amount: i64,
    v_quote_balance: i64,
    margin: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<bool, PerpError> {
    let notional = calc_value_i64(mark_price, amount, base_decimals, price_decimals)?;
    let position_value = notional
        .checked_add(v_quote_balance)
        .and_then(|v| v.checked_add(margin))
        .ok_or_else(|| perp_err("math: maintenance margin value overflow"))?;
    let threshold = maintenance_margin(
        tiers,
        notional
            .checked_abs()
            .ok_or_else(|| perp_err("math: maintenance margin abs overflow"))?,
    )?;
    Ok(position_value >= threshold)
}

/// Position equity at `mark_price`: isolated margin plus unrealized PnL.
#[inline]
pub fn calc_position_equity(
    mark_price: u64,
    amount: i64,
    v_quote_balance: i64,
    margin: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<i64, PerpError> {
    calc_value_i64(mark_price, amount, base_decimals, price_decimals)?
        .checked_add(v_quote_balance)
        .and_then(|v| v.checked_add(margin))
        .ok_or_else(|| perp_err("math: position equity overflow"))
}

/// Proportionally scale `initial_margin` down by the unfilled portion.
#[inline]
pub fn calc_remaining_margin(
    total_quantity: u64,
    incoming_quantity: u64,
    initial_margin: i64,
) -> Result<i64, PerpError> {
    if incoming_quantity >= total_quantity {
        return Ok(0);
    }
    let remaining = (total_quantity - incoming_quantity) as i128;
    let scaled = remaining
        .checked_mul(initial_margin as i128)
        .ok_or_else(|| perp_err("math: remaining margin overflow"))?
        / total_quantity as i128;
    i64::try_from(scaled).map_err(|_| perp_err("math: remaining margin exceeds i64"))
}

/// Entry price derived from position state. Returns 0 if `amount == 0`.
#[inline]
pub fn calc_entry_price(
    amount: i64,
    v_quote_balance: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<u64, PerpError> {
    if amount == 0 {
        return Ok(0);
    }
    let numerator = (-(v_quote_balance as i128))
        .checked_mul(pow10_i128(price_decimals)?)
        .and_then(|v| v.checked_mul(pow10_i128(base_decimals).ok()?))
        .ok_or_else(|| perp_err("math: entry price numerator overflow"))?;
    let denominator = (amount as i128)
        .checked_mul(pow10_i128(QUOTE_DECIMALS)?)
        .ok_or_else(|| perp_err("math: entry price denominator overflow"))?;
    let price = numerator / denominator;
    u64::try_from(price).map_err(|_| perp_err("math: entry price exceeds u64"))
}

/// Bankruptcy price: the price at which the position's equity is exactly zero
/// (`calc_value_i64(P_b, amount) + v_quote_balance + margin == 0`). This is the price at
/// which the position is EXACTLY bankrupt, i.e. the liquidation price WITHOUT the
/// maintenance-margin adjustment (equity == 0, not equity == maintenance). Returns 0 if
/// `amount == 0`.
///
/// Used by ADL to close a liquidated residual against opposite-side holders as a
/// forced trade at `P_b`. Rounding is chosen so the LIQUIDATED position's equity at
/// `P_b` is `>= 0` (a long rounds the price UP, a short rounds it DOWN): closing the
/// residual at `P_b` then never realizes a loss beyond the position's own margin, so
/// it never produces bad debt — ADL scheme X routes NO bad debt to the Insurance
/// Fund. A tiny (<= 1 sub-unit) equity surplus is a conserving credit from the ADL
/// counterparty, never a mint.
#[inline]
pub fn calc_bankruptcy_price(
    amount: i64,
    v_quote_balance: i64,
    margin: i64,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<u64, PerpError> {
    if amount == 0 {
        return Ok(0);
    }
    let numerator = (-(v_quote_balance as i128 + margin as i128))
        .checked_mul(pow10_i128(price_decimals)?)
        .and_then(|v| v.checked_mul(pow10_i128(base_decimals).ok()?))
        .ok_or_else(|| perp_err("math: bankruptcy price numerator overflow"))?;
    let denominator = (amount as i128)
        .checked_mul(pow10_i128(QUOTE_DECIMALS)?)
        .ok_or_else(|| perp_err("math: bankruptcy price denominator overflow"))?;
    // Integer division truncates toward zero. For a long (num>0, den>0) that floors,
    // so round UP to keep equity(P_b) >= 0; a short (num<0, den<0) yields a floored
    // positive, which is already the DOWN rounding we want.
    let mut price = numerator / denominator;
    if amount > 0 && numerator % denominator != 0 {
        price += 1;
    }
    // `calc_value_i64` floors internally, so nudge once more if that flooring pushed
    // the liquidated equity a sub-unit negative (long: price up, short: price down).
    // Bounded, deterministic.
    for _ in 0..2 {
        let p = u64::try_from(price).map_err(|_| perp_err("math: bankruptcy price exceeds u64"))?;
        let eq = calc_value_i64(p, amount, base_decimals, price_decimals)?
            .checked_add(v_quote_balance)
            .and_then(|v| v.checked_add(margin))
            .ok_or_else(|| perp_err("math: bankruptcy equity overflow"))?;
        if eq >= 0 {
            return Ok(p);
        }
        price += if amount > 0 { 1 } else { -1 };
    }
    u64::try_from(price).map_err(|_| perp_err("math: bankruptcy price exceeds u64"))
}

/// Compute the funding rate using Binance's formula:
///   F = P + clamp(interest_rate − P, CLAMP_LOWER_BOUND, CLAMP_UPPER_BOUND)
///   F_final = clamp(F, MIN_FUNDING_RATE, MAX_FUNDING_RATE)
#[inline]
pub fn calc_funding_rate(average_premium_index: i64, interest_rate: i64) -> i64 {
    let clamped =
        (interest_rate - average_premium_index).clamp(CLAMP_LOWER_BOUND, CLAMP_UPPER_BOUND);
    (average_premium_index + clamped).clamp(MIN_FUNDING_RATE, MAX_FUNDING_RATE)
}

/// Funding payment (signed, quote units) accrued by a position between two
/// cumulative-funding-index checkpoints.
///
/// `index_delta = cumulative_funding_index_now - position.last_funding_index`,
/// where the per-market index accumulates `mark_price * funding_rate` at each
/// epoch boundary. The result is what should be ADDED to the wallet: positive =
/// credit (the position receives funding), negative = charge (it pays).
///
/// ```text
/// payment = -(amount * index_delta * 10^QUOTE_DECIMALS)
///           / (10^price_decimals * 10^base_decimals * FUNDING_RATE_ONE)
/// ```
///
/// A long (`amount > 0`) pays when the rate is positive (longs pay shorts); the
/// scaling matches `calc_value` so the payment is in 6-decimal quote units like
/// every other balance in the engine.
#[inline]
pub fn calc_funding_payment(
    amount: i64,
    index_delta: i128,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<i64, PerpError> {
    let numerator = (amount as i128)
        .checked_mul(index_delta)
        .and_then(|v| v.checked_mul(pow10_i128(QUOTE_DECIMALS).ok()?))
        .ok_or_else(|| perp_err("math: funding payment numerator overflow"))?;
    let denominator = pow10_i128(price_decimals)?
        .checked_mul(pow10_i128(base_decimals)?)
        .and_then(|v| v.checked_mul(FUNDING_RATE_ONE as i128))
        .ok_or_else(|| perp_err("math: funding payment denominator overflow"))?;
    let payment = -(numerator / denominator);
    i64::try_from(payment).map_err(|_| perp_err("math: funding payment exceeds i64"))
}

// ── Per-side resting-order aggregates (Binance `Bid` / `Ask`) ───────────────────────────────
//
// The flip-aware escrow reservation these primitives used to feed is GONE (derived-ooIM Phase 2):
// the open-order requirement is now derived on demand from `(N, Bid, Ask, L)` by
// [`open_order_margin`], so the only thing still needed off the order lists is each side's
// `(Σ qty, Σ notional)` — which IS `(·, Bid)` / `(·, Ask)`. The cover-prefix leg reconstruction
// (`side_leg_from_total`), the four-leg `max(S + B', B + S')` fold and their per-side helpers were
// deleted with the escrow; nothing computes an opening notional per side any more.

/// `(Σ amount, Σ calc_value(price, amount))` over a side's order list — the maintained aggregate
/// the leg reconstruction below consumes. Cold-rebuild / test helper; production maintains these
/// incrementally so this full pass runs only on a cold first-touch.
#[inline]
pub fn sum_side_totals(
    entries: impl Iterator<Item = OrderEntry>,
    base_decimals: u32,
    price_decimals: u32,
) -> Result<(u64, u64), PerpError> {
    let mut qty = 0u64;
    let mut notional = 0u64;
    for e in entries {
        qty = qty
            .checked_add(e.amount)
            .ok_or_else(|| perp_err("math: side total qty overflow"))?;
        let v = calc_value(e.price, e.amount, base_decimals, price_decimals)?;
        notional = notional
            .checked_add(v)
            .ok_or_else(|| perp_err("math: side total notional overflow"))?;
    }
    Ok((qty, notional))
}

#[cfg(test)]
mod bankruptcy_price_tests {
    use super::*;

    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// For realistic longs AND shorts across many decimals/leverages, the bankruptcy
    /// price must satisfy `0 <= equity(P_b) <= dust` — i.e. closing the residual at
    /// `P_b` NEVER leaves the position insolvent (no bad debt, ADL scheme X routes none
    /// to the IF), and `P_b` is tight (equity is a sub-unit above zero, a conserving
    /// crumb, never a mint).
    #[test]
    fn bankruptcy_price_keeps_liquidated_equity_nonnegative_and_tight() {
        let mut s: u64 = 0xc0ffee_1234_5678;
        for _ in 0..20000 {
            let bd = (next(&mut s) % 5) as u32; // 0..=4
            let pd = (next(&mut s) % 5) as u32; // 0..=4
            let entry = (next(&mut s) % 10_000_000 + 1) as u64; // <= maxPrice (1e8)
            let qty = (next(&mut s) % 100_000 + 1) as u64;
            let lev = (next(&mut s) % 6 + 1) as i64; // 1..=6
            let notional = match calc_value(entry, qty, bd, pd) {
                Ok(v) if v > 0 && v <= i64::MAX as u64 => v as i64,
                _ => continue, // beyond realistic on-chain bounds (maxPrice/maxQuantity)
            };
            let margin = notional / lev;
            let is_long = next(&mut s) & 1 == 0;
            // Long: paid the notional (vq negative); short: received it (vq positive).
            let (amount, v_quote) = if is_long {
                (qty as i64, -notional)
            } else {
                (-(qty as i64), notional)
            };

            let p_b = calc_bankruptcy_price(amount, v_quote, margin, bd, pd).unwrap();
            // P_b == 0 is valid for a <=1x position (bankrupt only at price 0 => never
            // insolvent => never ADL'd). Only the equity invariant must hold.
            let eq = match calc_position_equity(p_b, amount, v_quote, margin, bd, pd) {
                Ok(e) => e,
                Err(_) => continue, // P_b*amount beyond i64 (unrealistic)
            };
            assert!(eq >= 0, "equity(P_b) < 0 => bad debt: eq={eq} P_b={p_b} amount={amount} vq={v_quote} m={margin} bd={bd} pd={pd}");
            // Tightness: equity at P_b is within one price sub-unit's worth of value.
            let one_tick = calc_value(1, qty, bd, pd).unwrap() as i64 + 2;
            assert!(eq <= one_tick, "equity(P_b) not tight: eq={eq} bound={one_tick} P_b={p_b}");
        }
    }

    #[test]
    fn bankruptcy_price_zero_amount_is_zero() {
        assert_eq!(calc_bankruptcy_price(0, -100, 10, 2, 2).unwrap(), 0);
    }
}

#[cfg(test)]
mod funding_payment_tests {
    use super::*;

    // amount=10 (base_decimals=0), mark=$100 (10000 @ price_decimals=2),
    // one epoch at rate 7500 (0.75%): index_delta = mark*rate = 75_000_000.
    // notional = $1000 = 1e9 quote units; funding = 1e9 * 0.0075 = 7_500_000.
    const DELTA_ONE_EPOCH: i128 = 10_000 * 7_500; // 75_000_000

    #[test]
    fn long_pays_funding_when_rate_positive() {
        // Long (amount > 0) pays → negative (debit).
        assert_eq!(
            calc_funding_payment(10, DELTA_ONE_EPOCH, 0, 2).unwrap(),
            -7_500_000
        );
    }

    #[test]
    fn short_receives_funding_when_rate_positive() {
        // Short (amount < 0) receives → positive (credit).
        assert_eq!(
            calc_funding_payment(-10, DELTA_ONE_EPOCH, 0, 2).unwrap(),
            7_500_000
        );
    }

    #[test]
    fn zero_position_pays_nothing() {
        assert_eq!(calc_funding_payment(0, DELTA_ONE_EPOCH, 0, 2).unwrap(), 0);
    }

    #[test]
    fn accrual_scales_linearly_across_epochs() {
        // Two epochs of accrued index → twice the payment.
        assert_eq!(
            calc_funding_payment(10, DELTA_ONE_EPOCH * 2, 0, 2).unwrap(),
            -15_000_000
        );
    }

    #[test]
    fn long_receives_when_rate_negative() {
        // Negative funding rate flips the direction: long receives.
        assert_eq!(
            calc_funding_payment(10, -DELTA_ONE_EPOCH, 0, 2).unwrap(),
            7_500_000
        );
    }
}

#[cfg(test)]
mod mark_band_bounds_tests {
    use super::{mark_band_bounds, DEFAULT_PRICE_BAND_BPS};

    #[test]
    fn unset_mark_disables_band() {
        // mark == 0: cannot evaluate a band -> (MAX, 0) = no bound either way.
        assert_eq!(mark_band_bounds(0, 1_000), (u128::MAX, 0));
        assert_eq!(mark_band_bounds(0, 0), (u128::MAX, 0));
    }

    #[test]
    fn zero_bps_uses_default_ten_percent() {
        assert_eq!(DEFAULT_PRICE_BAND_BPS, 1_000);
        // mark 100 -> [90, 110].
        assert_eq!(mark_band_bounds(100, 0), (110, 90));
    }

    #[test]
    fn configured_bps_is_used_verbatim() {
        // 500 bps = +-5%. mark 100 -> [95, 105].
        assert_eq!(mark_band_bounds(100, 500), (105, 95));
    }

    #[test]
    fn large_bps_widens_lower_bound_to_zero() {
        // bps >= 10_000: lower clamps to 0; upper still grows with bps.
        assert_eq!(mark_band_bounds(100, 10_000), (200, 0));
        let (upper, lower) = mark_band_bounds(100, 1_000_000);
        assert_eq!(lower, 0);
        assert_eq!(upper, 100 * (10_000 + 1_000_000) / 10_000);
    }

    #[test]
    fn no_overflow_at_extremes() {
        // huge mark * huge bps must saturate, not panic.
        let (upper, lower) = mark_band_bounds(u64::MAX, u32::MAX);
        assert_eq!(lower, 0);
        assert!(upper > 0);
    }
}

// ── Margin tiers ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod margin_tier_tests {
    use super::*;
    use crate::types::{MarginTier, MarginTiers};

    fn table(rows: &[(u64, u32)]) -> MarginTiers {
        let v: Vec<MarginTier> = rows
            .iter()
            .map(|&(lower_bound_notional, max_leverage)| MarginTier {
                lower_bound_notional,
                max_leverage,
            })
            .collect();
        MarginTiers::from_tiers(&v).expect("valid tier count")
    }

    /// Deterministic xorshift (no rng dependency in this crate).
    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// v1 BEHAVIOUR-PRESERVATION PROOF: the default single tier `{0, 3}` has
    /// `mmr = 1/(2*3) = 1/6`, i.e. exactly the deleted `MAINTENANCE_MARGIN_DENOMINATOR`.
    /// Every reachable notional must reproduce the old `notional / 6` byte for byte.
    #[test]
    fn single_default_tier_is_notional_over_six() {
        let t = MarginTiers::default();
        assert_eq!(t.as_slice(), table(&[(0, 3)]).as_slice());

        assert_eq!(maintenance_margin(&t, 0).unwrap(), 0);
        for n in 0..5_000i64 {
            assert_eq!(maintenance_margin(&t, n).unwrap(), n / 6, "n={n}");
        }
        // Dense sweep across the magnitude range, plus the i64 extremes.
        let mut s: u64 = 0x51ed_270b_d772_1e6f;
        for _ in 0..20_000 {
            let n = (next(&mut s) % (i64::MAX as u64 / 2)) as i64;
            assert_eq!(maintenance_margin(&t, n).unwrap(), n / 6, "n={n}");
        }
        for n in [
            i64::MAX,
            i64::MAX - 1,
            i64::MAX - 2,
            i64::MAX - 3,
            i64::MAX - 4,
            i64::MAX - 5,
            i64::MAX / 6,
            1_000_000_000_000_000_000,
        ] {
            assert_eq!(maintenance_margin(&t, n).unwrap(), n / 6, "n={n}");
        }
    }

    /// THE LOAD-BEARING TEST. At a tier boundary the two adjacent tiers' rules must agree
    /// EXACTLY: the requirement at `B_n` computed from the full table must equal the
    /// requirement at `B_n` computed from the table truncated just before tier `n` (i.e.
    /// still under the previous tier's rule). Any +1 step here liquidates a healthy
    /// position on a negligible size increase — deterministically, on every node.
    ///
    /// This test FAILS on the spec's literal subtractive recursion (a single floored
    /// `⌊B*(mmr_n - mmr_{n-1})⌋` deduction) and PASSES on the slice form. That is its
    /// entire purpose — see [`maintenance_margin`]'s doc comment.
    #[test]
    fn tier_boundaries_are_continuous() {
        // Exhaustive small 2-tier grid.
        for l0 in 2u32..=8 {
            for l1 in 1u32..l0 {
                for b in 1u64..=400 {
                    let full = table(&[(0, l0), (b, l1)]);
                    let truncated = table(&[(0, l0)]);
                    assert_eq!(
                        maintenance_margin(&full, b as i64).unwrap(),
                        maintenance_margin(&truncated, b as i64).unwrap(),
                        "discontinuity at 2-tier boundary l0={l0} l1={l1} b={b}"
                    );
                }
            }
        }

        // Randomised 3..=8-tier tables, checked at EVERY boundary.
        let mut s: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..4_000 {
            let n_tiers = 3 + (next(&mut s) % 6) as usize; // 3..=8
            let mut rows: Vec<(u64, u32)> = Vec::with_capacity(n_tiers);
            let mut bound = 0u64;
            let mut lev = 1 + (next(&mut s) % 100) as u32;
            rows.push((0, lev));
            for _ in 1..n_tiers {
                // Strictly increasing bounds spanning several magnitudes.
                bound += 1 + next(&mut s) % 10_000_000_000;
                // Non-increasing leverage.
                lev = 1 + next(&mut s) as u32 % lev;
                rows.push((bound, lev));
            }
            let full = table(&rows);
            for k in 1..rows.len() {
                let b = rows[k].0 as i64;
                let truncated = table(&rows[..k]);
                assert_eq!(
                    maintenance_margin(&full, b).unwrap(),
                    maintenance_margin(&truncated, b).unwrap(),
                    "discontinuity at boundary {k} of {rows:?}"
                );
            }
        }
    }

    /// The exact counter-example the slice form exists for: at the boundary of
    /// `[{0,3},{50e9,2}]` the previous tier's rule gives 8_333_333_333 (= 50e9/6) while the
    /// spec's literal recursion gives 8_333_333_334 — a +1 step in the
    /// liquidate-a-healthy-position direction.
    #[test]
    fn documented_counter_example_stays_on_the_slice_value() {
        let b: i64 = 50_000_000_000;
        let full = table(&[(0, 3), (b as u64, 2)]);
        assert_eq!(maintenance_margin(&full, b).unwrap(), 8_333_333_333);
        assert_eq!(maintenance_margin(&table(&[(0, 3)]), b).unwrap(), 8_333_333_333);

        // What the spec's literal recursion produces, spelled out so the divergence is
        // visible in the source and nobody "fixes" the implementation back to it:
        //   notional*mmr(1) - deduction(1),  deduction(1) = floor(B * (mmr(1) - mmr(0)))
        //                                                 = floor(B * (1/4 - 1/6)) = floor(B/12)
        let literal = b as i128 / 4 - b as i128 / 12;
        assert_eq!(
            literal, 8_333_333_334,
            "the literal recursion is the +1 the slice form exists to avoid"
        );
    }

    /// Maintenance margin must never decrease as the position grows — otherwise a trader
    /// could shed a requirement by adding size.
    #[test]
    fn maintenance_margin_is_monotone_in_notional() {
        let tables = [
            table(&[(0, 3)]),
            table(&[(0, 3), (1_000, 2), (5_000, 1)]),
            table(&[(0, 100), (7, 50), (13, 9), (101, 3), (1_000_003, 1)]),
            table(&[
                (0, 20),
                (1_000_000, 10),
                (5_000_000, 5),
                (25_000_000, 4),
                (100_000_000, 3),
                (500_000_000, 2),
                (2_500_000_000, 1),
                (10_000_000_000, 1),
            ]),
        ];
        for t in &tables {
            let mut prev = 0i64;
            for n in 0..30_000i64 {
                let mm = maintenance_margin(t, n).unwrap();
                assert!(mm >= prev, "mm dropped at n={n} in {t:?}");
                prev = mm;
            }
            // And across the boundaries at scale.
            let mut prev = maintenance_margin(t, 0).unwrap();
            let mut n = 0i64;
            while n < 12_000_000_000 {
                n += 999_331;
                let mm = maintenance_margin(t, n).unwrap();
                assert!(mm >= prev, "mm dropped at n={n} in {t:?}");
                prev = mm;
            }
        }
    }

    #[test]
    fn maintenance_margin_clamps_negative_input_and_survives_a_zero_leverage_blob() {
        let t = table(&[(0, 3)]);
        assert_eq!(maintenance_margin(&t, -1).unwrap(), 0);
        // A corrupt max_leverage of 0 must not divide by zero (floored to 1 → mmr 1/2).
        let corrupt = table(&[(0, 0)]);
        assert_eq!(maintenance_margin(&corrupt, 100).unwrap(), 50);
    }

    #[test]
    fn max_leverage_for_notional_picks_the_reached_tier() {
        let t = table(&[(0, 5), (1_000, 3), (10_000, 1)]);
        assert_eq!(max_leverage_for_notional(&t, 0), 5);
        assert_eq!(max_leverage_for_notional(&t, 999), 5);
        assert_eq!(max_leverage_for_notional(&t, 1_000), 3);
        assert_eq!(max_leverage_for_notional(&t, 9_999), 3);
        assert_eq!(max_leverage_for_notional(&t, 10_000), 1);
        assert_eq!(max_leverage_for_notional(&t, i64::MAX), 1);
        // Default table: constant, equal to the setLeverage cap.
        let d = MarginTiers::default();
        assert_eq!(max_leverage_for_notional(&d, 0), 3);
        assert_eq!(max_leverage_for_notional(&d, i64::MAX), 3);
    }

    /// `is_above_maintenance_margin` under the default table must reproduce the pre-tier
    /// `>= notional/6` comparison verbatim — including the exact `>=` boundary.
    #[test]
    fn is_above_maintenance_margin_matches_the_legacy_rate() {
        let t = MarginTiers::default();
        let (bd, pd) = (8u32, 2u32);
        let mut s: u64 = 0xdead_beef_1234_5678;
        for _ in 0..20_000 {
            let mark = next(&mut s) % 1_000_000 + 1;
            let amount = (next(&mut s) % 2_000_000) as i64 - 1_000_000;
            let vq = (next(&mut s) % 4_000_000) as i64 - 2_000_000;
            let margin = (next(&mut s) % 2_000_000) as i64;
            let got = is_above_maintenance_margin(&t, mark, amount, vq, margin, bd, pd).unwrap();
            let notional = calc_value_i64(mark, amount, bd, pd).unwrap();
            let want = notional + vq + margin >= notional.abs() / 6;
            assert_eq!(got, want, "mark={mark} a={amount} vq={vq} m={margin}");
        }
    }
}

#[cfg(test)]
mod open_order_margin_tests {
    use super::*;

    /// `(PIM, IM, ooIM)` at an explicit leverage.
    fn at(n: i64, bid: u64, ask: u64, lev: u64) -> (u64, u64, u64) {
        let m = open_order_margin(n, bid, ask, lev).unwrap();
        (
            m.position_initial_margin,
            m.initial_margin,
            m.open_order_initial_margin,
        )
    }

    /// No resting orders ⇒ IM collapses to PIM ⇒ ooIM is exactly 0, at every leverage and both
    /// position signs. This is the identity the whole migration leans on: a user with no open
    /// orders must see no derived requirement at all.
    #[test]
    fn zero_orders_means_zero_open_order_margin() {
        for &n in &[
            0i64,
            1,
            7,
            1_000_000,
            -1,
            -7,
            -1_000_000,
            i64::MAX,
            i64::MIN,
        ] {
            for &lev in &[1u64, 2, 3, 7, 100] {
                let m = open_order_margin(n, 0, 0, lev).unwrap();
                assert_eq!(m.open_order_initial_margin, 0, "n={n} lev={lev}");
                assert_eq!(
                    m.initial_margin, m.position_initial_margin,
                    "n={n} lev={lev}"
                );
            }
        }
    }

    /// Flat position, buys only: `N = 0` ⇒ `max(|0 + Bid|, |0 − 0|) = Bid`, so the whole
    /// requirement is the bid leg and PIM is 0 ⇒ ooIM == IM == ROUND_UP(Bid / L).
    #[test]
    fn flat_with_only_bids() {
        assert_eq!(at(0, 1_000_000, 0, 1), (0, 1_000_000, 1_000_000));
        assert_eq!(at(0, 1_000_000, 0, 4), (0, 250_000, 250_000));
        // ROUND_UP, not floor and not HALF_UP: 999_999/4 = 249_999.75 → 250_000.
        assert_eq!(at(0, 999_999, 0, 4), (0, 250_000, 250_000));
        // ...and the exact multiple does NOT get a spurious +1.
        assert_eq!(at(0, 1_000_000, 0, 4), (0, 250_000, 250_000));
        assert_eq!(
            at(0, 1, 0, 1_000_000),
            (0, 1, 1),
            "one unit still rounds UP to 1"
        );
    }

    /// Flat position, sells only: `max(|0|, |0 − Ask|) = Ask` — the mirror image. The ask branch
    /// takes an ABSOLUTE value, so a sell-only book is a requirement, not a credit.
    #[test]
    fn flat_with_only_asks() {
        assert_eq!(at(0, 0, 1_000_000, 1), (0, 1_000_000, 1_000_000));
        assert_eq!(at(0, 0, 1_000_000, 4), (0, 250_000, 250_000));
        assert_eq!(at(0, 0, 999_999, 4), (0, 250_000, 250_000));
    }

    /// A LONG position: a resting BUY compounds the exposure (`|N + Bid|` grows), a resting SELL
    /// nets against it (`|N − Ask|` shrinks while `Ask <= 2N`). Same |order| notional, opposite
    /// effect — the asymmetry that makes this a genuinely different function from our
    /// `max(buy_side, sell_side)` escrow.
    #[test]
    fn long_with_bids_adds_but_long_with_asks_nets() {
        let n = 1_000_000i64;
        // Bid side: |1e6 + 4e5| = 1.4e6 ⇒ IM 700_000 at L=2, PIM 500_000 ⇒ ooIM 200_000.
        assert_eq!(at(n, 400_000, 0, 2), (500_000, 700_000, 200_000));
        // Ask side, same 4e5: |1e6 − 4e5| = 6e5 < |N| ⇒ IM is the PIM branch ⇒ ooIM 0.
        assert_eq!(at(n, 0, 400_000, 2), (500_000, 500_000, 0));
        // An ask big enough to flip the net exposure short DOES cost again: |1e6 − 3e6| = 2e6.
        assert_eq!(at(n, 0, 3_000_000, 2), (500_000, 1_000_000, 500_000));
        // Exactly closing the position (Ask == N) is the cheapest point: net exposure 0, but the
        // BID branch (|N| itself) still floors IM at PIM ⇒ ooIM 0.
        assert_eq!(at(n, 0, 1_000_000, 2), (500_000, 500_000, 0));
        // Mirror on a SHORT: a resting SELL compounds, a resting BUY nets.
        assert_eq!(at(-n, 0, 400_000, 2), (500_000, 700_000, 200_000));
        assert_eq!(at(-n, 400_000, 0, 2), (500_000, 500_000, 0));
    }

    /// The branch switch. With `N` fixed, `|N + Bid|` vs `|N − Ask|` cross at a determinable
    /// point; walk `Ask` across it one unit at a time and check the winner changes exactly there.
    ///
    /// `N = 1_000_000`, `Bid = 200_000` ⇒ bid branch = 1_200_000, constant. The ask branch is
    /// `|1_000_000 − Ask|`, which reaches 1_200_000 at `Ask = 2_200_000`. So the bid branch wins
    /// for `Ask < 2_200_000`, they TIE at `2_200_000`, and the ask branch wins beyond.
    #[test]
    fn the_max_switches_branch_at_the_crossing_point() {
        let (n, bid) = (1_000_000i64, 200_000u64);
        let bid_branch = 1_200_000u64;
        for (ask, want) in [
            (2_199_998u64, bid_branch),
            (2_199_999, bid_branch),
            (2_200_000, bid_branch), // tie: |1e6 − 2.2e6| == 1.2e6
            (2_200_001, 1_200_001),  // ask branch takes over, by exactly 1
            (2_200_002, 1_200_002),
        ] {
            let m = open_order_margin(n, bid, ask, 1).unwrap();
            assert_eq!(m.initial_margin, want, "ask={ask}");
            // ...and the max is genuinely a max, never a sum and never "one side always wins".
            let ask_branch = (n as i128 - ask as i128).unsigned_abs() as u64;
            assert_eq!(m.initial_margin, bid_branch.max(ask_branch), "ask={ask}");
            assert_ne!(
                m.initial_margin,
                bid_branch + ask_branch,
                "the two sides must not add"
            );
        }
    }

    /// ooIM is the DIFFERENCE OF TWO ROUND_UPs, never `ROUND_UP` of a difference. Pinned on a
    /// case where the two disagree by exactly 1 unit: `|N| = 9`, combined = 31, `L = 4` ⇒
    /// `ceil(31/4) − ceil(9/4) = 8 − 3 = 5`, whereas the convenience form
    /// `ceil((31 − 9)/4) = ceil(5.5) = 6`. A parity checker built on the convenience form
    /// mis-reports by one unit (`binance-margin-verified-model.md` §1.1).
    #[test]
    fn oo_im_is_a_difference_of_round_ups_not_a_round_up_of_a_difference() {
        // N = 9 long, Bid = 22 ⇒ bid branch 31, ask branch 9 ⇒ combined 31.
        let m = open_order_margin(9, 22, 0, 4).unwrap();
        assert_eq!((m.position_initial_margin, m.initial_margin), (3, 8));
        assert_eq!(m.open_order_initial_margin, 5);
        // The convenience form would say 6 — 1 unit too strict.
        assert_ne!(m.open_order_initial_margin, (31u64 - 9).div_ceil(4));
    }

    /// **The tier table is NOT an input to ooIM.** There is exactly one ooIM definition and it
    /// divides by the POSITION'S OWN leverage, whatever tier the combined notional lands in.
    ///
    /// A tier-capped second variant existed briefly (Phase 1) and was rejected — see the
    /// "Why no tier cap" note on [`open_order_margin`]. This test pins the consequence with the
    /// numbers that variant used to produce, so a reintroduction is a visible failure and not a
    /// silent tightening: with `N = 999_999`, `Bid = 1` and `L = 10`, the combined notional is
    /// exactly 1_000_000. Had a table `[(0, 10x), (1_000_000, 2x)]` been consulted, `L` would have
    /// been capped 10 → 2 and IM would read 500_000 with ooIM 0. It does not: IM stays
    /// `ceil(1_000_000 / 10) = 100_000` and ooIM `100_000 − ceil(999_999/10) = 100_000 − 100_000
    /// = 0`.
    #[test]
    fn the_position_leverage_is_used_uncapped_by_any_tier() {
        let no_orders = open_order_margin(999_999, 0, 0, 10).unwrap();
        assert_eq!(
            (no_orders.effective_leverage, no_orders.initial_margin),
            (10, 100_000)
        );
        let over_boundary = open_order_margin(999_999, 1, 0, 10).unwrap();
        assert_eq!(
            over_boundary.effective_leverage, 10,
            "leverage must NOT be lowered by the combined notional"
        );
        assert_eq!(over_boundary.initial_margin, 100_000); // ceil(1_000_000 / 10)
        assert_ne!(
            over_boundary.initial_margin, 500_000,
            "500_000 is the tier-capped answer the rejected variant gave"
        );
        // The only leverage input is the argument: the same inputs at L = 2 DO give the capped
        // numbers, proving the difference above is the leverage and nothing else.
        let at_two = open_order_margin(999_999, 1, 0, 2).unwrap();
        assert_eq!(
            (at_two.initial_margin, at_two.position_initial_margin),
            (500_000, 500_000)
        );
    }

    /// Leverage 0 is floored at 1 rather than dividing by zero (a defaulted or corrupt
    /// `pos.leverage` must not trap).
    #[test]
    fn zero_leverage_is_floored_at_one() {
        assert_eq!(at(0, 1_000, 0, 0), (0, 1_000, 1_000));
        let m = open_order_margin(0, 1_000, 0, 0).unwrap();
        assert_eq!((m.effective_leverage, m.initial_margin), (1, 1_000));
    }

    /// Extremes must produce a clean error, never a wrap. `|N| ~ 2^63` plus `Bid ~ 2^64` exceeds
    /// `u64` after a leverage-1 divide, so the narrowing must reject.
    #[test]
    fn saturating_inputs_error_rather_than_wrap() {
        assert!(open_order_margin(i64::MAX, u64::MAX, 0, 1).is_err());
        assert!(open_order_margin(i64::MIN, 0, u64::MAX, 1).is_err());
        // ...but the same inputs at a big enough leverage fit, and stay ordered.
        let m = open_order_margin(i64::MAX, u64::MAX, 0, 4).unwrap();
        assert!(m.initial_margin >= m.position_initial_margin);
    }

    /// `IM >= PIM` for every sign/magnitude combination — the invariant that makes
    /// `open_order_initial_margin`'s subtraction total. Randomised, since it is a claim about all
    /// inputs rather than a hand case.
    #[test]
    fn initial_margin_never_falls_below_position_initial_margin() {
        let mut s: u64 = 0x00_1f_bd_a5_c0_de_00_11;
        let next = |s: &mut u64| {
            *s ^= *s << 13;
            *s ^= *s >> 7;
            *s ^= *s << 17;
            *s
        };
        for _ in 0..20_000 {
            let n = (next(&mut s) % 4_000_000_000) as i64 - 2_000_000_000;
            let bid = next(&mut s) % 4_000_000_000;
            let ask = next(&mut s) % 4_000_000_000;
            let lev = next(&mut s) % 100 + 1;
            let m = open_order_margin(n, bid, ask, lev).unwrap();
            assert!(
                m.initial_margin >= m.position_initial_margin,
                "n={n} bid={bid} ask={ask} lev={lev}: IM {} < PIM {}",
                m.initial_margin,
                m.position_initial_margin
            );
            // ooIM == 0 exactly when the position branch already dominates.
            assert_eq!(
                m.open_order_initial_margin == 0,
                m.initial_margin == m.position_initial_margin
            );
        }
    }
}
