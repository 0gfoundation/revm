use context::ContextTr;
use super::*;
use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::{
    interface::IPerpDex::{
        cancelOrderCall, getMarketFeeTotalCall, getOrderCall, placeOrderCall, AccountBalanceChanged,
    },
    run_perp_dex_call, storage,
    types::{
        AccountUpdateReason, FundingState, MarginTiers, Market, OrderStatus, PerpPosition,
        UserFeeRates,
    },
    PERP_DEX_ADDRESS, USDC_ADDRESS,
};

// ── Constants ──────────────────────────────────────────────────────────────

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const BOB: Address = address!("2222222222222222222222222222222222222222");
const CAROL: Address = address!("3333333333333333333333333333333333333333");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

const MARKET_ID: u64 = 1;
/// 1 tick = $1 when the test market uses price_decimals = 9.
const TICK: u64 = 1_000_000_000;
/// Test price: $100.
const PRICE: u64 = 100 * TICK;
/// Test quantity: 0.01 BTC (base_decimals = 8, so 1 BTC = 1e8 units).
const QTY: u64 = 1_000_000;
/// calc_value(PRICE, QTY, 8, 9) = 1_000_000 (1 USDC in 6-decimal units).
/// price * qty * 1e6 / (1e9 * 1e8) = 100e9 * 1e6 * 1e6 / 1e17 = 1e6.
const FILL_VALUE: u64 = 1_000_000;
/// Initial margin at leverage 1 = FILL_VALUE / 1.
const INIT_MARGIN: u64 = FILL_VALUE;
const TAKER_FEE: u64 = 0;
const MAKER_FEE: u64 = 0;
/// Starting perp wallet balance per user (10 USDC = 10_000_000 in 6-decimal units).
const WALLET: u64 = 10_000_000;

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

// ── Context helpers ────────────────────────────────────────────────────────

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, BOB, CAROL, ADMIN] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

fn take_position_changes(
    ctx: &mut TestCtx,
) -> Vec<crate::interface::IPerpDex::PositionChanged> {
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|log| {
            log.data.topics().first()
                == Some(&crate::interface::IPerpDex::PositionChanged::SIGNATURE_HASH)
        })
        .map(|log| {
            crate::interface::IPerpDex::PositionChanged::decode_raw_log(
                log.data.topics(),
                &log.data.data,
            )
            .unwrap()
        })
        .collect()
}

/// Register the default BTC-perp market and fund ALICE + BOB with WALLET.
fn setup(ctx: &mut TestCtx) {
    storage::save_admin(ctx, ADMIN).unwrap();
    storage::save_market(
        ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 8,
            price_decimals: 9,
            tick_size: TICK,
            step_size: QTY,
            min_quantity: QTY,
            max_quantity: QTY * 1_000,
            max_price: PRICE * 1_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 0,
            mark_price: 0,
            tiers: MarginTiers::default(),
        },
    )
    .unwrap();
    fund(ctx, ALICE, WALLET);
    fund(ctx, BOB, WALLET);
}

/// Directly credit a user's perp wallet (bypasses deposit/transfer flow).
fn fund(ctx: &mut TestCtx, user: Address, amount: u64) {
    let mut acc = storage::load_account(ctx, user).unwrap();
    acc.credit_perp(amount).unwrap();
    storage::save_account(ctx, user, acc, AccountUpdateReason::Adjustment).unwrap();
}

fn wallet(ctx: &mut TestCtx, user: Address) -> u64 {
    storage::load_account(ctx, user)
        .unwrap()
        .visible_perp_wallet_balance()
}

fn pos(ctx: &mut TestCtx, user: Address) -> PerpPosition {
    storage::load_position(ctx, user, MARKET_ID).unwrap()
}

/// Read `getAccount(user)` through the full call shell.
fn get_account(ctx: &mut TestCtx, user: Address) -> crate::interface::IPerpDex::getAccountReturn {
    use crate::interface::IPerpDex::getAccountCall;
    let out = run_perp_dex_call(
        &getAccountCall { user }.abi_encode(),
        1_000_000,
        user,
        U256::ZERO,
        true,
        ctx,
    )
    .unwrap();
    assert!(!out.reverted, "getAccount reverted: {:?}", out.bytes);
    getAccountCall::abi_decode_returns(&out.bytes).unwrap()
}

/// **Every field of an `AccountBalanceChanged` payload against `getAccount` for the same user in the
/// CURRENT state.** The event's payload is a strict SUBSET of `getAccount`'s scalar set (three
/// balances; the seven account-level margin totals are `getAccount`-only, the way Binance keeps them
/// on REST rather than on `ACCOUNT_UPDATE`), so "every field of the payload" is three comparisons —
/// but they are still the *whole* payload, and one surface folding a different market set, or
/// clamping where the other does not, fails here.
///
/// It is deliberately **not** only a comparison against `getAccount`: the two folds are different
/// walks now (`index_account_wallet_balances` vs `index_account_scalars`), so this also re-derives
/// both non-trivial fields from RAW STORED STATE — the account blob and `Σ pos.margin` over the
/// index — which is what keeps it from degenerating into "two calls into the same code agree".
///
/// Only meaningful for a user's LAST event of a call (earlier ones are intermediate after-images by
/// design), which is exactly how the callers use it.
fn assert_event_matches_get_account(ctx: &mut TestCtx, event: &AccountBalanceChanged) {
    let a = get_account(ctx, event.user);
    assert_eq!(
        (
            event.usdcBalance,
            event.totalWalletBalance,
            event.totalCrossWalletBalance,
        ),
        (
            a.usdcBalance,
            a.totalWalletBalance,
            a.totalCrossWalletBalance,
        ),
        "AccountBalanceChanged and getAccount disagree for {:?}",
        event.user
    );

    // ── …and both non-trivial fields, re-derived from raw stored state ────────────────────────
    let acct = storage::load_account(ctx, event.user).unwrap();
    let stored_usdc: U256 = acct.usdc_balance.clone().into();
    assert_eq!(
        (event.usdcBalance, event.totalCrossWalletBalance),
        (stored_usdc, acct.perp_wallet_balance),
        "the two stored balances go out verbatim, unclamped, for {:?}",
        event.user
    );
    let sigma_margin: i128 = a
        .positions
        .iter()
        .map(|p| {
            storage::load_position(ctx, event.user, p.marketId)
                .unwrap()
                .margin as i128
        })
        .sum();
    assert_eq!(
        event.totalWalletBalance as i128,
        event.totalCrossWalletBalance as i128 + sigma_margin,
        "totalWalletBalance is GROSS: cross + Σ pos.margin over the per-user index, for {:?}",
        event.user
    );
}

/// The DERIVED open-order requirement `ooIM` for `user` in the test market — the replacement for
/// the deleted `pos.margin_reserved` field in every assertion that used to read it. Unlike that
/// field this is not stored: it is recomputed from `(N, Bid, Ask, L)` and moves with the mark.
fn oo_im(ctx: &mut TestCtx, user: Address) -> u64 {
    let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    let p = storage::load_position(ctx, user, MARKET_ID).unwrap();
    crate::margin_view::position_open_order_margin(&market, &p).unwrap()
}

/// `perp_wallet_balance − Σ ooIM` — the account's spendable headroom, i.e. the quantity every
/// admission gate now compares against. Signed: it can legitimately go negative.
fn available(ctx: &mut TestCtx, user: Address) -> i128 {
    crate::margin_view::derived_available_balance(ctx, user).unwrap()
}

/// Set `user`'s wallet so that AVAILABLE lands exactly on `target`.
///
/// The derived-basis analogue of the old `perp_wallet_balance = <hand-computed leftover>`
/// fixtures: those numbers were the leftover AFTER the escrow had physically removed the resting
/// orders' margin, i.e. they WERE the available. Nothing is removed any more, so reproducing the
/// same account state means putting the requirement back into the wallet.
fn set_available(ctx: &mut TestCtx, user: Address, target: i64) {
    let current = available(ctx, user);
    let mut acc = storage::load_account(ctx, user).unwrap();
    acc.perp_wallet_balance += (target as i128 - current) as i64;
    storage::save_account(ctx, user, acc, AccountUpdateReason::Adjustment).unwrap();
    assert_eq!(available(ctx, user), target as i128);
}

/// Give the test market a live mark price.
///
/// `setup()` leaves `mark_price` at 0, which PRODUCTION CANNOT REACH (`addMarket` rejects a zero
/// initial mark and every mark component is floored away from zero). A zero mark makes `N = 0`,
/// so the derived basis is blind to the position and a risk-reducing order looks naked. Any test
/// whose subject is how a POSITION interacts with resting orders has to set a real mark, or it is
/// pinning a fixture artefact.
fn set_mark(ctx: &mut TestCtx, price: u64) {
    storage::save_mark_price(ctx, MARKET_ID, price).unwrap();
}

/// Deterministic distinct test user address from a small index (avoids ALICE/BOB/CAROL/ADMIN).
fn user_addr(i: u64) -> Address {
    let mut b = [0u8; 20];
    b[12..20].copy_from_slice(&i.to_be_bytes());
    Address::from(b)
}

// ── Call boundaries for the coalesced `AccountBalanceChanged` drain ───────────────────────────
//
// `AccountBalanceChanged` is no longer emitted at the write site: writes MARK the user in a
// call-scoped set and `call::run_perp_dex_call` drains it once, in address order, on its success
// path. Most helpers here drive an engine handler DIRECTLY (`run_place_order`, `storage::*`), which
// is deliberately below that shell — so a test asserting on the account event has to open and close
// the call itself. Tests that go through `run_perp_dex_call` need neither.

/// Open a fresh call: drop marks the fixture (or a previous action) left behind.
fn start_call(ctx: &mut TestCtx) {
    storage::begin_perp_call(ctx);
}

/// Close the call: publish one coalesced snapshot per marked user, in ascending address order.
fn end_call(ctx: &mut TestCtx) {
    storage::flush_account_snapshots(ctx).unwrap();
}

/// Place an order and return its 32-byte order ID.
fn place(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8, // 0 = Buy,   1 = Sell
    price: u64,
    qty: u64,
    order_type: u8, // 0 = Limit, 1 = Market
    tif: u8,        // 0 = GTC,   1 = IOC,  2 = FOK,  3 = PostOnly
) -> [u8; 32] {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let ret = run_place_order(&input, caller, ctx).unwrap();
    ret[..32].try_into().unwrap()
}

fn get_order(ctx: &mut TestCtx, id: [u8; 32]) -> crate::types::Order {
    storage::load_order(ctx, &id).unwrap().unwrap()
}

/// delete-on-terminal (commit-only #23): a Filled/Cancelled/Expired order is removed from the map.
/// Tests that used to assert `get_order(id).status == <terminal>` now assert the record is GONE —
/// the concrete terminal flavour (fill vs cancel vs expire) is no longer stored; each test pins the
/// actual outcome via its position/account/book assertions.
fn assert_terminal(ctx: &mut TestCtx, id: [u8; 32]) {
    assert!(
        storage::load_order(ctx, &id).unwrap().is_none(),
        "order {id:?} should be terminal (deleted under delete-on-terminal) but is still present"
    );
}

fn market_fee_total(ctx: &mut TestCtx) -> u64 {
    let ret = run_get_market_fee_total(
        &getMarketFeeTotalCall {
            marketId: MARKET_ID,
        }
        .abi_encode(),
        ctx,
    )
    .unwrap();
    U256::from_be_slice(&ret[..32]).to::<u64>()
}

// ── Input validation ───────────────────────────────────────────────────────

#[test]
fn rejects_unknown_market() {
    let mut ctx = make_ctx();
    let input = placeOrderCall {
        marketId: 99,
        side: 0,
        price: PRICE,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("unknown market"), "{err}");
}

#[test]
fn rejects_quantity_above_maximum() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY * 1_001,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("exceeds maximum"), "{err}");
}

#[test]
fn rejects_price_above_maximum() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE * 1_001,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("exceeds maximum"), "{err}");
}

#[test]
fn rejects_quantity_below_minimum() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY / 2,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("below minimum"), "{err}");
}

// ── Price band (fill-time) ──────────────────────────────────────────────────

/// Register a market with an explicit `price_band_bps` (no mark set) and fund
/// ALICE/BOB generously so far-from-mark acceptance cases can reserve margin.
/// Tests set the mark themselves via `storage::save_mark_price` when they want
/// the band active (an unset mark == 0 skips the band by design).
fn setup_banded(ctx: &mut TestCtx, band_bps: u32) {
    storage::save_admin(ctx, ADMIN).unwrap();
    storage::save_market(
        ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 8,
            price_decimals: 9,
            tick_size: TICK,
            step_size: QTY,
            min_quantity: QTY,
            max_quantity: QTY * 1_000,
            max_price: PRICE * 1_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: band_bps,
            mark_price: 0,
            tiers: MarginTiers::default(),
        },
    )
    .unwrap();
    fund(ctx, ALICE, WALLET * 1_000);
    fund(ctx, BOB, WALLET * 1_000);
}

fn try_place_limit(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8,
    price: u64,
) -> Result<Bytes, PerpError> {
    try_place_limit_tif(ctx, caller, side, price, 0)
}

/// `try_place_limit` with an explicit TIF. The `fill_band_blocks_*` fixtures below need an **IOC**
/// taker, and the reason is structural, not cosmetic: to cross a level that sits on the FAR side of
/// the band the taker's own limit price must be past the same edge, so a GTC taker's surviving
/// remainder would itself be a too-good new best and be refused at placement. IOC drops the
/// remainder instead, which leaves the fill-time band as the only thing under test.
fn try_place_limit_tif(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8,
    price: u64,
    tif: u8,
) -> Result<Bytes, PerpError> {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: QTY,
        orderType: 0, // Limit
        tif,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    run_place_order(&input, caller, ctx)
}

/// The CURRENT `(upper, lower)` band edges of the test market, read from stored state rather than
/// recomputed from a comment — so a fixture's precondition assertion cannot drift away from the
/// market it actually runs against.
fn band(ctx: &mut TestCtx) -> (u128, u128) {
    let m = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    crate::math::mark_band_bounds(m.mark_price, m.price_band_bps)
}

/// PRECONDITION assertion shared by every fixture below that needs a resting level the fill-time
/// band must refuse: the level really is OUTSIDE the band, and it really is the cached best, so the
/// match walk provably reaches it.
///
/// Without this a fixture can silently stop testing anything. Its "did not fill" assertions pass
/// vacuously the moment the level becomes tradeable (a price constant, the band width or the mark
/// only has to drift), and they pass vacuously again if the level is not at the touch, because then
/// the walk stops before it. Both have happened in this file.
fn assert_out_of_band_best(ctx: &mut TestCtx, side: u8, price: u64) {
    let (upper, lower) = band(ctx);
    let p = price as u128;
    assert!(
        p < lower || p > upper,
        "precondition: {price} must be OUTSIDE the band [{lower}, {upper}] — otherwise the \
         fill-time band under test does not apply to it at all"
    );
    let best = if side == 0 {
        storage::load_best_bid(ctx, MARKET_ID)
    } else {
        storage::load_best_ask(ctx, MARKET_ID)
    }
    .unwrap();
    assert_eq!(
        best, price,
        "precondition: the out-of-band level must be the cached best, or the match walk never \
         reaches it and 'did not fill' proves nothing"
    );
}

/// The surviving half of the deleted `out_of_band_limit_orders_now_rest_at_placement`.
///
/// ⚠️ That fixture's two legs — a bid 30% ABOVE mark and an ask 60% BELOW mark, each on an empty
/// book — are now DELIBERATELY REJECTED: both are a too-good NEW BEST, which is exactly what the
/// placement reject forbids (`a_too_good_new_best_is_rejected_on_both_sides`). Its claim "there is
/// no placement band" is no longer true as stated, so it is replaced rather than adjusted.
///
/// What survives, and is the entire reason the reject is ONE-SIDED: a FAR-side out-of-band quote
/// still rests, *even when it becomes the BBO*. A bid below the lower edge / an ask above the upper
/// edge cannot mis-reject the opposite side's PostOnly orders (it only makes the cross check more
/// permissive), and it is the ordinary thin-book deep quote the fill-time band exists to allow.
#[test]
fn a_far_side_out_of_band_quote_still_rests_even_as_the_new_best() {
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0); // default +-10%
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap(); // mark $100 => [$90, $110]

    // A bid 60% BELOW mark: far side, and the new best bid.
    assert!(
        try_place_limit(&mut ctx, ALICE, 0, 40 * TICK).is_ok(),
        "a far-BELOW bid must still rest, even as the new best"
    );
    assert_out_of_band_best(&mut ctx, 0, 40 * TICK);

    // An ask 30% ABOVE mark: the mirror, and the new best ask.
    assert!(
        try_place_limit(&mut ctx, BOB, 1, 130 * TICK).is_ok(),
        "a far-ABOVE ask must still rest, even as the new best"
    );
    assert_out_of_band_best(&mut ctx, 1, 130 * TICK);
}

// ── Placement band: a too-good NEW BEST is refused ──────────────────────────
//
// The 2x2 the reject is defined over is (becomes best / does not) x (too good / far side):
//
//                 |  too good (ask < lower, bid > upper)  |  far side
//   becomes best  |  REJECT  <- the only rejected cell    |  ACCEPT (a_far_side_..._new_best)
//   not the best  |  ACCEPT  (a_too_good_quote_behind_..) |  ACCEPT (trivially, never at the touch)
//
// plus the crossing-GTC remainder, which reaches the rejected cell without going near PostOnly.

/// The rejected cell, both sides, with the edge pinned inclusive on each so the boundary cannot
/// drift: 111 is refused and 110 accepted; 89 is refused and 90 accepted.
#[test]
fn a_too_good_new_best_is_rejected_on_both_sides() {
    // ── Buy: a bid ABOVE the upper edge ──
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap(); // [$90, $110]
    let err = try_place_limit(&mut ctx, ALICE, 0, 111 * TICK).unwrap_err();
    assert!(
        err.to_string()
            .contains("a new best quote must be inside the price band"),
        "{err}"
    );
    assert!(
        storage::load_bid_prices(&mut ctx, MARKET_ID)
            .unwrap()
            .is_empty(),
        "the reject is write-clean: nothing entered the book"
    );
    assert_eq!(
        storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(),
        0,
        "and the BBO cache was not written either"
    );
    assert!(
        try_place_limit(&mut ctx, ALICE, 0, 110 * TICK).is_ok(),
        "the upper edge itself is INSIDE the band"
    );

    // ── Sell: an ask BELOW the lower edge ──
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    let err = try_place_limit(&mut ctx, BOB, 1, 89 * TICK).unwrap_err();
    assert!(
        err.to_string()
            .contains("a new best quote must be inside the price band"),
        "{err}"
    );
    assert!(
        storage::load_ask_prices(&mut ctx, MARKET_ID)
            .unwrap()
            .is_empty(),
        "the reject is write-clean: nothing entered the book"
    );
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), 0);
    assert!(
        try_place_limit(&mut ctx, BOB, 1, 90 * TICK).is_ok(),
        "the lower edge itself is INSIDE the band"
    );
}

/// PostOnly is not special-cased — it takes the same reject through the same code. Worth pinning
/// separately because PostOnly is the ONE path whose harm motivated the reject, so a future
/// refactor that moved the test onto the PostOnly branch would still pass this and fail
/// `a_crossing_gtc_remainder_is_refused_as_a_too_good_best`.
#[test]
fn a_too_good_post_only_new_best_is_rejected() {
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    let err = try_place_limit_tif(&mut ctx, ALICE, 0, 111 * TICK, 3).unwrap_err();
    assert!(
        err.to_string()
            .contains("a new best quote must be inside the price band"),
        "{err}"
    );
    assert!(storage::load_bid_prices(&mut ctx, MARKET_ID)
        .unwrap()
        .is_empty());
}

/// The "does not become the best x too good" cell: ACCEPTED.
///
/// Only reachable through MARK DRIFT, and necessarily so — "too good" means a better price, and
/// bids rank better-first, so against an in-band book a too-good bid is ALWAYS the best. It takes an
/// even-more-too-good level in front of it, which itself can only have been stranded by a mark move.
/// That is the precise sense in which the reject is gated on the BBO and not on out-of-bandness.
#[test]
fn a_too_good_quote_behind_an_even_better_stranded_best_is_accepted() {
    // ── Buy side ──
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    // Placed IN band: mark $200 => [$180, $220].
    storage::save_mark_price(&mut ctx, MARKET_ID, 200 * TICK).unwrap();
    try_place_limit(&mut ctx, ALICE, 0, 200 * TICK).expect("in band at the mark it was placed at");
    // DRIFT (no oracle call, so no band-expiry GC): mark $100 => [$90, $110]. The $200 bid is now a
    // stranded too-good best.
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    assert_out_of_band_best(&mut ctx, 0, 200 * TICK);
    // $150 is ALSO above the upper edge, but it is not the best, so it is invisible to both harms.
    try_place_limit(&mut ctx, BOB, 0, 150 * TICK)
        .expect("a too-good quote BEHIND the best is not the BBO, so it rests");
    assert_eq!(
        storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![150 * TICK, 200 * TICK],
        "it really rested (the index is ascending)"
    );
    assert_eq!(
        storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(),
        200 * TICK,
        "precondition: it did NOT become the best — that is why it was allowed"
    );

    // ── Sell mirror ──
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, 50 * TICK).unwrap(); // [$45, $55]
    try_place_limit(&mut ctx, ALICE, 1, 50 * TICK).expect("in band at the mark it was placed at");
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap(); // [$90, $110]
    assert_out_of_band_best(&mut ctx, 1, 50 * TICK);
    try_place_limit(&mut ctx, BOB, 1, 70 * TICK)
        .expect("a too-good ask ABOVE the stranded best ask rests");
    assert_eq!(
        storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![50 * TICK, 70 * TICK]
    );
    assert_eq!(
        storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(),
        50 * TICK,
        "precondition: it did NOT become the best"
    );
}

/// A crossing GTC that fills the whole book and rests its remainder: the maker ask at $100 is IN
/// band and gets taken, and the remainder at $150 is refused as a too-good best.
///
/// Build the scenario: an in-band maker at the touch, and a taker whose own limit is past the upper
/// edge. Both halves are asserted as preconditions below, because either one drifting turns the
/// reject into a different (already-covered) case.
fn band_cancel_reasons(ctx: &mut TestCtx) -> Vec<u8> {
    use crate::interface::IPerpDex::OrderCancelled;
    use alloy_sol_types::SolEvent;
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
        .map(|l| {
            OrderCancelled::decode_raw_log(l.data.topics(), &l.data.data)
                .unwrap()
                .reason
        })
        .collect()
}

fn crossing_gtc_over_an_in_band_maker(ctx: &mut TestCtx) -> ([u8; 32], Result<Bytes, PerpError>) {
    setup_banded(ctx, 0);
    storage::save_mark_price(ctx, MARKET_ID, PRICE).unwrap(); // mark $100 => [$90, $110]
    let ask = try_place_limit(ctx, ALICE, 1, PRICE).unwrap();
    let ask: [u8; 32] = ask[..32].try_into().unwrap();

    let (upper, lower) = band(ctx);
    assert_eq!(
        storage::load_best_ask(ctx, MARKET_ID).unwrap(),
        PRICE,
        "precondition: the maker is at the touch, so the walk reaches it"
    );
    assert!(
        (PRICE as u128) >= lower && (PRICE as u128) <= upper,
        "precondition: the maker is IN band, so the fill-time band lets it trade — without this \
         the taker would fill nothing and this would be the empty-book case"
    );
    let taker_price = 150 * TICK;
    assert!(
        taker_price > PRICE,
        "precondition: the taker's limit crosses the resting ask in PRICE"
    );
    assert!(
        (taker_price as u128) > upper,
        "precondition: and the taker's own limit is too good, so its REMAINDER is the rejected cell"
    );

    // 2 lots at $150 against 1 resting lot: 1 fills at $100, 1 would rest at $150.
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: taker_price,
        quantity: QTY * 2,
        orderType: 0,
        tif: 0, // GTC — the remainder rests
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let result = run_place_order(&input, BOB, ctx);
    (ask, result)
}
/// **The case gating PostOnly alone would have missed.** A crossing GTC fills what the band allows
/// and then wants to rest its REMAINDER at its own limit price, so it can manufacture a too-good
/// best without ever touching `ensure_post_only_does_not_cross`.
///
/// FLIPPED by the atomic match+rest hoist. It used to fill, then EXPIRE the remainder, because
/// `rest_in_book` ran after the flush and raising would have leaked the fills. The barrier now sits
/// BELOW the rest decision, so the whole order rejects — which is what Binance does and what the
/// order-lifecycle contract needs (the expiry emitted an `OrderCancelled` with no `OrderRested`
/// before it, and `CancelReason::PriceBandExpiry` says "the order was RESTING out of band", which
/// this order never was; a downstream projector broke on exactly that).
#[test]
fn a_crossing_gtc_remainder_that_would_be_a_too_good_best_rejects_the_whole_order() {
    let mut ctx = make_ctx();
    let (_ask, result) = crossing_gtc_over_an_in_band_maker(&mut ctx);
    let err = result.expect_err(
        "a crossing GTC whose remainder would be a too-good best must reject as a WHOLE order — \
         the rest decision is now taken before the write barrier, so there are no fills to keep",
    );
    assert!(
        err.to_string().contains("must be inside the price band"),
        "unexpected reject reason: {err}"
    );
    assert!(
        storage::load_bid_prices(&mut ctx, MARKET_ID)
            .unwrap()
            .is_empty(),
        "the remainder must not sneak into the book"
    );
    assert_eq!(
        storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(),
        0,
        "nor into the BBO cache — the two writes `becomes_best` guards stayed inside PHASE 2"
    );
    assert_eq!(
        band_cancel_reasons(&mut ctx),
        Vec::<u8>::new(),
        "and NO OrderCancelled: a whole-order reject is not a cancellation of a resting order, so \
         it must not borrow a CancelReason that claims the order was resting"
    );
}

/// The other half: a whole-order reject really does mean **the fills do not happen either**.
///
/// This is the assertion the old expiry could not make, and the reason the hoist is the right end
/// state rather than a second mitigation: because the decision now precedes `MatchRegistry::flush`,
/// dropping the `MatchOutcome` un-flushed rolls the entire match back — the maker's ask is still
/// resting, and neither party's position moved. Under the old ordering the maker's order was already
/// deleted by the time the refusal was known, which is precisely why raising was unsafe there.
#[test]
fn a_refused_crossing_gtc_remainder_rolls_back_its_own_fills() {
    let mut ctx = make_ctx();
    let (ask, result) = crossing_gtc_over_an_in_band_maker(&mut ctx);
    assert!(result.is_err(), "precondition: the placement is refused");
    assert!(
        storage::load_order(&mut ctx, &ask).unwrap().is_some(),
        "the maker's ask must SURVIVE — a reject that consumed it would be the val0 leak with a \
         different trigger"
    );
    assert_eq!(
        pos(&mut ctx, BOB).amount,
        0,
        "and the taker keeps nothing: no fill happened, so there is nothing to keep"
    );
    assert_eq!(
        pos(&mut ctx, ALICE).amount,
        0,
        "…on the maker's side either (mirror of the taker assertion)"
    );
}

// ── commit-only #23: a rejected placement must not leave perp writes behind ───────────────────
//
// The class, stated once: off-trie perp writes are COMMIT-ONLY (there is no `PerpUndo`), while
// `context.log` is EVM-journaled and truncates on revert. So any `perp_err` raised AFTER the
// registry flush produces the exact signature observed on val0 at block 1,098,719 — a failed
// receipt, ZERO logs, and mutated positions/wallets. `match_order`'s APPLY comment asserts
// "nothing a user can provoke rejects after this line"; these tests are what holds it to that.

/// The slice of off-trie perp state a placement can move, for the two counterparties plus the book.
///
/// Deliberately NOT the on-trie commitment hash: `place_order_core` bumps the caller's orderId
/// nonce before any of this, so even a perfectly clean reject moves the commitment. What must be
/// invariant across a reject is the *economic* state — positions, wallets, the book and the order
/// map — which is exactly what the val0 projector observed moving.
/// Serialises every test that touches `call::PERP_WRITE_THEN_REVERT_COUNT`.
///
/// Same reason as `batch_place::ABORT_COUNTER_LOCK`: the counter is deliberately PROCESS-WIDE and
/// cargo runs tests in parallel, so `the_tripwire_still_bites_on_a_real_write_then_revert` bumping
/// it can land between another test's `before` and `after` reads. Poisoning is ignored — a panic in
/// one of these tests is already a failure, and `#[should_panic]` unwinds through this guard by
/// design.
static TRIPWIRE_COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_tripwire_counter() -> std::sync::MutexGuard<'static, ()> {
    TRIPWIRE_COUNTER_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, PartialEq)]
struct PerpStateSnapshot {
    positions: Vec<(Address, PerpPosition)>,
    wallets: Vec<(Address, i64)>,
    buy_entries: Vec<(Address, Vec<OrderEntry>)>,
    sell_entries: Vec<(Address, Vec<OrderEntry>)>,
    bid_prices: Vec<u64>,
    ask_prices: Vec<u64>,
    best: (u64, u64),
    last_traded: u64,
    orders_present: Vec<([u8; 32], bool)>,
}

fn snapshot_perp_state(
    ctx: &mut TestCtx,
    users: &[Address],
    order_ids: &[[u8; 32]],
) -> PerpStateSnapshot {
    let hot = storage::load_market_hot(ctx, MARKET_ID).unwrap();
    PerpStateSnapshot {
        positions: users
            .iter()
            .map(|&u| (u, storage::load_position(ctx, u, MARKET_ID).unwrap()))
            .collect(),
        wallets: users
            .iter()
            .map(|&u| {
                (
                    u,
                    storage::load_account(ctx, u).unwrap().perp_wallet_balance,
                )
            })
            .collect(),
        buy_entries: users
            .iter()
            .map(|&u| {
                (
                    u,
                    storage::load_buy_orders(ctx, u, MARKET_ID)
                        .unwrap()
                        .into_iter()
                        .collect(),
                )
            })
            .collect(),
        sell_entries: users
            .iter()
            .map(|&u| {
                (
                    u,
                    storage::load_sell_orders(ctx, u, MARKET_ID)
                        .unwrap()
                        .into_iter()
                        .collect(),
                )
            })
            .collect(),
        bid_prices: storage::load_bid_prices(ctx, MARKET_ID).unwrap(),
        ask_prices: storage::load_ask_prices(ctx, MARKET_ID).unwrap(),
        best: (hot.best_bid, hot.best_ask),
        last_traded: hot.last_traded,
        orders_present: order_ids
            .iter()
            .map(|id| (*id, storage::load_order(ctx, id).unwrap().is_some()))
            .collect(),
    }
}

/// The val0 fixture, parameterised on the taker's starting wallet.
///
/// A SELL GTC for 2 lots against a single resting bid of 1 lot: 1 lot fills at $100 and the
/// remainder wants to rest at $100. The taker ends SHORT, which is what makes the
/// Assuming-Price floor bite — it applies to the SELL side only (`rest_in_book` freezes
/// `price.max(assuming_floor)` on a sell and `price` verbatim on a buy), matching the SELL in the
/// on-chain report.
///
/// Returns `(result, snapshot_before, snapshot_after, maker_order_id)`.
fn sell_gtc_partial_fill_then_rest(
    ctx: &mut TestCtx,
    taker_available: i64,
) -> (
    Result<Bytes, PerpError>,
    PerpStateSnapshot,
    PerpStateSnapshot,
    [u8; 32],
) {
    setup(ctx);
    // A real mark is mandatory here, not decoration: `assuming_price_floor` is
    // `max(ROUND_UP(lastTraded x 1.0015), mark)`, so with mark = 0 the pre-fill floor would be 0
    // and the fixture would be measuring a zero-mark artefact instead of the divergence.
    set_mark(ctx, PRICE);

    // Maker: one bid lot at $100 for the taker to cross.
    let bid = place(ctx, ALICE, 0, PRICE, QTY, 0, 0);

    // PRECONDITION: no trade has happened in this market yet, so the pre-fill floor is the MARK...
    assert_eq!(
        storage::load_market_hot(ctx, MARKET_ID)
            .unwrap()
            .last_traded,
        0,
        "precondition: last_traded must be unset, so the pre-check's floor is the mark"
    );
    let floor_before = crate::math::assuming_price_floor(0, PRICE).unwrap();
    assert_eq!(
        floor_before, PRICE,
        "precondition: pre-fill floor == the limit price, so the pre-check values the rest at $100"
    );
    // ...and the post-fill floor is STRICTLY above it, because a crossing sell always drags
    // last_traded to at least its own limit price and the markup is then applied on top. This is
    // the whole divergence: same formula, two different `last_traded` reads.
    let floor_after = crate::math::assuming_price_floor(PRICE, PRICE).unwrap();
    assert!(
        floor_after > floor_before,
        "precondition: the post-fill floor ({floor_after}) must exceed the pre-fill one \
         ({floor_before}) or there is no divergence to reproduce"
    );

    set_available(ctx, BOB, taker_available);

    let before = snapshot_perp_state(ctx, &[ALICE, BOB], &[bid]);
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 1, // SELL
        price: PRICE,
        quantity: QTY * 2,
        orderType: 0, // Limit
        tif: 0,       // GTC — the remainder rests, so `rest_in_book` runs AFTER the flush
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let result = run_place_order(&input, BOB, ctx);
    let after = snapshot_perp_state(ctx, &[ALICE, BOB], &[bid]);
    (result, before, after, bid)
}

/// **The val0 leak, reproduced.**
///
/// History, because the fixture only makes sense with it. The taker's wallet used to be set to
/// exactly what the PRE-check demanded and 1500 units less than what `rest_in_book` demanded, so the
/// two disagreed and the reject landed after the flush:
///
/// * pre-check (`finalize_compute`, settlement.rs): `need = total_required + rest_delta`
///   `= 1_000_000 + calc_value($100.00, QTY) = 2 x FILL_VALUE`, because it resolved the floor
///   from the PRE-fill `last_traded` (= 0 → floor = mark = $100).
/// * real check (`rest_in_book`, trading/mod.rs sell arm): the same formula over
///   `calc_value($100.15, QTY)`, because `save_last_traded_price` ran in between.
///
/// **That window no longer exists**: `save_last_traded_price` is deferred past the rest decision, so
/// `T` is resolved exactly once — and at the PRE-fill `last_traded`, which is the documented-correct
/// input ("not the caller's own fill", `margin_view::assuming_price_floor`). `2 x FILL_VALUE` is
/// therefore now genuinely affordable, so the fixture is driven **one quantum below** it: enough to
/// refuse, which is the state this test is about.
///
/// The assertion is the invariant and not the mechanism: **`Err` ⇒ no perp state moved.** It held
/// under the expiry fix (nothing rejected) and it holds under the hoist (the reject is clean).
#[test]
fn a_rejected_placement_must_not_move_perp_state() {
    let mut ctx = make_ctx();
    // The single evaluation's exact requirement, minus one: the filled lot's opening margin plus
    // the marginal ooIM of a rest valued at the LIMIT price (`|N| + Ask` at leverage 1, minus
    // `|N|`), i.e. `2 x FILL_VALUE` — one quantum short of it.
    let need = (2 * FILL_VALUE) as i64;
    let (result, before, after, _bid) = sell_gtc_partial_fill_then_rest(&mut ctx, need - 1);

    let e = result.expect_err(
        "NOT VACUOUS: this fixture must actually reject, or the write-clean assertion below proves \
         nothing. One quantum below the requirement is a refusal.",
    );
    assert_eq!(
        after, before,
        "commit-only #23 VIOLATED: placeOrder returned Err({e:?}) but perp state MOVED. \
         Logs are EVM-journaled and truncate on revert, so on-chain this is a failed receipt \
         with zero logs and a silently mutated position — exactly val0 block 1,098,719."
    );
}

/// The same invariant swept across the whole divergence window, so the fixture cannot go vacuous
/// if a constant drifts and `precheck_need` stops landing inside it. Every wallet value that
/// rejects must reject cleanly; the ones that succeed are checked by the sibling test.
#[test]
fn no_wallet_value_lets_a_rejected_placement_move_perp_state() {
    // The window is `[precheck_need, rest_in_book_need)`, i.e. the 0.15% markup on one lot; sweep
    // a margin either side of it.
    let base = (2 * FILL_VALUE) as i64;
    let markup = calc_value(
        crate::math::assuming_price_floor(PRICE, PRICE).unwrap(),
        QTY,
        8,
        9,
    )
    .unwrap() as i64
        - FILL_VALUE as i64;
    assert!(markup > 0, "the markup must be a real gap");
    for target in [
        base - 1,
        base,
        base + 1,
        base + markup - 1,
        base + markup,
        base + markup + 1,
    ] {
        let mut ctx = make_ctx();
        let (result, before, after, _bid) = sell_gtc_partial_fill_then_rest(&mut ctx, target);
        if let Err(e) = &result {
            assert_eq!(
                after, before,
                "commit-only #23 VIOLATED at available={target}: Err({e:?}) with moved perp state"
            );
        }
    }
}

/// The other side of the boundary: **exactly at the requirement, the order is ACCEPTED and rests.**
///
/// This is the half that keeps the sibling honest. `Err ⇒ no writes` is also satisfied by "reject
/// everything", so something has to pin that the gate did not simply get stricter — and this is the
/// very wallet value (`2 x FILL_VALUE`) that used to be refused *after the flush*, i.e. the val0
/// input itself. It now rests, because the one surviving resolution of `T` reads the PRE-fill
/// `last_traded`, which is what `assuming_price_floor` documents as correct ("not the caller's own
/// fill").
#[test]
fn the_val0_wallet_value_is_affordable_and_the_remainder_rests() {
    let mut ctx = make_ctx();
    let (result, _before, after, _bid) =
        sell_gtc_partial_fill_then_rest(&mut ctx, (2 * FILL_VALUE) as i64);
    let ret = result.expect(
        "with `T` resolved once, from the pre-fill last_traded, this wallet covers the fill plus \
         the rest — the order must be accepted",
    );
    let order_id: [u8; 32] = ret[..32].try_into().unwrap();

    // The fills stand, on BOTH sides.
    assert_eq!(
        after.positions[1].1.amount,
        -(QTY as i64),
        "the taker keeps the lot it filled"
    );
    assert_eq!(
        after.positions[0].1.amount, QTY as i64,
        "and so does the maker (mirror of the taker assertion)"
    );
    assert!(
        !after.orders_present[0].1,
        "the maker's bid was consumed by a trade that COMMITTED"
    );

    // …and the remainder really RESTS: book, BBO cache and `Ask` aggregate all moved.
    assert_eq!(
        after.ask_prices,
        vec![PRICE],
        "the remainder must be in the book"
    );
    assert_eq!(after.best.1, PRICE, "and in the BBO cache");
    assert_eq!(
        (
            after.positions[1].1.total_sell_qty,
            after.positions[1].1.total_sell_notional
        ),
        (QTY, FILL_VALUE),
        "`Ask` carries the remainder at its FROZEN assuming price — the limit price, because the \
         pre-fill floor is the mark and equals it here"
    );
    assert_eq!(
        after.sell_entries[1].1.len(),
        1,
        "and it landed on the taker's own order list"
    );

    // No cancellation of any kind: the order rested, it was not shed.
    assert_eq!(
        band_cancel_reasons(&mut ctx),
        Vec::<u8>::new(),
        "an accepted rest emits no OrderCancelled"
    );
    // Still live (PartiallyFilled), so `getOrder` finds it.
    assert!(
        storage::load_order(&mut ctx, &order_id).unwrap().is_some(),
        "a resting remainder keeps its order record"
    );
}

/// One quantum below the requirement: the whole order rejects, and the maker's bid SURVIVES.
///
/// The mirror of `a_refused_crossing_gtc_remainder_rolls_back_its_own_fills` on the margin trigger
/// rather than the band trigger, and the direct replacement for the deleted
/// `an_unaffordable_gtc_remainder_expires_and_keeps_its_fills`: same fixture, opposite resolution.
#[test]
fn an_unaffordable_gtc_remainder_rejects_the_whole_order() {
    let mut ctx = make_ctx();
    let (result, before, after, _bid) =
        sell_gtc_partial_fill_then_rest(&mut ctx, (2 * FILL_VALUE) as i64 - 1);
    let err = result.expect_err("one quantum short of the requirement must reject");
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "unexpected reject reason: {err}"
    );
    assert!(
        !err.to_string().contains("[INVARIANT] "),
        "this is a user error, not an invariant breach: {err}"
    );
    assert_eq!(
        after, before,
        "…and the reject is CLEAN: the maker's bid is still resting and neither position moved"
    );
    assert!(
        after.orders_present[0].1,
        "explicitly: the maker's bid was NOT consumed"
    );
    assert_eq!(
        band_cancel_reasons(&mut ctx),
        Vec::<u8>::new(),
        "a whole-order reject emits no OrderCancelled — the `CancelReason::TakerMarginCover` reuse \
         that broke a downstream projector (OrderCancelled with no OrderRested) is gone"
    );
}

/// The same scenario driven through the FULL CALL SHELL (`run_perp_dex_call`) rather than the
/// handler, which is the only way to exercise the structural guard in `call.rs`.
///
/// Swept over BOTH sides of the affordability boundary, because the two outcomes exercise different
/// halves of the tripwire's contract and only the second one is new:
///
/// * **accepted** (`2 x FILL_VALUE`) — the call must not revert. Nothing for the tripwire to see.
/// * **refused** (`2 x FILL_VALUE − 1`) — the call MUST revert, and the tripwire must stay silent
///   anyway, i.e. the revert carried ZERO perp writes. That is the clean-reject path the hoist
///   created, and it is exactly the shape the tripwire exists to catch: a reverting `placeOrder`
///   that had written the overlay. It ticks on `writes_after != writes_before` with no bookkeeping
///   of its own, so passing here means the barrier really is below the decision.
///
/// The tripwire is also a `debug_assert!`, so a regression fails INSIDE the shell (naming the
/// offending selector) before this assertion is even reached — which is the difference between a
/// guard and the silent witness that let the val0 leak ship.
#[test]
fn the_val0_scenario_through_the_call_shell_never_trips_the_tripwire() {
    let _guard = lock_tripwire_counter();
    for (available, must_revert) in [
        ((2 * FILL_VALUE) as i64, false),
        (2 * FILL_VALUE as i64 - 1, true),
    ] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        set_mark(&mut ctx, PRICE);
        let _bid = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        set_available(&mut ctx, BOB, available);

        let (trips_before, _) = crate::call::last_perp_write_then_revert();
        let input = placeOrderCall {
            marketId: MARKET_ID,
            side: 1,
            price: PRICE,
            quantity: QTY * 2,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode();
        let out = run_perp_dex_call(&input, 1_000_000, BOB, U256::ZERO, false, &mut ctx).unwrap();

        assert_eq!(
            out.reverted,
            must_revert,
            "available={available}: expected reverted={must_revert}, reason = {:?}",
            String::from_utf8_lossy(&out.bytes)
        );
        let (trips_after, sel) = crate::call::last_perp_write_then_revert();
        assert_eq!(
            trips_after, trips_before,
            "available={available}: commit-only #23 tripwire ticked (last selector {sel:#010x}) — \
             a reverting call committed perp writes"
        );
    }
}

// ── The buy/sell MIRROR DIFFERENTIAL ──────────────────────────────────────────────────────────
//
// `rest_in_book`'s two side arms used to be ~165 lines of verbatim duplication apiece and there was
// no test that compared them, so an asymmetry would have been silent (most tests drive one side).
// The atomic match+rest hoist collapsed the DECISION into one side-generic block, which removes most
// of that surface — but the freeze (`price` vs `price.max(T)`), the four aggregate adds and PHASE 2's
// four book writes are still per-side, and those are exactly what this pins.
//
// The method is a differential, not two hand-written expectations: each side is run and CANONICALISED
// into a side-independent record, and the two records must be equal. A hand-written pair could be
// wrong in the same way twice; a canonical comparison cannot.

/// One side's outcome, with the side rotated out of it: "own" = the side the taker rests on.
#[derive(Debug, PartialEq)]
struct MirrorRecord {
    accepted: bool,
    reject_reason: String,
    own_side_prices: Vec<u64>,
    opposite_side_prices: Vec<u64>,
    own_best: u64,
    opposite_best: u64,
    /// `|amount|` and its sign relative to the taker's side (+1 = the taker's exposure grew in the
    /// direction it traded), so a long and a short compare equal.
    abs_amount: u64,
    signed_in_trade_direction: i8,
    own_qty: u64,
    own_notional: u64,
    other_qty: u64,
    other_notional: u64,
    taker_own_list_len: usize,
    maker_order_alive: bool,
    cancel_reasons: Vec<u8>,
}

/// Runs the mirrored fixture for one taker side and canonicalises the result.
///
/// A crossing GTC for 2 lots against one resting lot at the same price: 1 fills, 1 wants to rest.
/// `available` is the taker's derived available balance, which is where the two legs differ.
fn mirror_leg(taker_side: u8, available: i64) -> MirrorRecord {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    set_mark(&mut ctx, PRICE);
    let maker_side = 1 - taker_side;
    let maker = place(&mut ctx, ALICE, maker_side, PRICE, QTY, 0, 0);
    // PRECONDITION: no trade yet, so the Assuming-Price floor is the MARK on both legs and the two
    // sides' requirements really are equal (a stale `last_traded` would mark the SELL leg up only).
    assert_eq!(
        storage::load_market_hot(&mut ctx, MARKET_ID)
            .unwrap()
            .last_traded,
        0,
        "the differential is only meaningful while the sell-side markup is inert"
    );
    set_available(&mut ctx, BOB, available);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: taker_side,
        price: PRICE,
        quantity: QTY * 2,
        orderType: 0,
        tif: 0, // GTC — the one kind with a rest phase
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let result = run_place_order(&input, BOB, &mut ctx);

    let bids = storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap();
    let asks = storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap();
    let hot = storage::load_market_hot(&mut ctx, MARKET_ID).unwrap();
    let p = pos(&mut ctx, BOB);
    let buys = storage::load_buy_orders(&mut ctx, BOB, MARKET_ID).unwrap();
    let sells = storage::load_sell_orders(&mut ctx, BOB, MARKET_ID).unwrap();
    let taker_is_buy = taker_side == 0;
    MirrorRecord {
        accepted: result.is_ok(),
        reject_reason: result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default(),
        own_side_prices: if taker_is_buy {
            bids.clone()
        } else {
            asks.clone()
        },
        opposite_side_prices: if taker_is_buy { asks } else { bids },
        own_best: if taker_is_buy {
            hot.best_bid
        } else {
            hot.best_ask
        },
        opposite_best: if taker_is_buy {
            hot.best_ask
        } else {
            hot.best_bid
        },
        abs_amount: p.amount.unsigned_abs(),
        signed_in_trade_direction: match (p.amount.signum(), taker_is_buy) {
            (0, _) => 0,
            (1, true) | (-1, false) => 1,
            _ => -1,
        },
        own_qty: if taker_is_buy {
            p.total_buy_qty
        } else {
            p.total_sell_qty
        },
        own_notional: if taker_is_buy {
            p.total_buy_notional
        } else {
            p.total_sell_notional
        },
        other_qty: if taker_is_buy {
            p.total_sell_qty
        } else {
            p.total_buy_qty
        },
        other_notional: if taker_is_buy {
            p.total_sell_notional
        } else {
            p.total_buy_notional
        },
        taker_own_list_len: if taker_is_buy {
            buys.len()
        } else {
            sells.len()
        },
        maker_order_alive: storage::load_order(&mut ctx, &maker).unwrap().is_some(),
        cancel_reasons: band_cancel_reasons(&mut ctx),
    }
}

/// **The mirror differential.** A crossing GTC's match+rest must behave identically on both sides,
/// on BOTH sides of the affordability boundary.
///
/// Every edit to this path has to be applied twice, and this is the assertion that notices when it
/// was applied once. It compares the two sides against each other rather than against a written-out
/// expectation, so it also catches a *pair* of edits that are individually plausible.
#[test]
fn the_match_and_rest_path_is_mirror_symmetric_between_buy_and_sell() {
    let need = (2 * FILL_VALUE) as i64;

    // ── Leg 1: exactly at the requirement → ACCEPTED, remainder rests. ──
    let buy = mirror_leg(0, need);
    let sell = mirror_leg(1, need);
    assert!(
        buy.accepted,
        "precondition: the acceptance leg must accept (buy), reason = {}",
        buy.reject_reason
    );
    assert_eq!(
        buy, sell,
        "BUY and SELL diverged on an ACCEPTED crossing GTC — the rest path is not symmetric"
    );
    // …and pin what "accepted" means, so the differential cannot pass by both sides doing nothing.
    assert_eq!(
        (
            buy.own_side_prices.as_slice(),
            buy.own_best,
            buy.abs_amount,
            buy.signed_in_trade_direction,
            buy.own_qty,
            buy.taker_own_list_len,
            buy.maker_order_alive
        ),
        (&[PRICE][..], PRICE, QTY, 1, QTY, 1, false),
        "the remainder must really rest and the maker must really have been filled"
    );

    // ── Leg 2: one quantum short → REJECTED, cleanly, with the same reason on both sides. ──
    let buy = mirror_leg(0, need - 1);
    let sell = mirror_leg(1, need - 1);
    assert!(
        !buy.accepted,
        "precondition: the refusal leg must refuse (buy)"
    );
    assert_eq!(
        buy, sell,
        "BUY and SELL diverged on a REFUSED crossing GTC — the reject is not symmetric"
    );
    assert_eq!(
        (
            buy.reject_reason.as_str(),
            buy.own_side_prices.as_slice(),
            buy.own_best,
            buy.abs_amount,
            buy.own_qty,
            buy.taker_own_list_len,
            buy.maker_order_alive,
            buy.cancel_reasons.as_slice()
        ),
        (
            "placeOrder: insufficient perp wallet for margin",
            &[][..],
            0,
            0,
            0,
            0,
            true,
            &[][..]
        ),
        "a refusal must leave the book, the taker's position and the MAKER'S ORDER untouched, and \
         emit no OrderCancelled"
    );
}

// ── The pre-walk conservative early-out ───────────────────────────────────────────────────────
//
// `reject_provably_unaffordable_limit_order` is a FILTER, not a decision: its only obligation is
// `bound <= true_requirement`, so that an early reject can never be wrong. These tests are about the
// two directions that obligation splits into — it must never reject something the authoritative
// gates would accept (the guards), and it must actually fire ahead of the walk (the ordering).
//
// The ORDERING is observable without instrumentation, via a trick worth stating once: for a limit
// order that is BOTH unaffordable and independently rejectable by a walk-based check, the reject
// MESSAGE says which check ran first. `check_fok_feasibility` walks the book and reports "FOK order
// cannot be fully filled"; the early-out reports "insufficient perp wallet for margin". So the
// message is a direct witness to whether the walk happened.
mod pre_walk_early_out {
    use super::*;

    const FOK_REJECT: &str = "FOK order cannot be fully filled";
    const MARGIN_REJECT: &str = "insufficient perp wallet for margin";

    fn try_place(
        ctx: &mut TestCtx,
        caller: Address,
        side: u8,
        price: u64,
        qty: u64,
        tif: u8,
    ) -> Result<Bytes, PerpError> {
        let input = placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: 0, // Limit
            tif,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode();
        run_place_order(&input, caller, ctx)
    }

    /// An unfillable FOK for `qty` at `PRICE`, on `side`.
    fn unfillable_fok(ctx: &mut TestCtx, caller: Address, side: u8, qty: u64) -> PerpError {
        try_place(ctx, caller, side, PRICE, qty, 2).expect_err("an unfillable FOK must reject")
    }

    /// **⚠️ THE TRAP.** A position-CLOSING order must never be early-rejected, however broke the
    /// account is.
    ///
    /// A reducing order's true requirement is zero or NEGATIVE — closing frees `pos.margin` and
    /// realises PnL into the wallet — and `pos.margin` is unbounded above (`addPositionMargin`), so
    /// no fixed credit could make a positive bound safe. A false reject here is much worse than a
    /// wasted walk: it locks a user out of de-risking at the moment they most need to.
    ///
    /// Both directions, and both resolutions (rest and cross), because the guard is per-side.
    #[test]
    fn a_closing_order_is_never_early_rejected_however_broke_the_account_is() {
        for open_side in [0u8, 1u8] {
            let close_side = 1 - open_side;
            for crossing in [false, true] {
                let mut ctx = make_ctx();
                setup(&mut ctx);
                set_mark(&mut ctx, PRICE);
                // BOB takes a position by crossing ALICE.
                place(&mut ctx, ALICE, close_side, PRICE, QTY, 0, 0);
                place(&mut ctx, BOB, open_side, PRICE, QTY, 0, 1); // IOC → never rests
                let signed = if open_side == 0 { 1i64 } else { -1 };
                assert_eq!(
                    pos(&mut ctx, BOB).amount,
                    signed * QTY as i64,
                    "precondition: BOB must hold the position the close will reduce"
                );
                // As broke as the fixture can make it — far deeper than anything the close releases.
                set_available(&mut ctx, BOB, -50 * FILL_VALUE as i64);
                if crossing {
                    // Someone to close against, so the close FILLS instead of resting.
                    place(&mut ctx, ALICE, open_side, PRICE, QTY, 0, 0);
                }

                try_place(&mut ctx, BOB, close_side, PRICE, QTY, 0).unwrap_or_else(|e| {
                    panic!(
                        "open_side={open_side} crossing={crossing}: a closing order must never be \
                         refused for margin — the pre-walk bound must not apply to it: {e}"
                    )
                });
                if crossing {
                    assert_eq!(
                        pos(&mut ctx, BOB).amount,
                        0,
                        "open_side={open_side}: the close really closed"
                    );
                }
            }
        }
    }

    /// A hedged order whose true requirement is ZERO must not be early-rejected either.
    ///
    /// `ooIM = max(|N + Bid|, |N − Ask|)/L − |N|/L`, so a bid that fits under an existing ask costs
    /// nothing. The bound is not hand-rolled arithmetic precisely so it inherits that `max()` — this
    /// is the test that would fail if it were ever replaced by "notional / leverage".
    #[test]
    fn a_hedged_order_with_a_zero_requirement_is_not_early_rejected() {
        for (resting_side, probe_side) in [(1u8, 0u8), (0u8, 1u8)] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            set_mark(&mut ctx, PRICE);
            // TWICE the probe's size, so the probe's notional stays strictly under it even after
            // the one-tick offset below — that is what makes the `max()` unmoved and the
            // requirement exactly 0.
            place(&mut ctx, BOB, resting_side, PRICE, QTY * 2, 0, 0);
            // Nothing spendable left at all.
            set_available(&mut ctx, BOB, 0);
            // One tick AWAY on the probe's own side, so it cannot self-match BOB's resting order.
            let probe_price = if probe_side == 0 {
                PRICE - TICK
            } else {
                PRICE + TICK
            };
            try_place_limit(&mut ctx, BOB, probe_side, probe_price).unwrap_or_else(|e| {
                panic!(
                    "resting_side={resting_side}: a free hedged order must be admitted at zero \
                     available: {e}"
                )
            });
        }
    }

    /// **It fires, and it fires BEFORE the walk.** The FOK message is the witness: reaching
    /// `check_fok_feasibility` would have produced it, and it did not.
    #[test]
    fn a_provably_unaffordable_fok_is_refused_without_walking_the_book() {
        for side in [0u8, 1u8] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            set_mark(&mut ctx, PRICE);
            set_available(&mut ctx, BOB, 0);
            // 11 lots against an EMPTY book: unfillable AND unaffordable at once.
            let err = unfillable_fok(&mut ctx, BOB, side, QTY * 11);
            assert!(
                err.to_string().contains(MARGIN_REJECT),
                "side={side}: expected the pre-walk margin reject, got {err}"
            );
            assert!(
                !err.to_string().contains(FOK_REJECT),
                "side={side}: the FOK feasibility WALK ran — the early-out did not precede it: {err}"
            );
        }
    }

    /// Guard 1 is really a guard: with an offsetting position the filter stands down, so the
    /// walk-based check runs and reports the FOK message.
    ///
    /// The mirror of the previous test on the same fixture, which is what makes it a differential on
    /// the guard rather than an assertion about a message.
    #[test]
    fn an_offsetting_position_stands_the_filter_down() {
        for open_side in [0u8, 1u8] {
            let close_side = 1 - open_side;
            let mut ctx = make_ctx();
            setup(&mut ctx);
            set_mark(&mut ctx, PRICE);
            place(&mut ctx, ALICE, close_side, PRICE, QTY, 0, 0);
            place(&mut ctx, BOB, open_side, PRICE, QTY, 0, 1);
            set_available(&mut ctx, BOB, 0);
            // A REDUCING FOK, unfillable (the book is empty again).
            let err = unfillable_fok(&mut ctx, BOB, close_side, QTY * 11);
            assert!(
                err.to_string().contains(FOK_REJECT),
                "open_side={open_side}: the filter must stand down for a reducing order, so the \
                 walk-based FOK check should be what rejects: {err}"
            );
        }
    }

    /// Guard 2, the same way: a resting order on the order's OWN side stands the filter down (it is
    /// what the cover loop would cancel, and a cancel can shrink the baseline the bound assumes).
    #[test]
    fn an_own_side_resting_order_stands_the_filter_down() {
        for side in [0u8, 1u8] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            set_mark(&mut ctx, PRICE);
            // A same-side resting order, placed while BOB can still afford it…
            let away = if side == 0 {
                PRICE - 10 * TICK
            } else {
                PRICE + 10 * TICK
            };
            place(&mut ctx, BOB, side, away, QTY, 0, 0);
            // …then drain the account.
            set_available(&mut ctx, BOB, 0);
            let err = unfillable_fok(&mut ctx, BOB, side, QTY * 11);
            assert!(
                err.to_string().contains(FOK_REJECT),
                "side={side}: with an own-side resting order the filter must stand down: {err}"
            );
        }
    }

    /// **The bound never cuts into the accepted region.** At EXACTLY the requirement the order is
    /// still admitted; one quantum below, it is refused. That the boundary is unmoved is the
    /// operative half — a filter that shaved even one quantum off it would fail here.
    ///
    /// (The crossing case is pinned by
    /// `the_match_and_rest_path_is_mirror_symmetric_between_buy_and_sell`, whose acceptance leg sits
    /// exactly on the boundary and whose orders satisfy both guards.)
    #[test]
    fn the_bound_does_not_move_the_acceptance_boundary_of_a_pure_rest() {
        for side in [0u8, 1u8] {
            for (target, expect_ok) in [(FILL_VALUE as i64, true), (FILL_VALUE as i64 - 1, false)] {
                let mut ctx = make_ctx();
                setup(&mut ctx);
                set_mark(&mut ctx, PRICE);
                set_available(&mut ctx, BOB, target);
                let result = try_place_limit(&mut ctx, BOB, side, PRICE);
                assert_eq!(
                    result.is_ok(),
                    expect_ok,
                    "side={side} available={target}: expected ok={expect_ok}, got {result:?}"
                );
            }
        }
    }
}

/// **Proof the tripwire still BITES.**
///
/// A guard that cannot be made to fail is indistinguishable from a comment, and the val0 leak
/// shipped precisely because this counter was diagnostic-only. So this drives a REAL write-then-
/// revert through the shell and requires the `debug_assert!` in `call.rs` to fire.
///
/// The vehicle is the one genuine post-barrier failure the engine still has, the same one
/// `batch_place::arm_post_write_place_abort` uses: a non-zero taker fee with the fee recipient
/// UNSET, so `credit_fee_recipient` fails INSIDE `MatchOutcome::apply`, after
/// `MatchRegistry::flush`. (It is unreachable on a live chain — a market cannot be added without a
/// non-zero admin and neither `initAdmin` nor `transferAdmin` can zero one — which is exactly why
/// it is the right thing to point the guard at in a test.)
///
/// `#[should_panic]` on the assertion's own text: if the barrier is ever moved back above a reject,
/// or the `debug_assert!` is downgraded to a counter bump again, THIS test fails.
#[test]
#[should_panic(expected = "commit-only #23 VIOLATED")]
fn the_tripwire_still_bites_on_a_real_write_then_revert() {
    let _guard = lock_tripwire_counter();
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_user_fee_rates(
        &mut ctx,
        ALICE,
        crate::types::UserFeeRates {
            maker_fee_bps: 0,
            taker_fee_bps: 100,
        },
    )
    .unwrap();
    // A maker for ALICE to cross, so the taker really settles a fill (and therefore a fee).
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    storage::save_admin(&mut ctx, Address::ZERO).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let _ = run_perp_dex_call(&input, 1_000_000, ALICE, U256::ZERO, false, &mut ctx);
}

/// The zero-fill half of the same fork STILL rejects, and must: `match_order` never flushed, so a
/// reject there is write-clean and is the better answer (the client learns the order was refused
/// rather than silently getting nothing back). Pins that the fix did not convert clean rejects into
/// silent expiries.
#[test]
fn a_zero_fill_unaffordable_rest_still_rejects() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    set_mark(&mut ctx, PRICE);
    // No maker, so nothing crosses: the whole order wants to rest.
    set_available(&mut ctx, BOB, 0);
    let before = snapshot_perp_state(&mut ctx, &[ALICE, BOB], &[]);
    let err = try_place_limit(&mut ctx, BOB, 1, PRICE).expect_err(
        "a rest that cannot afford its own margin, with zero fills behind it, must still reject",
    );
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "unexpected reject reason: {err}"
    );
    assert!(
        !err.to_string().contains("[INVARIANT] "),
        "this is a user error, not an invariant breach: {err}"
    );
    let after = snapshot_perp_state(&mut ctx, &[ALICE, BOB], &[]);
    assert_eq!(after, before, "and it must still be write-clean");
}

#[test]
fn fill_band_blocks_buy_against_ask_above_band() {
    // mark 100, default +-10% -> upper edge 110. An ask at 120 rests (FAR side, so placement lets
    // it), but a crossing buy must NOT fill it: the fill-time band ends matching at the first ask
    // above mark+band.
    //
    // The taker is IOC, and structurally has to be: crossing an ask ABOVE the upper edge requires a
    // limit above that edge too, so a GTC's remainder would be a too-good new best and be refused
    // at placement (`a_crossing_gtc_remainder_is_refused_as_a_too_good_best`). IOC drops the
    // remainder, leaving the fill-time band as the only thing this fixture exercises.
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    let ask = try_place_limit(&mut ctx, ALICE, 1, 120 * TICK).unwrap();
    let ask: [u8; 32] = ask[..32].try_into().unwrap();
    assert_out_of_band_best(&mut ctx, 1, 120 * TICK);
    let taker = try_place_limit_tif(&mut ctx, BOB, 0, 200 * TICK, 1).unwrap(); // IOC, crosses 120
    let taker: [u8; 32] = taker[..32].try_into().unwrap();
    assert_eq!(
        get_order(&mut ctx, ask).filled,
        0,
        "out-of-band ask must not fill"
    );
    assert_eq!(
        pos(&mut ctx, BOB).amount,
        0,
        "buy taker must not fill above the band"
    );
    assert_terminal(&mut ctx, taker); // IOC remainder expired, so it rested nothing

    // Non-vacuity: the SAME taker fills once the band is widened to contain the level, so "did not
    // fill" above is the band's doing and not a crossing/affordability accident.
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 3_000); // +-30% -> upper edge 130, so 120 is IN band
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    let _ = try_place_limit(&mut ctx, ALICE, 1, 120 * TICK).unwrap();
    let _ = try_place_limit_tif(&mut ctx, BOB, 0, 200 * TICK, 1).unwrap();
    assert!(
        pos(&mut ctx, BOB).amount > 0,
        "control: the same crossing IOC DOES fill when the level is inside the band"
    );
}

#[test]
fn fill_band_skips_buy_against_ask_below_band() {
    // An ask below the LOWER edge (e.g. a closing maker dumping cheap): a buy taker must SKIP it
    // rather than seize the off-mark price (hole-#1 direction).
    //
    // Reached by MARK DRIFT, because that is now the only way: an ask below the lower edge is a
    // too-good quote, and a too-good ask is by definition the best ask, so placing one directly is
    // refused. So it is placed IN band and the mark is then moved out from under it — which is also
    // the faithful scenario (`save_mark_price`, not the oracle: an oracle update would run the
    // band-expiry GC and delete the very level under test).
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap(); // mark $100 => [$90, $110]
    let ask = try_place_limit(&mut ctx, ALICE, 1, 95 * TICK).unwrap(); // IN band at placement
    let ask: [u8; 32] = ask[..32].try_into().unwrap();
    storage::save_mark_price(&mut ctx, MARKET_ID, 200 * TICK).unwrap(); // => [$180, $220]
    assert_out_of_band_best(&mut ctx, 1, 95 * TICK);

    // The taker's own limit is INSIDE the new band, so nothing about the taker is out of band — the
    // only off-mark thing in the call is the maker level, which is the point.
    let taker_price = 200 * TICK;
    let (upper, lower) = band(&mut ctx);
    assert!(
        (taker_price as u128) >= lower && (taker_price as u128) <= upper,
        "precondition: the taker itself is in band"
    );
    assert!(
        taker_price >= 95 * TICK,
        "precondition: and it crosses the stranded ask in PRICE, so only the band can stop it"
    );
    let _ = try_place_limit(&mut ctx, BOB, 0, taker_price).unwrap();
    assert_eq!(
        get_order(&mut ctx, ask).filled,
        0,
        "off-mark-cheap ask must be skipped"
    );
    assert_eq!(
        pos(&mut ctx, BOB).amount,
        0,
        "buy taker must not fill below the band"
    );
}

#[test]
fn fill_band_blocks_sell_against_bid_below_band() {
    // mark 100, lower edge 90. A bid at 40 rests (FAR side), but a crossing sell must NOT fill it.
    // IOC taker for the same structural reason as `fill_band_blocks_buy_against_ask_above_band`.
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    let bid = try_place_limit(&mut ctx, ALICE, 0, 40 * TICK).unwrap();
    let bid: [u8; 32] = bid[..32].try_into().unwrap();
    assert_out_of_band_best(&mut ctx, 0, 40 * TICK);
    let taker = try_place_limit_tif(&mut ctx, BOB, 1, TICK, 1).unwrap(); // IOC, crosses 40
    let taker: [u8; 32] = taker[..32].try_into().unwrap();
    assert_eq!(
        get_order(&mut ctx, bid).filled,
        0,
        "out-of-band bid must not fill"
    );
    assert_eq!(
        pos(&mut ctx, BOB).amount,
        0,
        "sell taker must not fill below the band"
    );
    assert_terminal(&mut ctx, taker);

    // Non-vacuity control: put the band's CENTRE on 40 instead of widening it, and the same
    // crossing IOC sell fills. (Widening the band cannot be the control on this side: a short
    // opened 60% below mark is instantly under maintenance and gets refused by a different gate —
    // `taker_open_below_maintenance_is_rejected` — which would prove nothing about the band.)
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, 40 * TICK).unwrap(); // => [$36, $44]
    let _ = try_place_limit(&mut ctx, ALICE, 0, 40 * TICK).unwrap();
    let _ = try_place_limit_tif(&mut ctx, BOB, 1, TICK, 1).unwrap();
    assert!(
        pos(&mut ctx, BOB).amount < 0,
        "control: the same crossing IOC DOES fill when the level is inside the band"
    );
}

#[test]
fn fill_band_skips_sell_against_bid_above_band() {
    // A bid above the UPPER edge (e.g. a closing maker buying rich): a sell taker must SKIP it
    // (hole-#1 direction). MARK DRIFT for the same reason as
    // `fill_band_skips_buy_against_ask_below_band` — a too-good bid is always the best bid, so it
    // cannot be placed directly.
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap(); // mark $100 => [$90, $110]
    let bid = try_place_limit(&mut ctx, ALICE, 0, 105 * TICK).unwrap(); // IN band at placement
    let bid: [u8; 32] = bid[..32].try_into().unwrap();
    storage::save_mark_price(&mut ctx, MARKET_ID, 50 * TICK).unwrap(); // => [$45, $55]
    assert_out_of_band_best(&mut ctx, 0, 105 * TICK);

    let taker_price = 50 * TICK;
    let (upper, lower) = band(&mut ctx);
    assert!(
        (taker_price as u128) >= lower && (taker_price as u128) <= upper,
        "precondition: the taker itself is in band"
    );
    assert!(
        taker_price <= 105 * TICK,
        "precondition: and it crosses the stranded bid in PRICE"
    );
    let _ = try_place_limit(&mut ctx, BOB, 1, taker_price).unwrap();
    assert_eq!(
        get_order(&mut ctx, bid).filled,
        0,
        "off-mark-rich bid must be skipped"
    );
    assert_eq!(
        pos(&mut ctx, BOB).amount,
        0,
        "sell taker must not fill above the band"
    );
}

#[test]
fn fill_band_allows_fill_at_edge() {
    // The band edge is inclusive: an ask at exactly mark+10% (110) fills a crossing buy.
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 0);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    let _ = try_place_limit(&mut ctx, ALICE, 1, 110 * TICK).unwrap(); // ask at edge
    let _ = try_place_limit(&mut ctx, BOB, 0, 110 * TICK).unwrap(); // buy fills at edge
    assert!(
        pos(&mut ctx, BOB).amount > 0,
        "buy must fill at the band edge"
    );
    assert!(
        pos(&mut ctx, ALICE).amount < 0,
        "maker sell must fill at the band edge"
    );
}

#[test]
fn fill_band_honors_configured_bps() {
    // Explicit 500 bps (+-5%) -> upper edge 105. An ask at 106 is out of band (no fill).
    // IOC taker: 106 is past the upper edge, so any limit that crosses it is too, and a GTC's
    // remainder would be refused as a too-good best.
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 500);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    let ask = try_place_limit(&mut ctx, ALICE, 1, 106 * TICK).unwrap();
    let ask: [u8; 32] = ask[..32].try_into().unwrap();
    // Precondition: 106 is out of band under the CONFIGURED 500 bps and would be IN band under the
    // 1_000 bps default — i.e. this fixture really is testing the configured width, which is the
    // whole point of it and is exactly what a stray `price_band_bps` change would silently undo.
    assert_out_of_band_best(&mut ctx, 1, 106 * TICK);
    let (default_upper, _) =
        crate::math::mark_band_bounds(PRICE, crate::math::DEFAULT_PRICE_BAND_BPS);
    assert!(
        (106 * TICK) as u128 <= default_upper,
        "precondition: 106 would be INSIDE the default band, so only the configured 500 bps \
         excludes it"
    );
    let _ = try_place_limit_tif(&mut ctx, BOB, 0, 200 * TICK, 1).unwrap(); // IOC
    assert_eq!(
        get_order(&mut ctx, ask).filled,
        0,
        "ask just outside the 5% band must not fill"
    );

    // Fresh ctx: an ask at the 5% edge (105) fills.
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 500);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    let _ = try_place_limit(&mut ctx, ALICE, 1, 105 * TICK).unwrap();
    let _ = try_place_limit(&mut ctx, BOB, 0, 105 * TICK).unwrap();
    assert!(
        pos(&mut ctx, BOB).amount > 0,
        "ask at the 5% edge must fill"
    );
}

#[test]
fn taker_open_below_maintenance_is_rejected() {
    // Disabled band so we can construct an off-mark fill; mark = 100 ticks.
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 1_000_000);
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    // ALICE rests a bid far below mark (50 ticks) — allowed (band disabled).
    assert!(
        try_place_limit(&mut ctx, ALICE, 0, 50 * TICK).is_ok(),
        "resting bid should be accepted"
    );
    // BOB sells into it: opens a short at 50 while mark is 100 -> position_value
    // = -notional + vquote + margin = 0, below the 1/6 maintenance threshold ->
    // the taker open-solvency guard reverts the whole order.
    let err = try_place_limit(&mut ctx, BOB, 1, 50 * TICK).unwrap_err();
    assert!(
        err.to_string().contains("breach maintenance margin"),
        "{err}"
    );
}

#[test]
fn maker_open_below_maintenance_is_cancelled_not_filled() {
    let mut ctx = make_ctx();
    setup_banded(&mut ctx, 1_000_000); // disabled band so the off-mark ask can rest
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap(); // mark = 100 ticks
                                                                   // ALICE rests an ask far below mark (50 ticks). Filling it would open ALICE a
                                                                   // short at 50 while mark is 100 -> equity 0, below the 1/6 maintenance threshold.
    let alice_ask = try_place_limit(&mut ctx, ALICE, 1, 50 * TICK).unwrap();
    let alice_ask: [u8; 32] = alice_ask[..32].try_into().unwrap();
    // BOB buys into it: the maker open-solvency guard rejects ALICE's fill, so her
    // order is cancelled and BOB matches nothing (his order rests instead).
    let _ = try_place_limit(&mut ctx, BOB, 0, 50 * TICK).unwrap();
    // rejected maker order must be cancelled → deleted under delete-on-terminal.
    assert_terminal(&mut ctx, alice_ask);
    assert_eq!(
        pos(&mut ctx, ALICE).amount,
        0,
        "maker must not have opened an insolvent position"
    );
    assert_eq!(
        pos(&mut ctx, BOB).amount,
        0,
        "taker must not have filled against the rejected maker"
    );
}

#[test]
fn position_registry_tracks_open_positions() {
    let mut ctx = make_ctx();
    let m = MARKET_ID;
    let u1 = user_addr(1);
    let u2 = user_addr(2);
    let open = |amt: i64| PerpPosition {
        amount: amt,
        ..PerpPosition::default()
    };

    // Opening (0 -> !=0) adds to the registry.
    storage::save_position(&mut ctx, u1, m, &open(5), AccountUpdateReason::Adjustment).unwrap();
    assert_eq!(
        storage::load_position_registry(&mut ctx, m).unwrap(),
        vec![u1]
    );
    // A second holder appends (insertion order preserved).
    storage::save_position(&mut ctx, u2, m, &open(-3), AccountUpdateReason::Adjustment).unwrap();
    assert_eq!(
        storage::load_position_registry(&mut ctx, m).unwrap(),
        vec![u1, u2]
    );
    // Same-membership save (amount changes sign but stays !=0, e.g. a flip) — no dup.
    storage::save_position(&mut ctx, u1, m, &open(-7), AccountUpdateReason::Adjustment).unwrap();
    assert_eq!(
        storage::load_position_registry(&mut ctx, m).unwrap(),
        vec![u1, u2]
    );
    // Closing (!=0 -> 0) removes, preserving the order of the rest.
    storage::save_position(&mut ctx, u1, m, &open(0), AccountUpdateReason::Adjustment).unwrap();
    assert_eq!(
        storage::load_position_registry(&mut ctx, m).unwrap(),
        vec![u2]
    );
    // Closing the last holder empties the registry (key deleted).
    storage::save_position(&mut ctx, u2, m, &open(0), AccountUpdateReason::Adjustment).unwrap();
    assert!(storage::load_position_registry(&mut ctx, m)
        .unwrap()
        .is_empty());
}

#[test]
fn rejects_quantity_not_multiple_of_step_size() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY + 1,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("step_size"), "{err}");
}

#[test]
fn rejects_limit_order_with_zero_price() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: 0,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("limit order price must be > 0"),
        "{err}"
    );
}

#[test]
fn rejects_price_not_multiple_of_tick_size() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE + 1,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("tick_size"), "{err}");
}

// ── Resting orders & margin reservation ───────────────────────────────────

#[test]
fn limit_buy_rests_in_book_when_no_ask() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // GTC limit buy
    assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open);
    assert_eq!(
        storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![PRICE]
    );
}

#[test]
fn limit_sell_rests_in_book_when_no_bid() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let id = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // GTC limit sell
    assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open);
    assert_eq!(
        storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![PRICE]
    );
}

/// CHANGED BY THE ESCROW REMOVAL (mechanism, not size). The order still commits `INIT_MARGIN` of
/// the account, but the WALLET no longer moves: the commitment is the derived requirement
/// `ooIM = ROUND_UP(Bid / L)`, subtracted from the available on read.
#[test]
fn resting_buy_commits_open_order_margin_without_debiting_the_wallet() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    // Flat position ⇒ N = 0 ⇒ ooIM = ROUND_UP(Bid / 1) = calc_value(PRICE, QTY, 8, 9).
    assert_eq!(oo_im(&mut ctx, ALICE), INIT_MARGIN);
    assert_eq!(
        wallet(&mut ctx, ALICE),
        WALLET,
        "resting escrows NOTHING — not the margin, and not the prospective maker fee"
    );
    assert_eq!(available(&mut ctx, ALICE), (WALLET - INIT_MARGIN) as i128);
}

/// Sell-side twin of the above.
#[test]
fn resting_sell_commits_open_order_margin_without_debiting_the_wallet() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);

    assert_eq!(oo_im(&mut ctx, BOB), INIT_MARGIN);
    assert_eq!(wallet(&mut ctx, BOB), WALLET);
    assert_eq!(available(&mut ctx, BOB), (WALLET - INIT_MARGIN) as i128);
}

#[test]
fn margin_uses_market_price_decimals() {
    let mut ctx = make_ctx();
    storage::save_market(
        &mut ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 8,
            price_decimals: 2,
            tick_size: 1,
            step_size: 100_000_000,
            min_quantity: 100_000_000,
            max_quantity: 100_000_000,
            max_price: 1_000_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 0,
            mark_price: 0,
            tiers: MarginTiers::default(),
        },
    )
    .unwrap();
    fund(&mut ctx, ALICE, 200_000_000);

    let price = 12_345; // $123.45 with price_decimals = 2.
    let qty = 100_000_000; // 1 base unit with base_decimals = 8.
    let expected_margin = 123_450_000; // $123.45 in 6-decimal quote units.

    place(&mut ctx, ALICE, 0, price, qty, 0, 0);

    assert_eq!(oo_im(&mut ctx, ALICE), expected_margin);
    // CHANGED BY THE ESCROW REMOVAL: the wallet is untouched; the charge lands on `available`.
    assert_eq!(wallet(&mut ctx, ALICE), 200_000_000);
    assert_eq!(
        available(&mut ctx, ALICE),
        (200_000_000 - expected_margin) as i128
    );
}

#[test]
fn consecutive_orders_have_unique_ids() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let id1 = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let id2 = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert_ne!(id1, id2);
}

// ── Matching ───────────────────────────────────────────────────────────────

#[test]
fn buy_taker_fully_matches_resting_ask() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let sell_id = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // resting ask
    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // taker buy

    assert_terminal(&mut ctx, buy_id);
    assert_terminal(&mut ctx, sell_id);
    assert!(storage::load_ask_prices(&mut ctx, MARKET_ID)
        .unwrap()
        .is_empty());
}

/// A crossing call publishes **one `AccountBalanceChanged` per economic event**, in the order those
/// events happen: the maker's at his fill, the taker's after her order settles, and the incidental
/// fee recipient's still coalesced into the end-of-call drain.
///
/// # The three rules, on one call
///
/// | party | rule | where it is published |
/// |---|---|---|
/// | BOB, the maker | one per FILL | inside the match flush, right after his `Trade` row |
/// | ALICE, the taker | one per ORDER | after `finalize_apply` |
/// | ADMIN, the fee recipient | coalesced | the end-of-call drain |
///
/// So the emission order is BOB, ALICE, ADMIN — **not** the ascending ADDRESS order this test used to
/// pin (`ALICE 0x11.. < BOB 0x22.. < ADMIN 0xaa..`), and not the old per-write order either. Address
/// order still governs the DRAIN (`the_drain_order_is_ascending_address_order`), which is now only
/// the third row here; the first two are placed by the events that caused them, which is the whole
/// change. The stream is still deterministic — the walk order, the flush's event replay and the
/// `BTreeSet` drain are all fixed — and that is the property that matters for consensus.
///
/// # What survives from the coalescing era
///
/// **No half-updated state reaches the stream.** The per-write emission published one event per
/// balance-moving write, `[ADMIN, BOB, ALICE, ALICE]`, and ALICE's first was genuinely
/// half-updated — her position silo funded before her wallet had paid for it, so
/// `totalWalletBalance` double-counted `total_required`. That is still gone, but for a different
/// reason in each case: ALICE's snapshot is taken after `finalize_apply` has debited her, and BOB's
/// is derived from the registry working copy the flush is about to write, which is his settled state
/// for that fill rather than a partial write of it. The `WALLET + INIT_MARGIN` assertion below is
/// what pins it.
///
/// **Every event still agrees with `getAccount` field for field**, including BOB's, which is the
/// interesting one: it was computed off a working copy, not off the store. That equality is the
/// user-visible face of `MatchRegistry::flush`'s convergence guard.
#[test]
fn matched_call_publishes_one_snapshot_per_economic_event() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_user_fee_rates(
        &mut ctx,
        BOB,
        UserFeeRates {
            maker_fee_bps: 1,
            taker_fee_bps: 0,
        },
    )
    .unwrap();
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let output = run_perp_dex_call(
        &placeOrderCall {
            marketId: MARKET_ID,
            side: 0,
            price: PRICE,
            quantity: QTY,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode(),
        10_000_000,
        ALICE,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(!output.reverted);
    // Balance after-image events are free: the call is charged only the flat placeOrder gas.
    assert_eq!(output.gas_used, 200_000);

    let events = JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
        .map(|log| {
            AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
        })
        .collect::<Vec<_>>();
    // Three parties, three events, in ECONOMIC-EVENT order: BOB at his fill (inside the flush),
    // ALICE once her order has settled (after `finalize_apply`), ADMIN from the drain. Note this is
    // neither ascending address order (ALICE 0x11.. < BOB 0x22.. < ADMIN 0xaa..) nor the old write
    // order (ADMIN's fee credit lands FIRST of the three writes). Determinism is unaffected — the
    // walk order and the flush replay are fixed and the drain is still a `BTreeSet` — and it is
    // determinism, not any particular order, that consensus needs.
    assert_eq!(
        events.iter().map(|e| e.user).collect::<Vec<_>>(),
        vec![BOB, ALICE, ADMIN],
        "one snapshot per economic event: maker at his fill, taker after her order, fee recipient \
         coalesced into the drain"
    );

    // ── Every published snapshot is a SETTLED state ───────────────────────────────────────────
    //
    // ALICE is written TWICE inside the call (the registry flush saves her position + account, then
    // `finalize_apply` debits `total_required` from her wallet), and under the old per-write emission
    // the first of those was published: `totalWalletBalance = cross + Σ isolatedWallet` counted the
    // funded silo while the wallet had not yet paid for it, over-stating by exactly `total_required`.
    // Publishing hers AFTER `finalize_apply` — rather than at the earlier of the two writes — is what
    // keeps that intermediate out of the stream now that the drain is no longer what emits it.
    let alice = events.iter().find(|e| e.user == ALICE).unwrap();
    assert_eq!(
        alice.totalCrossWalletBalance,
        (WALLET - INIT_MARGIN) as i64,
        "the debit has landed"
    );
    // `Σ isolatedWallet` carries the silo into `totalWalletBalance`, so the settled gross wallet is
    // back to `WALLET`: the money is in the silo instead of the wallet, counted once.
    // (This market's `mark_price` is 0, so the mark-derived totals — PIM, MM — are 0 here by
    // construction; `margin_view_tests` covers them at a live mark.)
    assert_eq!(alice.totalWalletBalance, WALLET as i64);
    assert!(
        events
            .iter()
            .all(|e| e.totalWalletBalance != (WALLET + INIT_MARGIN) as i64),
        "the half-updated pre-debit snapshot must not reach the stream at all"
    );

    // Each event must agree with `getAccount` FIELD FOR FIELD. For ALICE and ADMIN that is the same
    // producer at the same moment; for BOB it is the working-copy producer measured against the
    // store it converged to, which is the load-bearing half.
    for event in &events {
        let acct = storage::load_account(&mut ctx, event.user).unwrap();
        let usdc: U256 = acct.usdc_balance.clone().into();
        assert_eq!(event.usdcBalance, usdc, "snapshot for {:?}", event.user);
        // The wallet field is SIGNED and unclamped now: it is the stored value verbatim, not
        // `visible_perp_wallet_balance()`.
        assert_eq!(
            event.totalCrossWalletBalance, acct.perp_wallet_balance,
            "snapshot for {:?}",
            event.user
        );
        assert_event_matches_get_account(&mut ctx, event);
    }
}

#[test]
fn sell_taker_fully_matches_resting_bid() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // resting bid
    let sell_id = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // taker sell

    assert_terminal(&mut ctx, buy_id);
    assert_terminal(&mut ctx, sell_id);
    assert!(storage::load_bid_prices(&mut ctx, MARKET_ID)
        .unwrap()
        .is_empty());
}

#[test]
fn fill_opens_correct_long_and_short_positions() {
    // Bob rests a sell, Alice buys as taker.
    // After fill:
    //   Alice: long  QTY,  v_quote = -FILL_VALUE, margin = INIT_MARGIN
    //   Bob:   short QTY,  v_quote = +FILL_VALUE, margin = INIT_MARGIN
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    let alice = pos(&mut ctx, ALICE);
    let bob = pos(&mut ctx, BOB);

    assert_eq!(alice.amount, QTY as i64);
    assert_eq!(alice.v_quote_balance, -(FILL_VALUE as i64));
    assert_eq!(alice.margin, INIT_MARGIN as i64);

    assert_eq!(bob.amount, -(QTY as i64));
    assert_eq!(bob.v_quote_balance, FILL_VALUE as i64);
    assert_eq!(bob.margin, INIT_MARGIN as i64);
}

#[test]
fn fill_debits_init_margin_from_both_wallets() {
    // Maker reserves margin when resting; that reservation is released then
    // re-spent as initial margin on fill. Fees are charged on top and credited
    // to the admin fee recipient.
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // resting ask
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // taker buy

    assert_eq!(wallet(&mut ctx, ALICE), WALLET - INIT_MARGIN - TAKER_FEE);
    assert_eq!(wallet(&mut ctx, BOB), WALLET - INIT_MARGIN - MAKER_FEE);
    assert_eq!(wallet(&mut ctx, ADMIN), TAKER_FEE + MAKER_FEE);
}

/// A maker OPEN fill funds its FEE entirely out of the margin the fill itself posts, so the fee
/// needs no free wallet on top of the margin — the property the old `fee_reserved` escrow
/// provided, now provided by the Binance rule instead.
///
/// CHANGED BY THE ESCROW REMOVAL: the fixture used to drain BOB's wallet to ZERO before the fill,
/// because the order's own escrow was all the funding the fill needed. The MARGIN escrow is gone
/// too, so the fill draws `INIT_MARGIN` from the wallet at fill time and a zero wallet would make
/// it unfundable — the fill still happens and the SILO comes up short (pinned separately in
/// `an_unfundable_maker_fill_still_fills_and_the_silo_is_short_not_the_wallet`). The fixture now leaves BOB
/// exactly the margin and NOT ONE UNIT MORE, which is the sharpest form of the claim under test:
/// the fee is carved out of that margin, never charged on top of it.
#[test]
fn maker_open_fill_funds_its_fee_from_margin_needing_no_free_wallet() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_user_fee_rates(
        &mut ctx,
        BOB,
        UserFeeRates {
            maker_fee_bps: 200,
            taker_fee_bps: 0,
        },
    )
    .unwrap();
    let maker_fee = FILL_VALUE * 200 / 10_000;

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // resting ask, no escrow taken
                                               // Exactly the opening margin, nothing spare for a fee.
    let mut bob = storage::load_account(&mut ctx, BOB).unwrap();
    bob.perp_wallet_balance = INIT_MARGIN as i64;
    storage::save_account(&mut ctx, BOB, bob, AccountUpdateReason::Adjustment).unwrap();

    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // taker buy fills him

    let bob_pos = pos(&mut ctx, BOB);
    assert_eq!(bob_pos.margin, (INIT_MARGIN - maker_fee) as i64);
    assert_eq!(
        wallet(&mut ctx, BOB),
        0,
        "the wallet funded the margin and NOT the fee"
    );
    assert_eq!(wallet(&mut ctx, ADMIN), maker_fee);
}

/// **THE MAKER-FILL FUNDING RULE.** Deleting the placement escrow means a maker fill's opening
/// margin has to come out of the wallet AT FILL TIME, and the money may not be there — the wallet
/// is only ever gated against `Σ ooIM` at ADMISSION, and `ooIM` (which values the position leg at
/// mark and nets the close a fill performs) is not an upper bound on a fill's actual draw.
///
/// **The fill happens anyway, and the SILO takes the shortfall** — model **M1**. This test has been
/// rewritten twice. It first pinned `RejectedInsolvent` (the maker order cancelled, the taker
/// walking on) = model **M2 ("don't fill")** in `misc/binance-flip-and-admission.md` §3.3, which is
/// wrong: the measured exchange behaviour is that an already-resting order sits at `status = 'NEW'`
/// while headroom is NEGATIVE (`crossWalletBalance − totalOpenOrderInitialMargin = −0.00085981`) at
/// the same instant a NEW order is refused `-2019` (`binance-margin-verified-model.md` §1.6) —
/// admission is a one-time check and only LIQUIDATION kills an order. It then pinned **M1′** (silo
/// funded in full, wallet driven negative), which §3.3 flagged as a ~50/50 CONJECTURE.
///
/// R11 settled it and it is **M1** (`derived-ooim-plan.md` §3a): the silo receives all the cash
/// there is and not a satoshi more — measured `isolatedWallet == W0 + realized` digit-for-digit
/// against a higher IM-implied figure — and the wallet does NOT carry a margin shortfall.
///
/// Arithmetic here: BOB's requirement is `INIT_MARGIN` (leverage 1, so the full fill notional) and
/// his wallet holds `INIT_MARGIN − 1`, so
/// `opening_margin = min(INIT_MARGIN, INIT_MARGIN − 1) = INIT_MARGIN − 1`: the silo is short by
/// exactly 1 and the wallet lands on 0, not −1. Nothing is minted either way — the position gains
/// exactly what the wallet loses — but under M1 the missing unit is attached to the POSITION, so
/// closing or liquidating it resolves the deficit instead of stranding it (see
/// `risk::tests::usdc_custody`).
#[test]
fn an_unfundable_maker_fill_still_fills_and_the_silo_is_short_not_the_wallet() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);
    // A LIVE mark, so K9 actually runs on the short-funded trial position (it is skipped at
    // `mark == 0`) and so `compute_margin_info` below reports a real requirement. BOB's short at
    // the mark has equity `999_999 + 1e6 − 1e6` against a `1e6/6` maintenance requirement, so K9
    // passes — a silo short by 1 is nowhere near insolvent, which is the R11 shape (`silo/MM`
    // there was 24.5×).
    set_mark(&mut ctx, PRICE);

    // BOB rests at $100 (FIFO-first), CAROL behind him at the same price.
    let bob_sell = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let carol_sell = place(&mut ctx, CAROL, 1, PRICE, QTY, 0, 0);
    // BOB is then drained to ONE UNIT SHORT of the opening margin — reachable in production via a
    // fee, a funding charge or an adverse mark move between admission and fill.
    let mut bob = storage::load_account(&mut ctx, BOB).unwrap();
    bob.perp_wallet_balance = INIT_MARGIN as i64 - 1;
    storage::save_account(&mut ctx, BOB, bob, AccountUpdateReason::Adjustment).unwrap();

    let taker = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    // BOB's order is terminal because it FILLED, not because it was cancelled — which the position
    // below is the evidence for (delete-on-terminal drops the record either way).
    assert_terminal(&mut ctx, bob_sell);
    let bob_pos = pos(&mut ctx, BOB);
    assert_eq!(bob_pos.amount, -(QTY as i64), "the short was opened");
    assert_eq!(
        bob_pos.margin,
        INIT_MARGIN as i64 - 1,
        "the silo got all the cash there was and not a unit more: SHORT of its own \
         INIT_MARGIN requirement by exactly the 1-unit shortfall (M1, not M1′)"
    );
    assert_eq!(
        storage::load_account(&mut ctx, BOB)
            .unwrap()
            .perp_wallet_balance,
        0,
        "the wallet is drained to exactly zero and does NOT go negative — the deficit is \
         attached to the position, not decoupled from it"
    );
    // The requirement itself is unchanged — it is the FUNDING that fell short, and the gap is
    // visible as `positionInitialMargin > isolatedWallet` (the §3.9 shape, which
    // `a_position_naturally_below_its_own_initial_margin_survives_normally` proves is normal).
    let info = crate::margin_view::compute_margin_info(&mut ctx, BOB, MARKET_ID).unwrap();
    assert_eq!(info.position_initial_margin, INIT_MARGIN);
    assert_eq!(
        info.position_initial_margin as i64 - bob_pos.margin,
        1,
        "the silo is short of its own IM by exactly the unfunded unit"
    );
    // Conservation: BOB's wallet + position margin is unchanged by the fill (he opened at the mark,
    // so there is no PnL and no fee here), so nothing was minted to fund the silo — and nothing was
    // burned by capping it either. This assertion holds identically under M1 and M1′; it is the
    // SPLIT between the two terms that moved.
    assert_eq!(
        storage::load_account(&mut ctx, BOB)
            .unwrap()
            .perp_wallet_balance
            + bob_pos.margin,
        INIT_MARGIN as i64 - 1
    );

    // The taker got its FULL fill from BOB, so it never reached CAROL — the walk no longer skips
    // an underfunded maker.
    assert_terminal(&mut ctx, taker);
    assert_eq!(pos(&mut ctx, ALICE).amount, QTY as i64);
    assert_eq!(
        get_order(&mut ctx, carol_sell).status,
        OrderStatus::Open,
        "CAROL is still resting behind BOB — nothing walked past him"
    );
    assert_eq!(pos(&mut ctx, CAROL).amount, 0);

    // The shortfall is still a real constraint, it just sits in the silo now: BOB's AVAILABLE is
    // exactly zero, so every money-out gate (`transferFromPerp`, `withdraw`, `addPositionMargin`,
    // any new order with a positive requirement) still refuses him. What changed is that he is not
    // in DEBT — a unit credited to him is spendable rather than swallowed by a deficit.
    assert_eq!(available(&mut ctx, BOB), 0);
    assert!(!crate::margin_view::derived_can_afford(
        available(&mut ctx, BOB),
        1
    ));
    fund(&mut ctx, BOB, 1);
    assert_eq!(
        available(&mut ctx, BOB),
        1,
        "the credit is spendable: under M1′ it would have been absorbed netting a −1 deficit"
    );
}

/// **LP HONESTY — the second reason M1 beats M1′.** A short silo is not a cosmetic bookkeeping
/// choice: `pos.margin` is the `isolatedWallet` every maintenance evaluation is measured against
/// (`is_above_maintenance_margin`, and through it `liquidate()` and the sweep), so funding it short
/// moves the position's liquidation price to where its risk actually is. Under M1′ the silo looked
/// FULL — the deficit was parked on an account-global wallet no maintenance check reads — so LP was
/// optimistic by exactly the shortfall while the protocol carried the risk.
///
/// Same fixture as the test above, with a 300_000-unit shortfall instead of 1, and the mark then
/// pushed to $160. BOB is short QTY with `v_quote = +1_000_000`:
///
/// ```text
///                     equity = margin + v_quote − value(mark)      MM = value(mark)/6
///   short silo  700_000 + 1_000_000 − 1_600_000 =  100_000   <   266_666   ⇒ LIQUIDATABLE
///   full  silo  1_000_000 + 1_000_000 − 1_600_000 = 400_000   ≥   266_666   ⇒ safe
/// ```
///
/// So the whole 300_000 of shortfall shows up in the maintenance buffer, and the counterfactual is
/// the SAME function with the SAME arguments but the full margin — not a re-derivation.
#[test]
fn a_short_silo_lowers_the_maintenance_buffer_it_is_measured_against() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    set_mark(&mut ctx, PRICE);
    const SHORTFALL: u64 = 300_000;

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let mut bob = storage::load_account(&mut ctx, BOB).unwrap();
    bob.perp_wallet_balance = (INIT_MARGIN - SHORTFALL) as i64;
    storage::save_account(&mut ctx, BOB, bob, AccountUpdateReason::Adjustment).unwrap();
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    let bob_pos = pos(&mut ctx, BOB);
    assert_eq!(bob_pos.margin, (INIT_MARGIN - SHORTFALL) as i64);
    assert_eq!(wallet(&mut ctx, BOB), 0);

    // The shortfall is ABI-visible as the position's own margin, i.e. as `isolatedWallet` —
    // consumers compute LP from this number, so it has to be the short one.
    set_mark(&mut ctx, 160 * TICK);
    let i = crate::margin_view::compute_margin_info(&mut ctx, BOB, MARKET_ID).unwrap();
    assert_eq!(i.position_margin, (INIT_MARGIN - SHORTFALL) as i64);
    assert_eq!(i.maint_margin, 1_600_000 / 6);
    assert_eq!(
        i.isolated_margin, 100_000,
        "equity carries the shortfall: 700_000 margin + 400_000 unrealised loss offset"
    );
    assert!(
        i.isolated_margin < i.maint_margin as i64,
        "the short-silo position is BELOW maintenance at $160"
    );

    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    let is_safe = |margin: i64| {
        crate::math::is_above_maintenance_margin(
            &market.tiers,
            160 * TICK,
            bob_pos.amount,
            bob_pos.v_quote_balance,
            margin,
            market.base_decimals,
            market.price_decimals,
        )
        .unwrap()
    };
    assert!(!is_safe(bob_pos.margin), "short silo: liquidatable");
    assert!(
        is_safe(INIT_MARGIN as i64),
        "the SAME position with the silo funded in full is safe at this mark — the 300_000 of \
         shortfall is exactly what moved the liquidation price, and M1′ hid it"
    );

    // And it is not merely arithmetic: the real liquidation path acts on it.
    assert!(
        crate::risk::run_liquidate(
            &crate::interface::IPerpDex::liquidateCall {
                user: BOB,
                marketId: MARKET_ID,
            }
            .abi_encode(),
            CAROL,
            &mut ctx,
        )
        .is_ok(),
        "liquidate() reads pos.margin, so a short silo really does get liquidated here"
    );
    assert_eq!(pos(&mut ctx, BOB).amount, 0);
}

/// **The ONE remaining route to a negative `perp_wallet_balance`: an unabsorbable maker COMMISSION.**
///
/// After the M1 switch every other decrease is bounded below by zero — every money-out gate
/// (`transferFromPerp`, `addPositionMargin`, `depositInsuranceFund`) refuses unless
/// `derived_can_afford(available, amount)` with `amount > 0`, and `available = wallet − Σ ooIM ≤
/// wallet`; the taker fill is gated the same way on `total_required`; funding settles into
/// `pos.margin` and never touches the wallet; the liquidation clearance fee is explicitly
/// `.min(perp_wallet_balance.max(0))`; a liquidation residual and both ADL legs only ever CREDIT.
/// What is left is `fee_from_wallet = maker_fee − min(maker_fee, opening_margin)` in
/// `settle_maker_fill_core`: when the capped opening margin cannot absorb the commission, the wallet
/// pays the rest — and a PURE CLOSE has no opening margin at all.
///
/// This is deliberate, it is §3a's recommendation for the commission gap (let the position carry it
/// where there is a position to carry it, and do NOT open a second insurance-fund path), and it is
/// bounded by the fee. It is also the one place Binance's own wallet goes negative: R11 measured
/// `−0.25717240`, exactly the fill commission, cleared by `INSURANCE_CLEAR` seconds later. Ours is
/// not cleared, so `perp_wallet_balance: i64` stays.
///
/// **This state is the reason `AccountBalanceChanged` had to become signed.** The event's wallet
/// field used to be `uint64 perpWalletBalance` and floored at 0, so the very deficit this test
/// produces was invisible to anyone watching only the event stream — the field reported `0` while
/// the ledger held `−12_000`. The assertions below pin the fixed behaviour: the event reports
/// `totalCrossWalletBalance == −12_000` unclamped, and a hypothetical clamped reading would have
/// said `0`.
///
/// Arithmetic: BOB is long QTY at entry $100 with a leverage-3 silo of 333_333 and an EMPTY wallet,
/// and rests a sell at the $60 mark (a pure reduce, so `ooIM = 0` admits it for free). The close
/// realizes `−1_000_000 + 600_000 = −400_000` against 333_333 of released margin, so it is INSOLVENT
/// — 66_667 of bad debt to the fund, and isolated margin leaves the wallet untouched at 0. The
/// 200 bps maker fee on the 600_000 filled is then 12_000 with nothing to absorb it.
#[test]
fn a_maker_close_fee_is_the_only_remaining_way_to_a_negative_wallet() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    set_mark(&mut ctx, 60 * TICK);
    storage::save_insurance_fund(&mut ctx, 10_000_000).unwrap();
    storage::save_user_fee_rates(
        &mut ctx,
        BOB,
        UserFeeRates {
            maker_fee_bps: 200,
            taker_fee_bps: 0,
        },
    )
    .unwrap();
    // BOB: long QTY @ $100, leverage 3, and NOT ONE UNIT of free wallet.
    storage::save_position(
        &mut ctx,
        BOB,
        MARKET_ID,
        &PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -(FILL_VALUE as i64),
            margin: (FILL_VALUE / 3) as i64,
            leverage: 3,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let mut bob = storage::load_account(&mut ctx, BOB).unwrap();
    bob.perp_wallet_balance = 0;
    storage::save_account(&mut ctx, BOB, bob, AccountUpdateReason::Adjustment).unwrap();

    // A pure-reduce sell is free on the derived basis (`|N − Ask| == 0`), so an empty wallet is no
    // obstacle to RESTING it — which is why this state is reachable without any hand-written order.
    assert_eq!(oo_im(&mut ctx, BOB), 0);
    let bob_sell = place(&mut ctx, BOB, 1, 60 * TICK, QTY, 0, 0);
    assert_eq!(oo_im(&mut ctx, BOB), 0, "still free once it is resting");

    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    // Drain the setup's own events AND its marks, so the snapshot below belongs to the fill alone.
    let _ = JournalTr::take_logs(ctx.journal_mut());
    start_call(&mut ctx);
    place(&mut ctx, ALICE, 0, 60 * TICK, QTY, 0, 0); // taker buy fills BOB
    end_call(&mut ctx);
    assert_terminal(&mut ctx, bob_sell);

    let maker_fee: i64 = 600_000 * 200 / 10_000; // 12_000
    let bob_pos = pos(&mut ctx, BOB);
    assert_eq!(
        (bob_pos.amount, bob_pos.margin),
        (0, 0),
        "flat, silo emptied"
    );
    assert_eq!(
        storage::load_account(&mut ctx, BOB)
            .unwrap()
            .perp_wallet_balance,
        -maker_fee,
        "the wallet went negative by EXACTLY the commission and no more — no margin shortfall \
         reaches it any more (that is the M1 cap), only a fee with nothing left to absorb it"
    );
    // ── THE TASK-A PIN: the deficit reaches the EVENT STREAM, unclamped ──────────────────────
    let bob_events = JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
        .map(|log| {
            AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
        })
        .filter(|e| e.user == BOB)
        .collect::<Vec<_>>();
    assert_eq!(
        bob_events.len(),
        1,
        "one snapshot per user per call, drained at the end"
    );
    assert_eq!(
        bob_events[0].totalCrossWalletBalance, -maker_fee,
        "the event reports the DEFICIT. Under the retired `uint64 perpWalletBalance` this read 0 \
         and an operator watching only the event stream could not see it accumulate."
    );
    assert_eq!(
        storage::load_account(&mut ctx, BOB)
            .unwrap()
            .visible_perp_wallet_balance(),
        0,
        "what a CLAMPED reading would have said — kept as the contrast, used by no published surface"
    );
    // BOB is flat with no orders, so he has left the market index entirely: the event's totals are
    // an EMPTY fold plus the raw wallet, and it still agrees with `getAccount` field for field.
    assert_eq!(
        bob_events[0].totalWalletBalance, -maker_fee,
        "no silos left"
    );
    // `availableBalance` left the event with the six other margin totals (they are `/fapi/v2/account`
    // fields, not `ACCOUNT_UPDATE` fields), so it is asserted on its only remaining surface. The
    // deficit shows through there too, which is the point of the un-clamping.
    assert_eq!(
        get_account(&mut ctx, BOB).availableBalance,
        -maker_fee,
        "no ooIM left, and getAccount reports the deficit unclamped"
    );
    assert_event_matches_get_account(&mut ctx, &bob_events[0]);
    // Not a mint: the fee recipient really was paid, and the insolvent close's beyond-margin slice
    // really did reach the fund. The negative is the funding gap between the two.
    assert_eq!(wallet(&mut ctx, ADMIN), maker_fee as u64);
    assert_eq!(
        if_before - storage::load_insurance_fund(&mut ctx).unwrap(),
        66_667,
        "loss 400_000 − released margin 333_333"
    );
    // Money-out is refused while under water, and a deposit nets against it — the receivable
    // machinery is unchanged, it just guards a commission-sized transient now.
    assert!(!crate::margin_view::derived_can_afford(
        available(&mut ctx, BOB),
        1
    ));
    fund(&mut ctx, BOB, maker_fee as u64);
    assert_eq!(available(&mut ctx, BOB), 0, "the credit nets the deficit");
}

/// The `RejectedInsolvent` channel is still LIVE — it just no longer answers "the wallet is short".
/// Its remaining trigger is K9: a fill that would leave the maker's POSITION below maintenance
/// margin at the current mark. A drained wallet alone must not fire it (previous test); an
/// underwater OPEN must, and the taker must still walk past the cancelled maker to the next one —
/// the "cancel + walk on" mechanics the wallet-shortfall case used to share.
///
/// BOB rests a sell at $100; the mark is then pushed to $200. Filling BOB would OPEN him a short of
/// 1 lot at $100 against a $200 mark — equity `−2_000_000 + 1_000_000 + 1_000_000 = 0` against a
/// maintenance requirement of `2_000_000 / 6` — so K9 refuses it. CAROL, resting behind him, is
/// already LONG 1 lot, so her identical sell is a pure CLOSE (`opening_qty == 0`) and K9 does not
/// gate it: she takes the fill instead.
#[test]
fn the_k9_maintenance_guard_still_cancels_a_maker_and_the_taker_walks_on() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);
    // CAROL is long 1 lot at $100 (margin 1e6, vq −1e6), so her sell below is a pure close.
    storage::save_position(
        &mut ctx,
        CAROL,
        MARKET_ID,
        &PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -(FILL_VALUE as i64),
            margin: INIT_MARGIN as i64,
            leverage: 1,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();

    let bob_sell = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let carol_sell = place(&mut ctx, CAROL, 1, PRICE, QTY, 0, 0);

    // Mark to $200, with a band wide enough that the $100 fill is still executable.
    let mut market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    market.mark_price = 200 * TICK;
    market.price_band_bps = 1_000_000;
    storage::save_market(&mut ctx, &market).unwrap();

    let bob_wallet_before = storage::load_account(&mut ctx, BOB)
        .unwrap()
        .perp_wallet_balance;
    let taker = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    // BOB: cancelled, no position, wallet untouched (K9 rejects BEFORE the debit is adopted).
    assert_terminal(&mut ctx, bob_sell);
    assert_eq!(pos(&mut ctx, BOB).amount, 0, "no position was opened");
    assert_eq!(
        storage::load_account(&mut ctx, BOB)
            .unwrap()
            .perp_wallet_balance,
        bob_wallet_before,
        "a K9 reject moves no money at all"
    );
    // CAROL took the fill instead — the walk continued past the rejected maker.
    assert_terminal(&mut ctx, taker);
    assert_terminal(&mut ctx, carol_sell);
    assert_eq!(pos(&mut ctx, ALICE).amount, QTY as i64);
    assert_eq!(pos(&mut ctx, CAROL).amount, 0, "her long was closed out");
}

#[test]
fn market_fee_total_tracks_collected_maker_and_taker_fees() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    storage::save_user_fee_rates(
        &mut ctx,
        ALICE,
        UserFeeRates {
            maker_fee_bps: 0,
            taker_fee_bps: 100,
        },
    )
    .unwrap();
    storage::save_user_fee_rates(
        &mut ctx,
        BOB,
        UserFeeRates {
            maker_fee_bps: 200,
            taker_fee_bps: 0,
        },
    )
    .unwrap();

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    let expected_taker_fee = FILL_VALUE * 100 / 10_000;
    let expected_maker_fee = FILL_VALUE * 200 / 10_000;
    let expected_total = expected_taker_fee + expected_maker_fee;
    assert_eq!(market_fee_total(&mut ctx), expected_total);
    assert_eq!(wallet(&mut ctx, ADMIN), expected_total);
}

// ── Fee funding: charged from the margin the fill funds, never escrowed at placement ────────
// One test per path of `fee_from_margin = min(fee, opening_margin)`.

/// PURE OPEN, taker. Binance parity (`isolatedWallet = Ne/L − f·Ne`): the wallet funds the
/// opening margin and NOTHING else — the fee is taken out of that margin, not added on top.
#[test]
fn taker_open_fill_charges_fee_from_margin_not_on_top_of_the_wallet() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_user_fee_rates(
        &mut ctx,
        ALICE,
        UserFeeRates {
            maker_fee_bps: 0,
            taker_fee_bps: 100,
        },
    )
    .unwrap();
    let fee = FILL_VALUE * 100 / 10_000;

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // resting ask, maker fee 0
    let alice_before = wallet(&mut ctx, ALICE);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // taker buy — ALICE is flat, so PURE open

    assert_eq!(
        alice_before - wallet(&mut ctx, ALICE),
        INIT_MARGIN,
        "wallet funds the opening margin ONLY (was margin + fee)"
    );
    assert_eq!(
        pos(&mut ctx, ALICE).margin,
        (INIT_MARGIN - fee) as i64,
        "the fee left the position margin"
    );
    assert_eq!(
        wallet(&mut ctx, ADMIN),
        fee,
        "the recipient is still paid the FULL fee"
    );
}

/// **A silo BELOW its own IM is the NORMAL case, and nothing in the engine may treat it as an
/// anomaly** — `misc/binance-flip-and-admission.md` §3.9, citing the formula set §1.2:
/// 「**Binance 只连续检查 MM,不检查 IM**」, with 「逐仓仓位**天生**就低于自己的 `IM`」 as its prior.
///
/// The measurement behind that: run1's market open computed `PIM = 6.34041` while the silo actually
/// received `6.30870795` — short by exactly one opening commission — and the position 「照常存活」.
///
/// Ours does the same thing for the same reason (`7cc26360`: the opening fill funds the trading fee
/// out of the margin it creates), so this test produces the state through a REAL fill rather than by
/// writing it, and then asserts the position is in every respect healthy:
///
/// * `isolatedWallet < positionInitialMargin`, by exactly the fee — the §3.9 shape;
/// * it is NOT liquidatable (the maintenance check is the only continuous one);
/// * it still accepts an `addPositionMargin`/`removePositionMargin` round trip (B2 — the removed IM
///   gate used to refuse this exact no-op);
/// * it still accepts a new order.
///
/// If a continuous IM check is ever reintroduced anywhere, at least one of these fails.
#[test]
fn a_position_naturally_below_its_own_initial_margin_survives_normally() {
    use crate::interface::IPerpDex::{
        addPositionMarginCall, liquidateCall, removePositionMarginCall,
    };

    let mut ctx = make_ctx();
    setup(&mut ctx);
    // `setup()` leaves the mark at 0, which would make every derived quantity read as 0 (`N = 0`)
    // and would also disable the maintenance guard — i.e. it would make this test vacuous.
    storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
    storage::save_user_fee_rates(
        &mut ctx,
        ALICE,
        UserFeeRates {
            maker_fee_bps: 0,
            taker_fee_bps: 100,
        },
    )
    .unwrap();
    let fee = FILL_VALUE * 100 / 10_000;

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // real taker open, leverage 1

    // ── the §3.9 shape, produced not written ──
    let i = crate::margin_view::compute_margin_info(&mut ctx, ALICE, MARKET_ID).unwrap();
    assert_eq!(
        i.position_initial_margin, INIT_MARGIN,
        "PIM = ROUND_UP(N/1)"
    );
    assert_eq!(i.position_margin, (INIT_MARGIN - fee) as i64);
    assert!(
        i.position_margin < i.position_initial_margin as i64,
        "the silo is BELOW its own IM: {} < {}",
        i.position_margin,
        i.position_initial_margin
    );
    assert_eq!(
        i.position_initial_margin as i64 - i.position_margin,
        fee as i64,
        "short by exactly one opening commission — run1's 6.34041 vs 6.30870795"
    );
    // Equity is below the joint requirement too, which is the same statement at account level.
    assert!(i.isolated_margin < i.initial_margin as i64);

    // ── and it survives: MM is the only continuous check ──
    assert!(
        i.maint_margin < i.isolated_margin as u64,
        "comfortably above maintenance: MM {} vs equity {}",
        i.maint_margin,
        i.isolated_margin
    );
    let liq = crate::risk::run_liquidate(
        &liquidateCall {
            user: ALICE,
            marketId: MARKET_ID,
        }
        .abi_encode(),
        BOB,
        &mut ctx,
    );
    assert!(
        liq.is_err(),
        "a silo below its own IM must NOT be liquidatable — MM is the only continuous gate"
    );

    // ── B2: the add/remove no-op round trip the removed IM gate refused ──
    let margin_before = pos(&mut ctx, ALICE).margin;
    let wallet_before = wallet(&mut ctx, ALICE);
    crate::risk::run_add_position_margin(
        &addPositionMarginCall {
            marketId: MARKET_ID,
            amount: 100_000,
        }
        .abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap();
    crate::risk::run_remove_position_margin(
        &removePositionMarginCall {
            marketId: MARKET_ID,
            amount: 100_000,
        }
        .abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(pos(&mut ctx, ALICE).margin, margin_before);
    assert_eq!(wallet(&mut ctx, ALICE), wallet_before);

    // ── and it can still trade ──
    place(&mut ctx, ALICE, 0, PRICE / 2, QTY, 0, 0);
    assert_eq!(
        pos(&mut ctx, ALICE).total_buy_qty,
        QTY,
        "a new order is admitted on the ordinary derived basis, not refused for an IM shortfall"
    );
}

/// PURE OPEN, maker. The highest-risk arm: this path had no wallet charge at all before (the fee
/// was released from the `fee_reserved` escrow), so the funding had to be ADDED, not deleted.
#[test]
fn maker_open_fill_charges_fee_from_margin_not_from_the_wallet() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_user_fee_rates(
        &mut ctx,
        BOB,
        UserFeeRates {
            maker_fee_bps: 200,
            taker_fee_bps: 0,
        },
    )
    .unwrap();
    let fee = FILL_VALUE * 200 / 10_000;

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // rest: no escrow of any kind
    let bob_resting = wallet(&mut ctx, BOB);
    // CHANGED BY THE ESCROW REMOVAL: resting takes nothing, so the wallet is still whole here.
    assert_eq!(bob_resting, WALLET);

    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // ALICE takes it (taker fee 0)

    // CHANGED: the fill DOES debit the maker's wallet now — by the opening margin, which the
    // placement escrow used to have withheld already. Net of the two steps the maker is in
    // exactly the same place as before (`WALLET - INIT_MARGIN`), and the fee is still carved out
    // of the margin rather than charged on top: BOB's total (wallet + margin) fell by `fee` only.
    assert_eq!(
        wallet(&mut ctx, BOB),
        bob_resting - INIT_MARGIN,
        "the fill funds the opening margin from the wallet"
    );
    assert_eq!(pos(&mut ctx, BOB).margin, (INIT_MARGIN - fee) as i64);
    assert_eq!(wallet(&mut ctx, ADMIN), fee, "recipient paid in full");
    assert_eq!(
        wallet(&mut ctx, BOB) as i64 + pos(&mut ctx, BOB).margin,
        (WALLET - fee) as i64,
        "conservation: the maker is down exactly the fee"
    );
}

/// PURE CLOSE. `opening_margin == 0` → there is nothing to charge the fee to, so the wallet pays
/// all of it: behaviour IDENTICAL to before the escrow was removed.
#[test]
fn pure_close_fill_charges_the_whole_fee_to_the_wallet() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_user_fee_rates(
        &mut ctx,
        ALICE,
        UserFeeRates {
            maker_fee_bps: 0,
            taker_fee_bps: 100,
        },
    )
    .unwrap();
    let fee = FILL_VALUE * 100 / 10_000;

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // ALICE long QTY
    let margin_held = pos(&mut ctx, ALICE).margin; // INIT_MARGIN − the opening fee
    let alice_before = wallet(&mut ctx, ALICE);

    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0); // BOB bids (pure reduce of his short)
    place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0); // ALICE sells — PURE close at zero PnL

    let alice = pos(&mut ctx, ALICE);
    assert_eq!((alice.amount, alice.margin), (0, 0));
    assert_eq!(
        wallet(&mut ctx, ALICE) as i64 - alice_before as i64,
        margin_held - fee as i64,
        "the close returns its margin and the wallet pays the whole fee"
    );
    assert_eq!(wallet(&mut ctx, ADMIN), fee * 2);
}

/// FLIP: one fill that closes part of a position and opens the rest, with the fee charged on the
/// FULL notional. This is the case a naive `margin -= fee` gets wrong — the fee here EXCEEDS the
/// opening margin, so it must split. `min()` bounds the margin draw, the remainder goes to the
/// wallet, and nothing underflows. (`mark_price` is 0 in this fixture, so K9 is not in play; the
/// ordering of the fee against K9 is a separate concern from this arithmetic.)
#[test]
fn flip_fill_splits_the_fee_between_margin_and_wallet_without_underflow() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, ALICE, 90_000_000);
    fund(&mut ctx, BOB, 90_000_000);
    let funded = (WALLET + 90_000_000) as i64;
    storage::save_user_fee_rates(
        &mut ctx,
        ALICE,
        UserFeeRates {
            maker_fee_bps: 0,
            taker_fee_bps: 1_000, // 10% — the tightened ceiling
        },
    )
    .unwrap();

    // ALICE opens long 10·QTY (notional 10 USDC at leverage 1).
    place(&mut ctx, BOB, 1, PRICE, QTY * 10, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY * 10, 0, 0);
    let open_fee = FILL_VALUE * 10 * 1_000 / 10_000; // 1_000_000
    assert_eq!(
        pos(&mut ctx, ALICE).margin,
        (FILL_VALUE * 10 - open_fee) as i64
    );

    // Now sell 11·QTY into a resting bid: closes the 10 long and opens 1 short.
    place(&mut ctx, BOB, 0, PRICE, QTY * 11, 0, 0);
    let margin_before = pos(&mut ctx, ALICE).margin; // 9_000_000
    let wallet_before = wallet(&mut ctx, ALICE) as i64;
    place(&mut ctx, ALICE, 1, PRICE, QTY * 11, 0, 0);

    // Fee is on the FULL 11 units; only the 1-unit opening leg funds margin.
    let flip_fee = FILL_VALUE * 11 * 1_000 / 10_000; // 1_100_000
    let opening_margin = FILL_VALUE; // 1 unit at leverage 1
    let fee_from_margin = flip_fee.min(opening_margin); // 1_000_000 — bounded by min()
    let fee_from_wallet = flip_fee - fee_from_margin; //   100_000 — the remainder
    assert!(flip_fee > opening_margin, "this must be a genuine split");

    let alice = pos(&mut ctx, ALICE);
    assert_eq!(
        alice.amount,
        -(QTY as i64),
        "flipped from long 10 to short 1"
    );
    assert_eq!(
        alice.margin,
        (opening_margin - fee_from_margin) as i64,
        "no underflow: the margin draw is capped at the opening margin"
    );
    let wallet_after = wallet(&mut ctx, ALICE) as i64;
    assert_eq!(
        wallet_after - wallet_before,
        margin_before - (opening_margin + fee_from_wallet) as i64,
        "the wallet returns the closed margin, then funds the new leg + the fee remainder"
    );
    // Conservation across the flip: (wallet + margin) fell by exactly the fee (PnL is zero here),
    // and the recipient holds every fee charged so far.
    assert_eq!(
        (wallet_after + alice.margin) - (wallet_before + margin_before),
        -(flip_fee as i64)
    );
    assert_eq!(wallet(&mut ctx, ADMIN), open_fee + flip_fee);
    assert_eq!(
        wallet_after + alice.margin,
        funded - (open_fee + flip_fee) as i64
    );
}

/// CHANGED BY THE ESCROW REMOVAL. Placement used to DEBIT the margin reserve (and nothing else)
/// and a cancel used to CREDIT it back. Now neither leg touches the wallet at all: placement
/// raises the derived requirement and cancel drops it, so the round-trip is exact for the same
/// reason but with no money moving in either direction. The maker fee is still not withheld.
#[test]
fn placement_charges_no_wallet_and_cancel_restores_the_available_exactly() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_user_fee_rates(
        &mut ctx,
        ALICE,
        UserFeeRates {
            maker_fee_bps: 200,
            taker_fee_bps: 0,
        },
    )
    .unwrap();

    let before = wallet(&mut ctx, ALICE);
    let available_before = available(&mut ctx, ALICE);
    let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    assert_eq!(
        wallet(&mut ctx, ALICE),
        before,
        "the wallet does not move at placement"
    );
    assert_eq!(
        available_before - available(&mut ctx, ALICE),
        INIT_MARGIN as i128,
        "available drops by the open-order margin ONLY — no fee is withheld"
    );
    assert_eq!(oo_im(&mut ctx, ALICE), INIT_MARGIN);

    run_cancel_order(
        &cancelOrderCall {
            orderId: id.into(),
            marketId: MARKET_ID,
        }
        .abi_encode(),
        ALICE,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(
        wallet(&mut ctx, ALICE),
        before,
        "cancel moves no money either — the wallet never left `before`"
    );
    assert_eq!(oo_im(&mut ctx, ALICE), 0);
    assert_eq!(
        available(&mut ctx, ALICE),
        available_before,
        "cancel is a clean round-trip on the available"
    );
}

/// CHANGED BY THE DERIVED-ooIM SWITCH. Two things move, both structural:
///
///  * A LIVE MARK is now required for the scenario to mean anything. With `setup()`'s mark of 0
///    the derived basis cannot see BOB's short at all, so his 2-lot buy would read as a naked
///    2e6 requirement instead of a hedge. Production always has a mark; the fixture did not.
///  * BOB's 2-lot buy against his own 1-lot short costs him NOTHING: at mark $100 the joint
///    requirement is `max(|N + Bid|, |N − Ask|) = max(|−1e6 + 2e6|, |−1e6|) = 1e6`, exactly
///    `|N|`, so `ooIM = 1e6 − 1e6 = 0`. Buying 2 lots against a 1-lot short can leave at most a
///    1-lot LONG — the same exposure, already margined. The escrow charged 1e6 for it (the
///    excess lot's opening notional), which is the flip-nets-nothing over-charge the migration
///    removed.
///
/// The claim under test is unchanged and still holds: once the fill closes BOB's short, the
/// surviving 1-lot buy stops being a hedge and becomes a full 1e6 requirement again.
#[test]
fn maker_fill_reprices_the_remaining_order_requirement_after_the_position_closes() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    set_mark(&mut ctx, PRICE);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert_eq!(pos(&mut ctx, BOB).amount, -(QTY as i64));
    // The maker fill funded BOB's opening margin FROM THE WALLET (there was no escrow to draw
    // on) — same net position as before the migration, reached in one step instead of two.
    assert_eq!(wallet(&mut ctx, BOB), WALLET - INIT_MARGIN);

    let buy_id = place(&mut ctx, BOB, 0, PRICE, QTY * 2, 0, 0);
    assert_eq!(
        oo_im(&mut ctx, BOB),
        0,
        "a 2-lot buy hedging a 1-lot short is free"
    );
    assert_eq!(wallet(&mut ctx, BOB), WALLET - INIT_MARGIN);
    assert_eq!(available(&mut ctx, BOB), (WALLET - INIT_MARGIN) as i128);

    place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);

    let bob = pos(&mut ctx, BOB);
    assert_eq!(bob.amount, 0);
    assert_eq!(bob.margin, 0);
    // Flat again ⇒ N = 0 ⇒ the surviving 1-lot buy is charged in full.
    assert_eq!(
        bob.total_buy_notional, INIT_MARGIN,
        "Bid after the partial fill"
    );
    assert_eq!(oo_im(&mut ctx, BOB), INIT_MARGIN);
    // Closing the short returned its whole margin to the wallet.
    assert_eq!(wallet(&mut ctx, BOB), WALLET);
    assert_eq!(available(&mut ctx, BOB), (WALLET - INIT_MARGIN) as i128);
    assert_eq!(
        get_order(&mut ctx, buy_id).status,
        OrderStatus::PartiallyFilled
    );
}

#[test]
fn fill_rejects_when_taker_wallet_cannot_cover_opening_margin() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // resting ask

    let mut alice = storage::load_account(&mut ctx, ALICE).unwrap();
    alice.perp_wallet_balance = (INIT_MARGIN - 1) as i64;
    storage::save_account(&mut ctx, ALICE, alice, AccountUpdateReason::Adjustment).unwrap();

    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "{err}"
    );
}

#[test]
fn taker_reverse_uses_released_close_margin_before_opening_margin_check() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: -(QTY as i64),
            v_quote_balance: FILL_VALUE as i64,
            margin: INIT_MARGIN as i64,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let mut alice = storage::load_account(&mut ctx, ALICE).unwrap();
    alice.perp_wallet_balance = 0;
    storage::save_account(&mut ctx, ALICE, alice, AccountUpdateReason::Adjustment).unwrap();

    place(&mut ctx, BOB, 1, PRICE, QTY * 2, 0, 0); // resting ask
    let market_buy = place(&mut ctx, ALICE, 0, 0, QTY * 2, 1, 1);

    assert_terminal(&mut ctx, market_buy);
    assert_eq!(wallet(&mut ctx, ALICE), 0);
    assert_eq!(
        pos(&mut ctx, ALICE),
        PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -(FILL_VALUE as i64),
            margin: (INIT_MARGIN - TAKER_FEE * 2) as i64,
            leverage: 1,
            ..PerpPosition::default()
        }
    );
}

#[test]
fn taker_reverse_accounts_close_and_open_values_at_each_fill_price() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);

    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: -(QTY as i64),
            v_quote_balance: FILL_VALUE as i64,
            margin: INIT_MARGIN as i64,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let mut alice = storage::load_account(&mut ctx, ALICE).unwrap();
    alice.perp_wallet_balance = 0;
    storage::save_account(&mut ctx, ALICE, alice, AccountUpdateReason::Adjustment).unwrap();

    let close_price = PRICE - 10 * TICK; // $90
    let open_price = PRICE + 10 * TICK; // $110
    let open_value = 1_100_000;
    let taker_fee = 0;

    place(&mut ctx, BOB, 1, close_price, QTY, 0, 0);
    place(&mut ctx, CAROL, 1, open_price, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, 0, QTY * 2, 1, 1);

    let position_changes = take_position_changes(&mut ctx);
    let taker_change = position_changes
        .iter()
        .find(|change| change.user == ALICE)
        .unwrap();
    assert_eq!(taker_change.realizedPnl, 100_000);
    assert_eq!(taker_change.closedQuantity, QTY);

    assert_eq!(
        pos(&mut ctx, ALICE),
        PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -(open_value as i64),
            margin: (open_value - taker_fee) as i64,
            leverage: 1,
            ..PerpPosition::default()
        }
    );
    assert_eq!(wallet(&mut ctx, ALICE), 0);
}

#[test]
fn maker_close_emits_fill_realized_pnl() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_position(
        &mut ctx,
        BOB,
        MARKET_ID,
        &PerpPosition {
            amount: -(QTY as i64),
            v_quote_balance: FILL_VALUE as i64,
            margin: INIT_MARGIN as i64,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();

    let close_price = PRICE - 10 * TICK;
    place(&mut ctx, BOB, 0, close_price, QTY, 0, 0);
    place(&mut ctx, ALICE, 1, close_price, QTY, 0, 0);

    let position_changes = take_position_changes(&mut ctx);
    let maker_change = position_changes
        .iter()
        .find(|change| change.user == BOB)
        .unwrap();
    assert_eq!(maker_change.realizedPnl, 100_000);
    assert_eq!(maker_change.closedQuantity, QTY);
}

#[test]
fn taker_fill_cancels_worst_same_side_order_to_cover_opening_margin() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let high_buy_price = PRICE - TICK;
    let low_buy_price = PRICE - 2 * TICK;
    let high_buy = place(&mut ctx, ALICE, 0, high_buy_price, QTY, 0, 0);
    let low_buy = place(&mut ctx, ALICE, 0, low_buy_price, QTY, 0, 0);
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // resting ask

    // CHANGED BY THE ESCROW REMOVAL: the fixture's hand-set balance was the leftover AFTER the
    // escrow removed both resting buys' margin — i.e. it was the AVAILABLE. Nothing is removed
    // now, so the same account state is expressed as `available == 20_000`, and the cover loop
    // frees headroom (by dropping `Bid`) rather than cash.
    let low_buy_margin = 980_000;
    set_available(
        &mut ctx,
        ALICE,
        (INIT_MARGIN + TAKER_FEE - low_buy_margin) as i64,
    );

    let market_buy = place(&mut ctx, ALICE, 0, 0, QTY, 1, 1);

    assert_terminal(&mut ctx, market_buy);
    assert_terminal(&mut ctx, low_buy);
    assert_eq!(get_order(&mut ctx, high_buy).status, OrderStatus::Open);
    // Cancelling the $98 buy freed 980_000 of requirement, which exactly funded the 1_000_000
    // opening margin out of the 20_000 that was already free. Nothing is left over.
    assert_eq!(available(&mut ctx, ALICE), 0);
}

#[test]
fn partial_fill_leaves_maker_partially_filled_in_book() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let sell_id = place(&mut ctx, BOB, 1, PRICE, QTY * 2, 0, 0);
    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    assert_eq!(
        get_order(&mut ctx, sell_id).status,
        OrderStatus::PartiallyFilled
    );
    assert_eq!(get_order(&mut ctx, sell_id).filled, QTY);
    assert_terminal(&mut ctx, buy_id);

    // Remaining sell still in ask book.
    assert_eq!(
        storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![PRICE]
    );
}

#[test]
fn maker_fill_does_not_auto_expire_remaining_order_under_isolated_margin() {
    // Under isolated margin a maker fill NEVER auto-cancels the maker's other/remaining
    // orders — the old reserve-deficit auto-expire path is gone. BOB's partially-filled
    // sell stays resting as PartiallyFilled even with a zero wallet (the close here is
    // break-even: BOB closes his long at its entry price, so no bad debt is produced).
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let sell_id = place(&mut ctx, BOB, 1, PRICE, QTY * 2, 0, 0);
    // LOAD-modify-save, NOT a fresh `PerpPosition { .. ..default() }`: overwriting the position
    // wholesale would also zero `total_sell_qty`/`total_sell_notional`, desynchronising the
    // maintained `Bid`/`Ask` aggregates from the order list the placement above just wrote. Those
    // aggregates now feed the derived requirement and are maintained incrementally by the fill
    // path, so a fixture that clobbers them produces a state the engine can never reach (and
    // trips its own underflow invariant).
    let mut bob_pos = storage::load_position(&mut ctx, BOB, MARKET_ID).unwrap();
    bob_pos.amount = QTY as i64;
    bob_pos.v_quote_balance = -(FILL_VALUE as i64);
    bob_pos.leverage = 1;
    storage::save_position(&mut ctx, BOB, MARKET_ID, &bob_pos, AccountUpdateReason::Adjustment).unwrap();
    // Zero wallet, kept deliberately — this is the only low-wallet maker-fill coverage in the file
    // and it is the header's actual subject ("even with a zero wallet"). The new fill-time
    // insolvency gate CANNOT fire here: ALICE's buy closes BOB's long exactly, so
    // `split_position_fill` yields `opening_qty == 0` and therefore `opening_margin == 0`, and the
    // gate reads `opening_margin > 0 && trial_wallet < opening_margin`. A pure close is affordable
    // at any balance, including a negative one (the B1 invariant) — which is what this pins.
    let mut bob_acc = storage::load_account(&mut ctx, BOB).unwrap();
    bob_acc.perp_wallet_balance = 0;
    storage::save_account(&mut ctx, BOB, bob_acc, AccountUpdateReason::Adjustment).unwrap();

    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    let maker = get_order(&mut ctx, sell_id);
    assert_eq!(maker.status, OrderStatus::PartiallyFilled);
    assert_eq!(maker.filled, QTY);
    assert_terminal(&mut ctx, buy_id);
    // The remaining QTY of the maker's sell is NOT auto-expired — it stays resting.
    assert!(storage::load_ask_prices(&mut ctx, MARKET_ID)
        .unwrap()
        .contains(&PRICE));
    assert!(!storage::load_ask_level(&mut ctx, MARKET_ID, PRICE)
        .unwrap()
        .is_empty());
}

#[test]
fn underwater_maker_close_routes_bad_debt_to_insurance_fund_not_wallet() {
    // Isolated margin end-to-end: an underwater position closed via a maker fill sends
    // its bad debt (loss beyond the position's margin) straight to the Insurance Fund;
    // the maker's wallet is never debited.
    let mut ctx = make_ctx();
    setup(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 10_000_000).unwrap();

    // BOB: long QTY entered at 2*PRICE (v_quote = -2*FILL_VALUE) with only 500_000
    // margin — deeply underwater at the current PRICE.
    storage::save_position(
        &mut ctx,
        BOB,
        MARKET_ID,
        &PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -((FILL_VALUE * 2) as i64),
            margin: 500_000,
            leverage: 1,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();

    // BOB rests a sell of QTY at PRICE (pure close → reserves nothing); ALICE buys it,
    // closing BOB's long at PRICE — a loss of 1e6 against a 500k margin.
    let _sell = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let buy = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert_terminal(&mut ctx, buy);

    // realised = margin_release(500k) + vq(-2e6) + close(+1e6) = -500k → bad debt 500k.
    let bob = pos(&mut ctx, BOB);
    assert_eq!(bob.amount, 0, "BOB flat");
    assert_eq!(bob.margin, 0, "position margin fully consumed by the loss");
    assert_eq!(
        wallet(&mut ctx, BOB),
        WALLET,
        "isolated margin: the loss never debited BOB's wallet"
    );
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        10_000_000 - 500_000,
        "the 500k bad debt was absorbed by the Insurance Fund"
    );
}

/// CHANGED BY THE ESCROW REMOVAL — the failure mode this test guarded CANNOT EXIST any more.
///
/// It was the brute-force MINIMUM "maker reserve deficit" scenario: under the old max-of-side
/// reservation a cross-side flip fill could find the stored reservation smaller than the fill
/// needed, firing a mid-fill deficit debit; formula C (`max(S + B', B + S')`) fixed that by
/// over-collecting up front. With no stored reservation at all there is nothing to be short of,
/// and formula C itself is gone. What is still worth pinning is the SCENARIO: a fill that flips
/// the position's sign while orders rest on both sides must leave every other resting order
/// untouched and conserve value exactly.
///
/// The fixture now needs a LIVE MARK (`setup()` leaves it 0, which blinds the derived basis to
/// the position — the whole subject here) and therefore a widened price band, since the fills
/// deliberately span $250–$300.
///
/// Numbers that moved, and why:
///   * up-front commitment 8e6 → **6e6**. At mark $250 with `N = +2.5e6`, `Bid = 6e6`,
///     `Ask = 9e6`: `IM = max(|2.5+6|, |2.5−9|) = 8.5e6`, `PIM = 2.5e6`, `ooIM = 6e6`. Formula C
///     added a further 2e6 for the buy leg re-opening after a total sell-side flip; Binance's
///     joint max already contains that path and does not double count it.
///   * ALICE's wallet after the flip 12.5e6 → **17.5e6**, because 5e6 of escrow was never taken
///     out of it. Her AVAILABLE (14.5e6) is what to compare, and the two ledgers agree on every
///     unit of value — asserted below.
#[test]
fn a_cross_side_flip_fill_leaves_the_other_resting_orders_alone() {
    //   Plo = $200 (buy level, below market)
    //   Pm  = $250 (Alice opens long here against Bob, and the mark)
    //   Phi = $300 (sell level, above market)  -> own book uncrossed (200<300)
    let mut ctx = make_ctx();
    setup(&mut ctx);
    // A live mark, and a band wide enough for the $250–$300 spread the scenario needs.
    let mut market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    market.mark_price = 250 * TICK;
    market.price_band_bps = 1_000_000;
    storage::save_market(&mut ctx, &market).unwrap();

    fund(&mut ctx, BOB, WALLET * 100);
    fund(&mut ctx, ALICE, WALLET);

    let plo = 200 * TICK;
    let pm = 250 * TICK;
    let phi = 300 * TICK;

    // (1) Alice opens a long of 1*QTY at Pm by lifting Bob's resting ask.
    let _bob_open = place(&mut ctx, BOB, 1, pm, QTY, 0, 0); // Bob sells (maker)
    let alice_open = place(&mut ctx, ALICE, 0, pm, QTY, 0, 0); // Alice buys (taker)
    assert_terminal(&mut ctx, alice_open);
    assert_eq!(pos(&mut ctx, ALICE).amount, QTY as i64, "Alice long +1");
    assert_eq!(wallet(&mut ctx, ALICE), 17_500_000, "20e6 − 2.5e6 margin");

    // (2) Alice rests buys at Plo (below market, no cross): qty 1 then qty 2.
    let alice_buy1 = place(&mut ctx, ALICE, 0, plo, QTY, 0, 0);
    let alice_buy2 = place(&mut ctx, ALICE, 0, plo, QTY * 2, 0, 0);
    // (3) Alice rests sells at Phi (above market, no cross): qty 2 FIRST (FIFO
    //     fills it), then qty 1.
    let alice_sell2 = place(&mut ctx, ALICE, 1, phi, QTY * 2, 0, 0);
    let alice_sell1 = place(&mut ctx, ALICE, 1, phi, QTY, 0, 0);

    // own book is uncrossed: best bid Plo < best ask Phi
    assert!(plo < phi);
    for id in [alice_buy1, alice_buy2, alice_sell2, alice_sell1] {
        assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open);
    }

    let pos_before = pos(&mut ctx, ALICE);
    assert_eq!(pos_before.amount, QTY as i64);
    assert_eq!(
        (
            pos_before.total_buy_notional,
            pos_before.total_sell_notional
        ),
        (6_000_000, 9_000_000)
    );
    // IM = max(|2.5e6 + 6e6|, |2.5e6 − 9e6|) = 8.5e6; PIM = 2.5e6 ⇒ ooIM = 6e6. The three sells
    // beyond the long are already inside the bid branch, so the last two cost nothing extra.
    assert_eq!(oo_im(&mut ctx, ALICE), 6_000_000);
    assert_eq!(
        wallet(&mut ctx, ALICE),
        17_500_000,
        "resting debited nothing"
    );
    assert_eq!(available(&mut ctx, ALICE), 11_500_000);

    // (4) Bob (a DIFFERENT taker) buys 2*QTY at Phi, lifting Alice's qty-2 ask.
    //     This closes 1*QTY of Alice's long and opens 1*QTY short => FLIP to -1.
    let bob_take = place(&mut ctx, BOB, 0, phi, QTY * 2, 0, 0);
    assert_terminal(&mut ctx, bob_take);
    assert_terminal(&mut ctx, alice_sell2);

    // Position flipped sign: +1 long -> -1 short.
    let pos_after = pos(&mut ctx, ALICE);
    assert_eq!(pos_after.amount, -(QTY as i64), "sign flip +1 -> -1");
    assert_eq!(
        pos_after.margin, 3_000_000,
        "the new short's own margin, at $300"
    );
    // The maker fill funded that 3e6 FROM THE WALLET (there is no escrow to draw on) and the
    // close returned 2.5e6 of margin + 0.5e6 of realised profit, so the wallet nets +0.5e6 ...
    assert_eq!(wallet(&mut ctx, ALICE), 17_500_000);
    // ... and the residual book is repriced against the new SHORT — through `N`, and ONLY through
    // `N`.
    //
    // **This is the R12 freeze, on a real match.** The fill PRINTED at $300, which lifts the
    // Assuming-Price floor to `T = ROUND_UP($300 × 1.0015) = $300.45` — above the $300 limit of
    // ALICE's surviving sell. Under the refuted `H_live` reading the read path would re-resolve `T`
    // and charge that sell 3_004_500. It does not: the sell was placed when the last print was $250
    // (`T = $250.375`, below $300), so its `assuming_price` is frozen at its own $300 limit and its
    // term stays 3_000_000 no matter what prints afterwards.
    // Bid 6e6, Ask 3_000_000, N = −2.5e6 ⇒ IM = max(|−2.5+6|, |−2.5−3.0|) = 5_500_000,
    // PIM = 2.5e6 ⇒ ooIM = 3_000_000.
    assert_eq!(oo_im(&mut ctx, ALICE), 3_000_000);
    assert_ne!(
        oo_im(&mut ctx, ALICE),
        3_004_500,
        "a print AFTER the order rested must not reprice it (R12)"
    );
    assert_eq!(available(&mut ctx, ALICE), 14_500_000);

    // Conservation across the flip: ALICE's wallet + position margin grew by exactly the 0.5e6
    // she realised (long opened at $250, closed at $300), and by nothing else.
    assert_eq!(
        wallet(&mut ctx, ALICE) as i128 + pos_after.margin as i128,
        (WALLET * 2) as i128 + 500_000
    );

    // No auto-cancel: every other resting order is untouched.
    assert_eq!(get_order(&mut ctx, alice_sell1).status, OrderStatus::Open);
    assert_eq!(get_order(&mut ctx, alice_buy1).status, OrderStatus::Open);
    assert_eq!(get_order(&mut ctx, alice_buy2).status, OrderStatus::Open);
}

#[test]
fn fifo_queue_fills_earlier_order_first() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);

    // Bob and Carol both rest sells at the same price.
    let bob_id = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let carol_id = place(&mut ctx, CAROL, 1, PRICE, QTY, 0, 0);

    // Alice buys QTY — should hit Bob's order first (FIFO).
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    assert_terminal(&mut ctx, bob_id);
    assert_eq!(get_order(&mut ctx, carol_id).status, OrderStatus::Open);
}

#[test]
fn self_trade_finalizes_taker_from_latest_maker_state() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let sell_id = place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);
    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    assert_terminal(&mut ctx, sell_id);
    assert_terminal(&mut ctx, buy_id);
    assert_eq!(wallet(&mut ctx, ALICE), WALLET);
    assert_eq!(
        pos(&mut ctx, ALICE),
        PerpPosition {
            leverage: 1,
            ..PerpPosition::default()
        }
    );
}

#[test]
fn self_trade_taker_margin_expiry_does_not_cancel_current_taker_order() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let old_buy_price = PRICE - 2 * TICK;
    let old_buy = place(&mut ctx, ALICE, 0, old_buy_price, QTY, 0, 0);
    let self_sell = place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);
    let bob_sell = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);

    // CHANGED BY THE ESCROW REMOVAL: the hand-set balance was the post-escrow leftover, i.e. the
    // available. See `taker_fill_cancels_worst_same_side_order_to_cover_opening_margin`.
    let old_buy_margin = 980_000;
    set_available(&mut ctx, ALICE, (INIT_MARGIN - old_buy_margin) as i64);

    let taker_buy = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 0);

    assert_terminal(&mut ctx, self_sell);
    assert_terminal(&mut ctx, bob_sell);
    assert_terminal(&mut ctx, taker_buy);
    assert_terminal(&mut ctx, old_buy);
    assert!(storage::load_bid_prices(&mut ctx, MARKET_ID)
        .unwrap()
        .is_empty());
    assert_eq!(
        pos(&mut ctx, ALICE),
        PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -(FILL_VALUE as i64),
            margin: INIT_MARGIN as i64,
            leverage: 1,
            ..PerpPosition::default()
        }
    );
    assert_eq!(wallet(&mut ctx, ALICE), INIT_MARGIN - old_buy_margin);
}

#[test]
fn taker_margin_expiry_records_mid_when_best_bid_is_cleared() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);

    let old_buy_price = PRICE - 2 * TICK;
    let old_buy = place(&mut ctx, ALICE, 0, old_buy_price, QTY, 0, 0);
    let bob_sell = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let carol_sell = place(&mut ctx, CAROL, 1, PRICE, QTY, 0, 0);

    // CHANGED BY THE ESCROW REMOVAL: the hand-set balance was the post-escrow leftover, i.e. the
    // available. See `taker_fill_cancels_worst_same_side_order_to_cover_opening_margin`.
    let old_buy_margin = 980_000;
    set_available(&mut ctx, ALICE, (INIT_MARGIN - old_buy_margin) as i64);

    ctx.block.timestamp = U256::from(10);
    let taker_buy = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    assert_terminal(&mut ctx, taker_buy);
    assert_terminal(&mut ctx, bob_sell);
    assert_eq!(get_order(&mut ctx, carol_sell).status, OrderStatus::Open);
    assert_terminal(&mut ctx, old_buy);
    assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), 0);
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), PRICE);

    let window = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(window.last_sample_ts, 10);
    assert_eq!(window.last_mid_price, PRICE);
}

#[test]
fn market_buy_matches_lowest_ask_first() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let low_price = PRICE;
    let high_price = PRICE + 10 * TICK;

    let low_sell = place(&mut ctx, BOB, 1, low_price, QTY, 0, 0);
    let _hi_sell = place(&mut ctx, BOB, 1, high_price, QTY, 0, 0);

    // Market IOC buy — should hit the lowest ask.
    let mkt_buy = place(&mut ctx, ALICE, 0, 0, QTY, 1, 1); // orderType=Market, tif=IOC

    assert_terminal(&mut ctx, low_sell);
    assert_terminal(&mut ctx, mkt_buy);
    // High-price level must still be present.
    assert!(storage::load_ask_prices(&mut ctx, MARKET_ID)
        .unwrap()
        .contains(&high_price));
}

// ── IOC ───────────────────────────────────────────────────────────────────

#[test]
fn matching_saves_last_traded_price_after_final_fill_price() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);

    let low_price = PRICE;
    let high_price = PRICE + 10 * TICK;

    place(&mut ctx, BOB, 1, low_price, QTY, 0, 0);
    place(&mut ctx, CAROL, 1, high_price, QTY, 0, 0);

    place(&mut ctx, ALICE, 0, high_price, QTY * 2, 0, 0);

    assert_eq!(
        storage::load_last_traded_price(&mut ctx, MARKET_ID).unwrap(),
        high_price
    );
}

#[test]
fn ioc_with_no_liquidity_is_immediately_cancelled() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 1); // IOC, no asks
                                                          // Expired (no liquidity) → deleted under delete-on-terminal; no fill → no position.
    assert_terminal(&mut ctx, id);
    assert_eq!(
        pos(&mut ctx, ALICE).amount,
        0,
        "IOC with no liquidity fills nothing"
    );
}

#[test]
fn ioc_partial_fill_cancels_remainder() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // only QTY available

    // IOC buy for 2×QTY: fills QTY, remainder expired.
    let id = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 1);
    // Expired (remainder) → deleted; the QTY that DID fill is pinned via the opened position.
    assert_terminal(&mut ctx, id);
    assert_eq!(
        pos(&mut ctx, ALICE).amount,
        QTY as i64,
        "IOC filled QTY before expiring the remainder"
    );
}

// ── FOK ───────────────────────────────────────────────────────────────────

#[test]
fn fok_rejected_when_insufficient_liquidity() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // only QTY available

    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY * 2,
        orderType: 0,
        tif: 2, // FOK
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("FOK order cannot be fully filled"),
        "{err}"
    );
}

#[test]
fn fok_fully_fills_when_sufficient_liquidity() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    // Bob places sell for 2×QTY; both wallets have enough for 2×INIT_MARGIN.
    let sell_id = place(&mut ctx, BOB, 1, PRICE, QTY * 2, 0, 0);
    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 2); // FOK

    assert_terminal(&mut ctx, buy_id);
    assert_terminal(&mut ctx, sell_id);
}

// ── PostOnly ──────────────────────────────────────────────────────────────

#[test]
fn post_only_rejected_if_would_immediately_match() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0); // resting bid at PRICE

    // PostOnly sell at PRICE would cross → rejected.
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 1,
        price: PRICE,
        quantity: QTY,
        orderType: 0,
        tif: 3, // PostOnly
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("PostOnly order would match"),
        "{err}"
    );
}

#[test]
fn post_only_rests_when_above_best_bid() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0); // resting bid at PRICE

    // PostOnly sell one tick above the best bid — should rest.
    let ask_price = PRICE + TICK;
    let id = place(&mut ctx, ALICE, 1, ask_price, QTY, 0, 3); // PostOnly
    assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open);
    assert_eq!(
        storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![ask_price]
    );
}

// ── (orderType, tif) legality — the OrderKind boundary ────────────────────
//
// The ABI carries `orderType` and `tif` as two independent `uint8`s, but only SIX of their eight
// combinations name a product. `validate_place_order` collapses the pair into ONE
// `types::OrderKind`, so the two that don't — `Market + Fok` (all-or-nothing at market: no exchange
// sells it) and `Market + PostOnly` ("never take liquidity" on the one order type that only ever
// takes it) — die at the boundary and are UNREPRESENTABLE past it. `Market + Gtc` survives as the
// unset-tif placeholder and means exactly `Market`.
//
// The type-level half of this guarantee (that `OrderKind::Market` has no TIF slot at all, so no
// future edit can reintroduce the combination) is pinned in `perp_core::types::order`.

/// Place with every wire field free, returning the raw result (`Err` = reject).
fn try_place_pair(
    ctx: &mut TestCtx,
    caller: Address,
    side: u8,
    price: u64,
    qty: u64,
    order_type: u8,
    tif: u8,
) -> Result<Bytes, PerpError> {
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side,
        price,
        quantity: qty,
        orderType: order_type,
        tif,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    run_place_order(&input, caller, ctx)
}

/// The wire `(orderType, tif)` each `OrderPlaced` echoes, in emission order.
fn placed_type_tif(ctx: &mut TestCtx) -> Vec<(u8, u8)> {
    use crate::interface::IPerpDex::OrderPlaced;
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|l| l.data.topics().first() == Some(&OrderPlaced::SIGNATURE_HASH))
        .map(|l| {
            let e = OrderPlaced::decode_raw_log(l.data.topics(), &l.data.data).unwrap();
            (e.orderType, e.tif)
        })
        .collect()
}

/// **THE MATRIX.** All eight `(orderType, tif)` pairs, each attempted against a book deep enough
/// that nothing can reject for liquidity or margin — so the only rejects left are the structural
/// ones, and this test IS the legality table.
#[test]
fn the_order_type_tif_matrix_accepts_exactly_six_of_eight_pairs() {
    const LIMIT: u8 = 0;
    const MARKET: u8 = 1;
    const GTC: u8 = 0;
    const IOC: u8 = 1;
    const FOK: u8 = 2;
    const POST_ONLY: u8 = 3;
    // (orderType, tif, taker price, legal?)
    let cases: [(u8, u8, u64, bool); 8] = [
        (LIMIT, GTC, PRICE - 10 * TICK, true), // rests below the ask
        (LIMIT, IOC, PRICE, true),             // crosses and fills
        (LIMIT, FOK, PRICE, true),             // crosses and fills COMPLETELY
        (LIMIT, POST_ONLY, PRICE - 10 * TICK, true), // does not cross
        (MARKET, GTC, 0, true),                // the unset-tif placeholder
        (MARKET, IOC, 0, true),                // the canonical spelling
        (MARKET, FOK, 0, false),               // all-or-nothing at market: not a product
        (MARKET, POST_ONLY, 0, false),         // never-take on a take-only order: not a product
    ];
    for (order_type, tif, price, legal) in cases {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        place(&mut ctx, BOB, 1, PRICE, QTY * 4, 0, 0); // deep resting ask
        let got = try_place_pair(&mut ctx, ALICE, 0, price, QTY, order_type, tif);
        assert_eq!(
            got.is_ok(),
            legal,
            "(orderType {order_type}, tif {tif}) legality changed; got {got:?}"
        );
        if !legal {
            assert_eq!(
                got.unwrap_err().to_string(),
                "placeOrder: tif not allowed for market order",
                "(orderType {order_type}, tif {tif}) must reject with the boundary message"
            );
        }
    }
}

/// An illegal pair dies at the FIRST validation, so the reject is write-clean AND log-clean and
/// does not consume the order id it would have used (commit-only #23: same contract as every other
/// genuine reject).
#[test]
fn an_illegal_pair_is_rejected_before_any_write_or_log() {
    for tif in [2u8 /* FOK */, 3 /* PostOnly */] {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let resting = place(&mut ctx, BOB, 1, PRICE, QTY * 4, 0, 0);
        take_event_names(&mut ctx); // drain the fixture's own logs

        let err = try_place_pair(&mut ctx, ALICE, 0, 0, QTY, 1, tif).unwrap_err();
        assert_eq!(
            err.to_string(),
            "placeOrder: tif not allowed for market order"
        );

        assert!(
            take_event_names(&mut ctx).is_empty(),
            "an illegal (orderType, tif) pair must emit no logs at all"
        );
        assert_eq!(get_order(&mut ctx, resting).filled, 0, "maker untouched");
        assert_eq!(pos(&mut ctx, ALICE), PerpPosition::default());
        assert_eq!(wallet(&mut ctx, ALICE), WALLET);
        assert_eq!(
            storage::load_user_nonce(&mut ctx, ALICE).unwrap(),
            0,
            "a rejected placement must not burn an order id"
        );
    }
}

/// `Market + Gtc` is the unset-tif placeholder, NOT a promise to rest. It must be the same order as
/// `Market + Ioc` in every observable: the state it leaves, and the `tif` it reports.
#[test]
fn market_with_the_placeholder_tif_is_identical_to_market_ioc() {
    /// Half-fill a 2×QTY market buy against QTY of ask, and report everything observable.
    #[allow(clippy::type_complexity)]
    fn run(tif: u8) -> (PerpPosition, u64, Vec<(u8, u8)>, bool, Vec<u64>) {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // only QTY of liquidity
        JournalTr::take_logs(ctx.journal_mut()); // drain the fixture's logs
        let id = place(&mut ctx, ALICE, 0, 0, QTY * 2, 1, tif);
        (
            pos(&mut ctx, ALICE),
            wallet(&mut ctx, ALICE),
            placed_type_tif(&mut ctx),
            storage::load_order(&mut ctx, &id).unwrap().is_some(),
            storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(),
        )
    }
    let placeholder = run(0); // Market + Gtc
    let canonical = run(1); // Market + Ioc
    assert_eq!(
        placeholder, canonical,
        "Market + Gtc must be the very same order as Market + Ioc"
    );

    let (position, _, wire, still_stored, bids) = placeholder;
    // Half filled …
    assert_eq!(position.amount, QTY as i64);
    // … and the other half DISCARDED: no record, nothing resting, despite the wire saying "GTC".
    assert!(!still_stored, "a market remainder never rests");
    assert!(bids.is_empty(), "a market remainder never enters the book");
    // Reported as orderType = Market(1), tif = IOC(1): the placeholder is not echoed back as a
    // promise the engine does not keep.
    assert_eq!(wire, vec![(1, 1)]);
}

/// The collapse must not smear the four LIMIT time-in-forces together: each still differs in
/// exactly what happens to the half of a 2×QTY taker that QTY of resting ask cannot fill.
#[test]
fn each_limit_tif_keeps_its_own_behaviour_after_the_collapse() {
    // GTC — the unfilled half RESTS.
    {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        let id = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 0);
        assert_eq!(
            get_order(&mut ctx, id).status,
            OrderStatus::PartiallyFilled,
            "GTC rests its remainder"
        );
        assert_eq!(
            storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(),
            vec![PRICE]
        );
    }
    // IOC — the unfilled half is DISCARDED.
    {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        let id = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 1);
        assert_terminal(&mut ctx, id);
        assert!(storage::load_bid_prices(&mut ctx, MARKET_ID)
            .unwrap()
            .is_empty());
        assert_eq!(pos(&mut ctx, ALICE).amount, QTY as i64);
    }
    // FOK — all-or-nothing: the WHOLE order is rejected and the maker keeps its quantity.
    {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let maker = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        let err = try_place_pair(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 2).unwrap_err();
        assert!(
            err.to_string().contains("FOK order cannot be fully filled"),
            "{err}"
        );
        assert_eq!(get_order(&mut ctx, maker).filled, 0);
        assert_eq!(pos(&mut ctx, ALICE), PerpPosition::default());
    }
    // PostOnly — refuses to take liquidity at all.
    {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        let err = try_place_pair(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 3).unwrap_err();
        assert!(
            err.to_string().contains("PostOnly order would match"),
            "{err}"
        );
    }
}

// ── OrderPlaced emission: accepted-only (batch-trade Phase 0) ──────────────
//
// `OrderPlaced` is buffered by `announce_new_order` and flushed at the first APPLY point, so it is
// emitted IFF the placement is accepted while keeping its original stream position (first among its
// own order's events). The single-order selectors revert on `Err` and truncate the logs anyway; the
// batch selectors will CATCH the per-item error and return `Ok`, so an eagerly-emitted log would
// survive in the receipts for an order that never existed.

/// Drains the journal and maps each log to a short perp-event name, in emission order.
/// (`take_logs` drains, so a call reports only the events since the previous call.)
fn take_event_names(ctx: &mut TestCtx) -> Vec<&'static str> {
    use crate::interface::IPerpDex::{
        FundingSettled, InsuranceFundChanged, InsuranceFundDepleted, OrderCancelled, OrderPlaced,
        OrderRested, PositionChanged, Trade,
    };
    JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .map(|log| {
            let sig = log.data.topics().first().copied();
            let named: &[(&'static str, FixedBytes<32>)] = &[
                ("OrderPlaced", OrderPlaced::SIGNATURE_HASH),
                ("OrderRested", OrderRested::SIGNATURE_HASH),
                ("Trade", Trade::SIGNATURE_HASH),
                ("PositionChanged", PositionChanged::SIGNATURE_HASH),
                ("OrderCancelled", OrderCancelled::SIGNATURE_HASH),
                ("FundingSettled", FundingSettled::SIGNATURE_HASH),
                ("InsuranceFundChanged", InsuranceFundChanged::SIGNATURE_HASH),
                (
                    "InsuranceFundDepleted",
                    InsuranceFundDepleted::SIGNATURE_HASH,
                ),
                (
                    "AccountBalanceChanged",
                    AccountBalanceChanged::SIGNATURE_HASH,
                ),
            ];
            named
                .iter()
                .find(|(_, h)| sig == Some(*h))
                .map(|(n, _)| *n)
                .unwrap_or("Unknown")
        })
        .collect()
}

fn count_of(names: &[&str], want: &str) -> usize {
    names.iter().filter(|n| **n == want).count()
}

#[test]
fn rejected_post_only_placement_emits_no_logs() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0); // resting bid at PRICE
    take_event_names(&mut ctx); // drain BOB's accepted placement

    // PostOnly sell at PRICE would cross → rejected before any apply.
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 1,
        price: PRICE,
        quantity: QTY,
        orderType: 0,
        tif: 3, // PostOnly
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("PostOnly order would match"),
        "{err}"
    );

    let logs = take_event_names(&mut ctx);
    assert!(
        logs.is_empty(),
        "a rejected placement must emit no logs at all, got {logs:?}"
    );
}

#[test]
fn rejected_fok_placement_emits_no_logs() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // only QTY available
    take_event_names(&mut ctx);

    // FOK buy for 2×QTY cannot be fully filled → rejected.
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY * 2,
        orderType: 0,
        tif: 2, // FOK
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(
        err.to_string().contains("FOK order cannot be fully filled"),
        "{err}"
    );

    let logs = take_event_names(&mut ctx);
    assert!(
        logs.is_empty(),
        "a rejected FOK placement must emit no logs at all, got {logs:?}"
    );
}

/// The deepest reject: the walk has already computed its fills when `finalize_compute` rejects the
/// taker on wallet cover — still pre-flush, so nothing (not even `OrderPlaced`) may be emitted.
#[test]
fn rejected_taker_wallet_cover_emits_no_logs() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // resting ask

    let mut alice = storage::load_account(&mut ctx, ALICE).unwrap();
    alice.perp_wallet_balance = (INIT_MARGIN - 1) as i64;
    storage::save_account(&mut ctx, ALICE, alice, AccountUpdateReason::Adjustment).unwrap();
    take_event_names(&mut ctx);

    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "{err}"
    );

    let logs = take_event_names(&mut ctx);
    assert!(
        logs.is_empty(),
        "a taker rejected in finalize_compute must emit no logs at all, got {logs:?}"
    );
}

/// The zero-fill GTC case: nothing matched, so the rest-margin reject is raised by
/// `finalize_compute`'s empty-fill arm (pre-flush) rather than later in `rest_in_book` — either way
/// the buffered `OrderPlaced` must die with the reject, unemitted.
#[test]
fn rejected_rest_in_book_margin_emits_no_logs() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    take_event_names(&mut ctx);

    // Empty book → no fills; 11×QTY at leverage 1 needs 11 USDC of margin but WALLET is 10.
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY * 11,
        orderType: 0,
        tif: 0, // GTC
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "{err}"
    );

    let logs = take_event_names(&mut ctx);
    assert!(
        logs.is_empty(),
        "a placement rejected by rest_in_book must emit no logs at all, got {logs:?}"
    );
}

/// A zero-fill GTC whose rest is unaffordable must reject WRITE-CLEAN, even when the match walk
/// entered a level (and therefore recorded writes).
///
/// The walk pushes a `SaveLevel` for every level it enters — here the crossing ask level survives
/// the walk with its live count intact because its only queued id is stale (lazy-queue sweep), so
/// nothing fills. Those writes used to be committed by `registry.flush` and only afterwards did
/// `rest_in_book` raise the perfectly ordinary margin reject: under commit-only they LEAKED (the
/// frame revert does not roll perp state back) and inside a batch they faked an `Aborted`. The rest
/// is now validated in `finalize_compute` before the flush, so the reject writes nothing.
#[test]
fn zero_fill_rest_margin_reject_is_write_clean() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    // A resting ask at PRICE whose order RECORD is then removed: the level keeps its live count and
    // its FIFO id, so a crossing taker enters the level, sweeps the stale id and fills nothing.
    let stale = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    storage::delete_order(&mut ctx, &stale).unwrap();
    take_event_names(&mut ctx);
    let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

    // Crosses that level (no fill), then wants to rest 11 USDC of margin against a 10 USDC wallet.
    let input = placeOrderCall {
        marketId: MARKET_ID,
        side: 0,
        price: PRICE,
        quantity: QTY * 11,
        orderType: 0,
        tif: 0, // GTC
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    let err = run_place_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for margin"),
        "{err}"
    );
    assert_eq!(
        JournalTr::perp_write_count(ctx.journal_mut()),
        writes_before,
        "the zero-fill rest-margin reject must fire BEFORE the match flush (no leaked writes)"
    );
    let logs = take_event_names(&mut ctx);
    assert!(logs.is_empty(), "and it must emit nothing, got {logs:?}");
}

#[test]
fn accepted_match_emits_order_placed_before_its_trade() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // resting ask
    take_event_names(&mut ctx);

    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // GTC buy, fully fills

    let logs = take_event_names(&mut ctx);
    assert_eq!(
        count_of(&logs, "OrderPlaced"),
        1,
        "exactly one OrderPlaced, got {logs:?}"
    );
    let placed = logs.iter().position(|n| *n == "OrderPlaced").unwrap();
    let trade = logs
        .iter()
        .position(|n| *n == "Trade")
        .unwrap_or_else(|| panic!("expected a Trade, got {logs:?}"));
    let pos_changed = logs
        .iter()
        .position(|n| *n == "PositionChanged")
        .unwrap_or_else(|| panic!("expected a PositionChanged, got {logs:?}"));
    assert!(
        placed < trade && placed < pos_changed,
        "OrderPlaced must precede its Trade/PositionChanged, got {logs:?}"
    );
}

#[test]
fn accepted_resting_placement_emits_exactly_one_order_placed() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    take_event_names(&mut ctx);

    // Empty book → no fill, rests (flushed at match_order's apply, no-op at rest_in_book's).
    // The call is opened and closed around each placement so the account-snapshot drain really does
    // run — without it, "no `AccountBalanceChanged`" would be true for the trivial reason that this
    // helper bypasses the shell.
    start_call(&mut ctx);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // GTC
    end_call(&mut ctx);
    let logs = take_event_names(&mut ctx);
    assert_eq!(
        logs,
        vec!["OrderPlaced", "OrderRested"],
        "a pure placement emits its two ORDER events and NO `AccountBalanceChanged`. Not because \
         nothing observable moved — resting does raise `Σ ooIM` and therefore does move \
         `availableBalance` — but because we match a measured venue: R14 (Binance mainnet, \
         2026-08-21) recorded a pure placement pushing only `ORDER_TRADE_UPDATE x=NEW` with no \
         `ACCOUNT_UPDATE`, and the official docs say it verbatim — \"Unfilled orders or cancelled \
         orders will not make the event `ACCOUNT_UPDATE` pushed, since there's no change on \
         positions.\" `availableBalance` is therefore stream-stale between fills, by decision; \
         `getAccount` is its source of truth. See `storage::mark_account_snapshot_dirty`"
    );

    // PostOnly never calls match_order → rest_in_book's apply block is the ONLY flush site.
    start_call(&mut ctx);
    place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 3); // PostOnly
    end_call(&mut ctx);
    let logs = take_event_names(&mut ctx);
    assert_eq!(
        logs,
        vec!["OrderPlaced", "OrderRested"],
        "same for PostOnly, which never matches: no account event either"
    );
}

/// Two apply sites are reached (match flush, then the rest) — the `take()` must keep it at one.
#[test]
fn accepted_partial_fill_then_rest_emits_exactly_one_order_placed() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // only QTY available
    take_event_names(&mut ctx);

    place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 0); // GTC buy: fills QTY, rests QTY

    let logs = take_event_names(&mut ctx);
    assert_eq!(
        count_of(&logs, "OrderPlaced"),
        1,
        "exactly one OrderPlaced across both apply sites, got {logs:?}"
    );
    assert_eq!(logs.first().copied(), Some("OrderPlaced"), "{logs:?}");
    assert_eq!(
        count_of(&logs, "OrderRested"),
        1,
        "the remainder rested, got {logs:?}"
    );
}

#[test]
fn accepted_ioc_expiring_with_no_fill_still_emits_one_order_placed() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    take_event_names(&mut ctx);

    // IOC against an empty book: accepted, matches nothing, rests nothing, expires.
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 1); // IOC
    let logs = take_event_names(&mut ctx);
    assert_eq!(
        logs,
        vec!["OrderPlaced"],
        "an accepted IOC that expired unfilled still emits exactly its OrderPlaced"
    );

    // Same for a market order with no book.
    place(&mut ctx, ALICE, 0, 0, QTY, 1, 1); // Market/IOC
    let logs = take_event_names(&mut ctx);
    assert_eq!(
        logs,
        vec!["OrderPlaced"],
        "an accepted market order that expired unfilled still emits exactly its OrderPlaced"
    );
}

// ── Cancel ────────────────────────────────────────────────────────────────

/// CHANGED BY THE ESCROW REMOVAL: the wallet no longer moves in either direction, so the
/// place/cancel round-trip is asserted on the AVAILABLE (`wallet − Σ ooIM`) instead.
#[test]
fn cancel_resting_order_frees_the_requirement_and_clears_book() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert!(
        available(&mut ctx, ALICE) < WALLET as i128,
        "the order should be holding a requirement"
    );
    assert_eq!(wallet(&mut ctx, ALICE), WALLET, "and holding no cash");

    let input = cancelOrderCall {
        orderId: id.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    run_cancel_order(&input, ALICE, &mut ctx).unwrap();

    assert_eq!(
        available(&mut ctx, ALICE),
        WALLET as i128,
        "the requirement should be released"
    );
    assert_eq!(wallet(&mut ctx, ALICE), WALLET);
    assert_terminal(&mut ctx, id);
    assert!(storage::load_bid_prices(&mut ctx, MARKET_ID)
        .unwrap()
        .is_empty());
}

#[test]
fn lazy_queue_sweeps_cancelled_maker_on_match() {
    // Two asks at the same price. Cancelling the first DELETES its record but leaves the id in the
    // level FIFO (lazy-queue) — only the live count drops to 1. A buy taker then walks the level,
    // skips the stale id (load_order → None), fills the survivor, and empties the level. Exercises
    // the None→skip sweep + per-level count end-to-end.
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let ask1 = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let ask2 = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    assert_eq!(
        storage::load_ask_count(&mut ctx, MARKET_ID, PRICE).unwrap(),
        2,
        "both live"
    );

    // Cancel the first: deleted from the map, count → 1, id lingers in the FIFO (lazy).
    run_cancel_order(
        &cancelOrderCall {
            orderId: ask1.into(),
            marketId: MARKET_ID,
        }
        .abi_encode(),
        BOB,
        &mut ctx,
    )
    .unwrap();
    assert_terminal(&mut ctx, ask1);
    assert_eq!(
        storage::load_ask_count(&mut ctx, MARKET_ID, PRICE).unwrap(),
        1,
        "count decremented in O(1)"
    );
    assert_eq!(
        storage::load_ask_level(&mut ctx, MARKET_ID, PRICE)
            .unwrap()
            .len(),
        2,
        "stale id still in the FIFO (lazy-queue, not eagerly removed)"
    );
    assert_eq!(
        storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![PRICE],
        "level still present (count > 0)"
    );

    // Buy taker for QTY: sweeps the stale ask1 (skip) and fills the live ask2 → level empties.
    let taker = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert_terminal(&mut ctx, taker); // fully filled
    assert_terminal(&mut ctx, ask2); // filled maker deleted
    assert_eq!(
        pos(&mut ctx, ALICE).amount,
        QTY as i64,
        "taker opened exactly QTY (stale maker contributed nothing)"
    );
    assert_eq!(
        storage::load_ask_count(&mut ctx, MARKET_ID, PRICE).unwrap(),
        0,
        "level empty"
    );
    assert!(
        storage::load_ask_prices(&mut ctx, MARKET_ID)
            .unwrap()
            .is_empty(),
        "price removed from index"
    );
    assert!(
        storage::load_ask_level(&mut ctx, MARKET_ID, PRICE)
            .unwrap()
            .is_empty(),
        "queue cleared on empty"
    );
}

#[test]
fn cancel_non_top_bid_keeps_best_bid() {
    // Cancelling a strictly-interior bid level (price < best_bid) must NOT move
    // best_bid. On the explicit cancel path (Current cache) remove_from_book skips
    // the refresh here; the cached best must remain the untouched top level.
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let p_lo = PRICE;
    let p_hi = PRICE + TICK;

    let lo = place(&mut ctx, ALICE, 0, p_lo, QTY, 0, 0);
    let hi = place(&mut ctx, ALICE, 0, p_hi, QTY, 0, 0);
    assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), p_hi);

    let input = cancelOrderCall {
        orderId: lo.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    run_cancel_order(&input, ALICE, &mut ctx).unwrap();

    // best_bid unchanged (top level survived), interior level gone, top still open.
    assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), p_hi);
    assert_eq!(
        storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![p_hi]
    );
    assert_terminal(&mut ctx, lo);
    assert_eq!(get_order(&mut ctx, hi).status, OrderStatus::Open);
}

#[test]
fn cancel_top_bid_refreshes_best_bid() {
    // Cancelling the top bid level (price == best_bid) MUST refresh best_bid down
    // to the next surviving level.
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let p_lo = PRICE;
    let p_hi = PRICE + TICK;

    let _lo = place(&mut ctx, ALICE, 0, p_lo, QTY, 0, 0);
    let hi = place(&mut ctx, ALICE, 0, p_hi, QTY, 0, 0);
    assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), p_hi);

    let input = cancelOrderCall {
        orderId: hi.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    run_cancel_order(&input, ALICE, &mut ctx).unwrap();

    assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), p_lo);
    assert_eq!(
        storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![p_lo]
    );
}

#[test]
fn cancel_non_top_ask_keeps_best_ask() {
    // Ask orientation is mirrored: asks sort ASC so best_ask is the LOWEST. A
    // non-top ask has price > best_ask; cancelling it must NOT move best_ask.
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let p_lo = PRICE; // best ask (lowest)
    let p_hi = PRICE + TICK; // interior (higher) ask

    let _lo = place(&mut ctx, ALICE, 1, p_lo, QTY, 0, 0);
    let hi = place(&mut ctx, ALICE, 1, p_hi, QTY, 0, 0);
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), p_lo);

    let input = cancelOrderCall {
        orderId: hi.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    run_cancel_order(&input, ALICE, &mut ctx).unwrap();

    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), p_lo);
    assert_eq!(
        storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![p_lo]
    );
    assert_terminal(&mut ctx, hi);
}

#[test]
fn cancel_top_ask_refreshes_best_ask() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let p_lo = PRICE;
    let p_hi = PRICE + TICK;

    let lo = place(&mut ctx, ALICE, 1, p_lo, QTY, 0, 0);
    let _hi = place(&mut ctx, ALICE, 1, p_hi, QTY, 0, 0);
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), p_lo);

    let input = cancelOrderCall {
        orderId: lo.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    run_cancel_order(&input, ALICE, &mut ctx).unwrap();

    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), p_hi);
}

#[test]
fn remove_from_book_after_cancel_rejects_bid_above_cached_best() {
    // Defensive tripwire: on the cancel path (cache assumed live), a removed bid
    // level above the cached best_bid means the cache was actually stale — an
    // invariant violation, not a normal cancel. Guards against a future caller
    // routing a stale cache through remove_from_book_after_cancel (which would
    // silently corrupt the BBO via a wrong skip).
    let mut ctx = make_ctx();
    setup(&mut ctx);
    let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    // Force the cache stale-low (below the resting bid at PRICE).
    storage::save_best_bid(&mut ctx, MARKET_ID, PRICE - TICK).unwrap();

    let err = super::remove_from_book_after_cancel(
        &mut ctx,
        MARKET_ID,
        crate::types::Side::Buy,
        PRICE,
        &id,
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("above cached best_bid"),
        "expected invariant error, got: {err}"
    );
}

#[test]
fn cancel_rejects_non_owner() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let input = cancelOrderCall {
        orderId: id.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    let err = run_cancel_order(&input, BOB, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("not owner"), "{err}");
}

#[test]
fn cancel_rejects_already_filled_order() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let sell_id = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // fills Bob's sell

    let input = cancelOrderCall {
        orderId: sell_id.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    let err = run_cancel_order(&input, BOB, &mut ctx).unwrap_err();
    // delete-on-terminal: the filled order was removed, so cancel now reports "order not found"
    // (was "not cancellable" when the Filled record lingered). Both reject.
    assert!(err.to_string().contains("order not found"), "{err}");
}

#[test]
fn cancel_rejects_nonexistent_order() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let input = cancelOrderCall {
        orderId: [0xab_u8; 32].into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    let err = run_cancel_order(&input, ALICE, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("order not found"), "{err}");
}

// ── getOrder ──────────────────────────────────────────────────────────────

#[test]
fn get_order_returns_all_correct_fields() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    let input = getOrderCall {
        orderId: id.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    let ret = run_get_order(&input, &mut ctx).unwrap();

    // ABI layout: 7 × 32-byte slots.
    // address is right-aligned: leading 12 zero bytes then 20 address bytes.
    let owner = Address::from_slice(&ret[12..32]);
    let market_id = U256::from_be_slice(&ret[32..64]).to::<u64>();
    let side = U256::from_be_slice(&ret[64..96]).to::<u8>();
    let price = U256::from_be_slice(&ret[96..128]).to::<u64>();
    let qty = U256::from_be_slice(&ret[128..160]).to::<u64>();
    let filled = U256::from_be_slice(&ret[160..192]).to::<u64>();
    let status = U256::from_be_slice(&ret[192..224]).to::<u8>();

    assert_eq!(owner, ALICE);
    assert_eq!(market_id, MARKET_ID);
    assert_eq!(side, 0u8); // Buy
    assert_eq!(price, PRICE);
    assert_eq!(qty, QTY);
    assert_eq!(filled, 0u64);
    assert_eq!(status, 0u8); // Open
}

#[test]
fn get_order_rejects_nonexistent_order() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let input = getOrderCall {
        orderId: [0u8; 32].into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    let err = run_get_order(&input, &mut ctx).unwrap_err();
    assert!(err.to_string().contains("order not found"), "{err}");
}

#[test]
fn get_open_orders_returns_user_order_entries() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let sell_price = PRICE + TICK;
    let sell_id = place(&mut ctx, ALICE, 1, sell_price, QTY * 2, 0, 0);

    let input = getOpenOrdersCall {
        user: ALICE,
        marketId: MARKET_ID,
    }
    .abi_encode();
    let ret = run_get_open_orders(&input, &mut ctx).unwrap();
    let decoded = getOpenOrdersCall::abi_decode_returns(&ret).unwrap();

    assert_eq!(
        decoded.orderIds,
        vec![FixedBytes(buy_id), FixedBytes(sell_id)]
    );
    assert_eq!(decoded.sides, vec![Side::Buy as u8, Side::Sell as u8]);
    assert_eq!(decoded.prices, vec![PRICE, sell_price]);
    assert_eq!(decoded.remainingQuantities, vec![QTY, QTY * 2]);
}

#[test]
fn get_book_prices_returns_matching_priority_order() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let low_bid = PRICE - TICK;
    let high_bid = PRICE;
    let low_ask = PRICE + TICK;
    let high_ask = PRICE + TICK * 2;

    place(&mut ctx, ALICE, 0, low_bid, QTY, 0, 0);
    place(&mut ctx, BOB, 0, high_bid, QTY, 0, 0);
    place(&mut ctx, ALICE, 1, high_ask, QTY, 0, 0);
    place(&mut ctx, BOB, 1, low_ask, QTY, 0, 0);

    let bids_input = getBookPricesCall {
        marketId: MARKET_ID,
        side: Side::Buy as u8,
    }
    .abi_encode();
    let bids_ret = run_get_book_prices(&bids_input, &mut ctx).unwrap();
    let bids = getBookPricesCall::abi_decode_returns(&bids_ret).unwrap();

    let asks_input = getBookPricesCall {
        marketId: MARKET_ID,
        side: Side::Sell as u8,
    }
    .abi_encode();
    let asks_ret = run_get_book_prices(&asks_input, &mut ctx).unwrap();
    let asks = getBookPricesCall::abi_decode_returns(&asks_ret).unwrap();

    assert_eq!(bids, vec![high_bid, low_bid]);
    assert_eq!(asks, vec![low_ask, high_ask]);
}

#[test]
fn get_book_level_returns_fifo_order_ids() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let first = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    let second = place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0);

    let input = getBookLevelCall {
        marketId: MARKET_ID,
        side: Side::Buy as u8,
        price: PRICE,
    }
    .abi_encode();
    let ret = run_get_book_level(&input, &mut ctx).unwrap();
    let decoded = getBookLevelCall::abi_decode_returns(&ret).unwrap();

    assert_eq!(decoded, vec![FixedBytes(first), FixedBytes(second)]);
}

// ── Off-trie PerpState (journal perp section) integration ────────────────────

#[test]
fn perp_data_stays_off_trie_not_in_evm_state() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    // A resting buy order writes order, book level, best-bid, etc. — all perp blobs.
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    // Perp writes are captured in the off-trie delta...
    let delta = ctx.journal_mut().take_perp_delta();
    assert!(
        !delta.is_empty(),
        "perp writes must land in the off-trie delta"
    );
    // `place` drives `run_place_order` directly (no dispatch). #16d: the commitment is folded ONCE
    // at block end — finalize the harvested net delta onto the 0x1003 slot, as the executor would.
    storage::finalize_block_commitment(&mut ctx, &delta).unwrap();

    // ...while the trie-bound EvmState carries only the single chained
    // commitment anchor under 0x1003 (keccak256("cmit"), commit a7b0699d). The
    // bulk perp data (orders, book levels, best-bid, …) lives off-trie and
    // never enters the state root.
    let state = ctx.journal_mut().finalize();
    let perp_storage = state.get(&PERP_DEX_ADDRESS).map(|acc| &acc.storage);
    let slot_count = perp_storage.map(|s| s.len()).unwrap_or(0);
    assert_eq!(
        slot_count, 1,
        "only the on-trie commitment anchor may live in the state trie"
    );
    let commitment = U256::from_be_bytes(storage::keys::commitment_slot().0);
    assert!(
        perp_storage.unwrap().contains_key(&commitment),
        "the sole on-trie perp slot must be the commitment anchor"
    );
}

#[test]
fn perp_writes_survive_enclosing_revert_commit_only() {
    // commit-only (#23): a successful placement's perp writes SURVIVE an enclosing frame
    // revert (there is no perp undo). On-chain the EOA-direct depth guard forbids enclosing
    // frames entirely; this documents the journal-level semantics.
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let cp = ctx.journal_mut().checkpoint();
    let order_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    ctx.journal_mut().checkpoint_revert(cp);
    assert!(
        storage::load_order(&mut ctx, &order_id).unwrap().is_some(),
        "perp writes are commit-only: they survive the enclosing revert"
    );
}

// ── Performance baseline harness ──────────────────────────────────────────
//
// Run with:
//   cargo test -p revm-precompile --release perf_ -- --ignored --nocapture --test-threads=1
//
// Methodology: each scenario does an explicit warmup, then accumulates
// per-section `Instant` deltas over enough iterations for >=100ms of timed
// work. Steady state: scenarios that would otherwise grow the order book
// pair every "add" with a corresponding "drain" (cancel or full fill) so the
// book returns to empty every iteration. Order blobs and per-user nonces do
// accumulate in the block-scoped perp overlay (a realistic in-block effect);
// positions/wallets drift monotonically (wallets are pre-funded huge so no
// margin rejection ever triggers).
mod perf {
    use super::*;
    use crate::{
        interface::IPerpDex::{placeOrderSignedCall, updateIndexPriceCall},
        risk::run_update_index_price,
        types::ApiKey,
    };
    use std::time::{Duration, Instant};

    /// Extra funding so margin never runs out (~1.15e18 quote units).
    const BIG: u64 = 1 << 60;

    fn report(label: &str, total: Duration, iters: u64) -> f64 {
        let ns = total.as_nanos() as f64 / iters as f64;
        println!("PERF {label}: {iters} iters, total {total:?}, {ns:.0} ns/op");
        ns
    }

    fn place_input(side: u8, price: u64, qty: u64, order_type: u8, tif: u8) -> Vec<u8> {
        placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: order_type,
            tif,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode()
    }

    /// (a)+(d): GTC limit buy that rests (empty book), then cancelOrder.
    /// The two legs are timed separately inside the same loop, so the book is
    /// empty again at the start of every iteration (true steady state).
    #[test]
    #[ignore = "perf baseline; run with --release --ignored --nocapture"]
    fn perf_place_rest_then_cancel() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        fund(&mut ctx, ALICE, BIG);

        let input = place_input(0, PRICE, QTY, 0, 0);
        let warmup = 2_000u64;
        let iters = 20_000u64;
        let mut t_place = Duration::ZERO;
        let mut t_cancel = Duration::ZERO;

        for i in 0..(warmup + iters) {
            let timed = i >= warmup;
            let t0 = Instant::now();
            let ret = run_place_order(&input, ALICE, &mut ctx).unwrap();
            if timed {
                t_place += t0.elapsed();
            }
            let id: [u8; 32] = ret[..32].try_into().unwrap();
            let cancel = cancelOrderCall {
                orderId: id.into(),
                marketId: MARKET_ID,
            }
            .abi_encode();
            let t1 = Instant::now();
            run_cancel_order(&cancel, ALICE, &mut ctx).unwrap();
            if timed {
                t_cancel += t1.elapsed();
            }
        }
        let p = report("placeOrder rest (limit GTC, no match)", t_place, iters);
        let c = report("cancelOrder (resting order)", t_cancel, iters);
        report("place+cancel pair avg", t_place + t_cancel, iters * 2);
        println!("PERF note: place {p:.0} ns + cancel {c:.0} ns per round-trip");
    }

    /// (b): taker fully filled by one resting maker. Maker (rest) and taker
    /// (single fill, settles both sides) timed separately; the book is empty
    /// again after every iteration.
    #[test]
    #[ignore = "perf baseline; run with --release --ignored --nocapture"]
    fn perf_match_single_fill() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        fund(&mut ctx, ALICE, BIG);
        fund(&mut ctx, BOB, BIG);

        let maker = place_input(1, PRICE, QTY, 0, 0); // BOB sell rests
        let taker = place_input(0, PRICE, QTY, 0, 0); // ALICE buy fills
        let warmup = 1_000u64;
        let iters = 10_000u64;
        let mut t_maker = Duration::ZERO;
        let mut t_taker = Duration::ZERO;

        for i in 0..(warmup + iters) {
            let timed = i >= warmup;
            let t0 = Instant::now();
            run_place_order(&maker, BOB, &mut ctx).unwrap();
            if timed {
                t_maker += t0.elapsed();
            }
            let t1 = Instant::now();
            run_place_order(&taker, ALICE, &mut ctx).unwrap();
            if timed {
                t_taker += t1.elapsed();
            }
        }
        report("placeOrder maker rest (sell side)", t_maker, iters);
        report("placeOrder taker, 1 fill (direct)", t_taker, iters);
        report("maker+taker pair total", t_maker + t_taker, iters);
    }

    /// (c): one taker sweeping 10 maker price levels. The 10 maker placements
    /// and the sweep are timed separately; per-extra-fill increment is derived
    /// against the 1-fill taker from scenario (b) offline.
    #[test]
    #[ignore = "perf baseline; run with --release --ignored --nocapture"]
    fn perf_taker_sweep_10_levels() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        fund(&mut ctx, ALICE, BIG);
        fund(&mut ctx, BOB, BIG);

        let makers: Vec<Vec<u8>> = (0..10u64)
            .map(|i| place_input(1, PRICE + i * TICK, QTY, 0, 0))
            .collect();
        let taker = place_input(0, PRICE + 9 * TICK, QTY * 10, 0, 0);
        let warmup = 200u64;
        let iters = 2_000u64;
        let mut t_makers = Duration::ZERO;
        let mut t_taker = Duration::ZERO;

        for i in 0..(warmup + iters) {
            let timed = i >= warmup;
            let t0 = Instant::now();
            for m in &makers {
                run_place_order(m, BOB, &mut ctx).unwrap();
            }
            if timed {
                t_makers += t0.elapsed();
            }
            let t1 = Instant::now();
            run_place_order(&taker, ALICE, &mut ctx).unwrap();
            if timed {
                t_taker += t1.elapsed();
            }
        }
        report("maker rest avg (10 distinct levels)", t_makers, iters * 10);
        report("placeOrder taker sweeping 10 levels", t_taker, iters);
        report(
            "full iteration (10 rests + 1 sweep)",
            t_makers + t_taker,
            iters,
        );
    }

    // ── Block-level benches ────────────────────────────────────────────────
    //
    // The perf_* benches above each run ONE op against a fresh/steady-state book, so the off-trie
    // overlay never accumulates and the SAME blob is never re-touched across txns — which makes the
    // serialization redundancy a per-block deser cache (#14) / block-end serialization (#16d) would
    // remove INVISIBLE. These run a whole "block" of txns against ONE ctx (overlay accumulates, hot
    // blobs are re-touched), and instrument the load_blob/store_blob choke (`bench_counter`) to
    // report ser/deser call-counts vs DISTINCT keys — the exact redundancy #14/#16d collapse.
    //
    // Like the other perf_* benches these call the inner `run_*` handlers directly (not
    // `run_perp_dex_call`), so the per-call commitment flush (BLAKE3) is excluded — consistent with
    // the existing baseline; this bench is about ser/deser VOLUME, not the commitment hash.

    fn report_block(
        label: &str,
        elapsed: Duration,
        calls: u64,
        st: &crate::storage::bench_counter::Stats,
    ) {
        // Post-#14 + #16d the typed msgpack helpers no longer hit the byte choke: reads go through
        // the struct overlay (perp_get_struct, no decode) and writes are deferred (perp_store_struct,
        // no per-write encode). So load_blob/store_blob below are the RESIDUAL byte-path traffic
        // (raw-packed level queues + cold reads), and msgpack serialization now happens once per key
        // at block end (block_end_ser). Compare against the pre-#16d committed bench, where
        // store_blob carried ~all writes and per-write serialization.
        println!(
            "PERF BLOCK {label}: {calls} handler calls, {elapsed:?} ({:.2} us/call)",
            elapsed.as_nanos() as f64 / 1000.0 / calls.max(1) as f64
        );
        println!(
            "  load_blob  (residual byte/cold reads): {} calls, {} KiB",
            st.read_calls,
            st.read_bytes / 1024
        );
        println!(
            "  store_blob (residual byte-path writes): {} calls, {} KiB",
            st.write_calls,
            st.write_bytes / 1024
        );
        println!(
            "  block-end serialize (#16d, once per key): {} calls, {} KiB",
            st.block_end_ser_calls,
            st.block_end_ser_bytes / 1024
        );
    }

    /// Resting-only block: a few makers post GTC limit buys across an 8-level band, round after
    /// round, against ONE ctx. No asks ever exist, so every buy rests (no matches). Maximises reuse
    /// of: market config (read every order), best_bid + bid_prices list, the 8 level queues, and
    /// each maker's account/position/nonce. The growing level queues also show the re-serialization
    /// cost (#14/#16d serialize each final queue once instead of once per push).
    #[test]
    #[ignore = "block-level perf; run with --release --ignored --nocapture"]
    fn perf_block_rest_heavy() {
        use crate::storage::bench_counter as bc;
        const N_MAKERS: u64 = 4;
        const N_LEVELS: u64 = 8;
        const ROUNDS: u64 = 500; // N_MAKERS * ROUNDS = 2000 resting placements

        let mut ctx = make_ctx();
        setup(&mut ctx);
        let makers: Vec<Address> = (1..=N_MAKERS).map(user_addr).collect();
        for &m in &makers {
            JournalTr::load_account(ctx.journal_mut(), m).unwrap();
            fund(&mut ctx, m, BIG);
        }

        bc::reset();
        bc::enable();
        let t0 = Instant::now();
        let mut calls = 0u64;
        for r in 0..ROUNDS {
            for (mi, &m) in makers.iter().enumerate() {
                let lvl = (r + mi as u64) % N_LEVELS;
                let price = PRICE - (lvl + 1) * TICK; // strictly below PRICE; no asks => always rests
                bc::next_txn();
                let _ = place(&mut ctx, m, 0, price, QTY, 0, 0); // buy / limit / GTC
                calls += 1;
            }
        }
        // #16d block-end harvest: serialize each struct key ONCE (counted as block_end_ser).
        let _ = ctx.journal_mut().take_perp_delta();
        let elapsed = t0.elapsed();
        bc::disable();
        report_block(
            "rest-heavy (resting limits only)",
            elapsed,
            calls,
            &bc::snapshot(),
        );
    }

    /// Mixed block: resting bids + periodic IOC taker sweeps (cross the top 2 levels) + periodic
    /// cancels of guaranteed-still-open deep bids. Exercises the rest, match, and cancel paths
    /// against ONE ctx so maker accounts/positions, level queues and best_bid are re-touched by all
    /// three paths.
    #[test]
    #[ignore = "block-level perf; run with --release --ignored --nocapture"]
    fn perf_block_mixed() {
        use crate::storage::bench_counter as bc;
        const N_MAKERS: u64 = 4;
        const N_LEVELS: u64 = 8;
        const ROUNDS: u64 = 600;
        const SWEEP_EVERY: u64 = 20;

        let mut ctx = make_ctx();
        setup(&mut ctx);
        let makers: Vec<Address> = (1..=N_MAKERS).map(user_addr).collect();
        for &m in &makers {
            JournalTr::load_account(ctx.journal_mut(), m).unwrap();
            fund(&mut ctx, m, BIG);
        }
        let taker = ALICE;
        fund(&mut ctx, taker, BIG);

        // Deep bids (level >= 2, price <= PRICE-3*TICK) are never crossed by a sweep at PRICE-2*TICK,
        // so they stay open and are safe to cancel deterministically.
        let mut deep: Vec<([u8; 32], Address)> = Vec::new();
        bc::reset();
        bc::enable();
        let t0 = Instant::now();
        let mut calls = 0u64;
        for r in 0..ROUNDS {
            for (mi, &m) in makers.iter().enumerate() {
                let lvl = (r + mi as u64) % N_LEVELS;
                let price = PRICE - (lvl + 1) * TICK;
                bc::next_txn();
                let id = place(&mut ctx, m, 0, price, QTY, 0, 0);
                calls += 1;
                if lvl >= 2 {
                    deep.push((id, m));
                }
            }
            if r % SWEEP_EVERY == SWEEP_EVERY - 1 {
                // IOC sell crossing only the top ~2 bid levels; unfilled remainder expires (no ask rests).
                bc::next_txn();
                let _ = place(&mut ctx, taker, 1, PRICE - 2 * TICK, QTY * 3, 0, 1); // sell / limit / IOC
                calls += 1;
            }
            if deep.len() > 32 {
                let (id, owner) = deep.remove(0);
                let cancel = cancelOrderCall {
                    orderId: id.into(),
                    marketId: MARKET_ID,
                }
                .abi_encode();
                bc::next_txn();
                run_cancel_order(&cancel, owner, &mut ctx).unwrap();
                calls += 1;
            }
        }
        // #16d block-end harvest: serialize each struct key ONCE (counted as block_end_ser).
        let _ = ctx.journal_mut().take_perp_delta();
        let elapsed = t0.elapsed();
        bc::disable();
        report_block(
            "mixed (rests + sweeps + cancels)",
            elapsed,
            calls,
            &bc::snapshot(),
        );
    }

    /// BBO-churn block: frequent place+cancel at exactly the best-bid and best-ask prices, NO matches,
    /// only TWO price levels ever touched (the case the user asked to measure). Runs the IDENTICAL
    /// workload twice against fresh ctxs — once with per-call ser/deser (`force_percall`, pre-#14/#16d)
    /// and once deferred (#14 read cache + #16d block-end write serialization) — and reports the
    /// execution-time SAVING (per-call wall-time − deferred wall-time).
    ///
    /// What deferral collapses here: the per-user account / position / nonce / best-bid+ask blobs are
    /// re-written on every place AND cancel (huge reuse → one ser/deser per key for the whole block);
    /// each order entry is written ~twice (place + cancel) → 2→1. What it does NOT touch: the per-price
    /// FIFO level queues are the raw-byte path (`store_blob`, not msgpack `save_cached`), so they
    /// serialize per-op in BOTH passes and cancel out of the delta = the residual deferral can't remove
    /// for this workload (catalog #21). The commitment is asserted identical across passes = byte-identity.
    #[test]
    #[ignore = "block-level perf; run with --release --ignored --nocapture"]
    fn perf_block_bbo_churn() {
        use crate::storage::bench_counter as bc;
        const ROUNDS: u64 = 2000; // each round = place+cancel @ bid AND place+cancel @ ask (4 calls)

        let bid_px = PRICE - TICK; // best bid
        let ask_px = PRICE + TICK; // best ask (bid_px < ask_px => orders never cross => no matches)

        // One block of identical work; returns (wall-time, counter snapshot, block commitment).
        let run_block = |force_percall: bool| -> (Duration, bc::Stats, U256) {
            let mut ctx = make_ctx();
            // Set the mode BEFORE any write so the whole overlay is one representation (bytes vs
            // struct) — otherwise the first churn read of a setup-written key pays a serialize-on-read.
            bc::set_force_percall(force_percall);
            setup(&mut ctx);
            let m = user_addr(1);
            JournalTr::load_account(ctx.journal_mut(), m).unwrap();
            fund(&mut ctx, m, BIG);
            // Resting bid + ask define a stable BBO that the churn never crosses; they stay open.
            let _rest_bid = place(&mut ctx, m, 0, bid_px, QTY, 0, 0); // buy  GTC @ bid
            let _rest_ask = place(&mut ctx, m, 1, ask_px, QTY, 0, 0); // sell GTC @ ask (no bid >= ask => rests)

            bc::reset();
            bc::enable();
            let t0 = Instant::now();
            for _ in 0..ROUNDS {
                bc::next_txn();
                let b = place(&mut ctx, m, 0, bid_px, QTY, 0, 0); // rests at best bid (no ask <= bid)
                bc::next_txn();
                let cb = cancelOrderCall {
                    orderId: b.into(),
                    marketId: MARKET_ID,
                }
                .abi_encode();
                run_cancel_order(&cb, m, &mut ctx).unwrap();
                bc::next_txn();
                let a = place(&mut ctx, m, 1, ask_px, QTY, 0, 0); // rests at best ask (no bid >= ask)
                bc::next_txn();
                let ca = cancelOrderCall {
                    orderId: a.into(),
                    marketId: MARKET_ID,
                }
                .abi_encode();
                run_cancel_order(&ca, m, &mut ctx).unwrap();
            }
            // Block-end harvest INSIDE the timed region: deferred mode serializes each struct key once
            // here (block_end_ser); per-call mode drains already-serialized bytes (no ser).
            let delta = ctx.journal_mut().take_perp_delta();
            let elapsed = t0.elapsed();
            bc::disable();
            bc::set_force_percall(false);
            let commit = storage::compute_block_commitment(U256::ZERO, &delta);
            (elapsed, bc::snapshot(), commit)
        };

        let calls = ROUNDS * 4;
        let (t_off, s_off, c_off) = run_block(true);
        let (t_on, s_on, c_on) = run_block(false);

        // #16d invariant: deferral must not change the net delta → same on-trie commitment.
        assert_eq!(
            c_off, c_on,
            "force_percall changed the block commitment — deferral must be byte-identical"
        );

        let us = |d: Duration| d.as_nanos() as f64 / 1000.0 / calls as f64;
        println!(
            "PERF BLOCK bbo-churn (place+cancel @ best bid/ask, no match, 2 levels): {calls} calls / {ROUNDS} rounds"
        );
        println!(
            "  PER-CALL  ser/deser (pre-#14/#16d): {t_off:?} ({:.3} us/call)",
            us(t_off)
        );
        println!(
            "    reads {} calls / {} KiB | writes {} calls / {} KiB | block-end ser {} calls",
            s_off.read_calls,
            s_off.read_bytes / 1024,
            s_off.write_calls,
            s_off.write_bytes / 1024,
            s_off.block_end_ser_calls
        );
        println!(
            "  DEFERRED  ser/deser (#14+#16d):     {t_on:?} ({:.3} us/call)",
            us(t_on)
        );
        println!(
            "    residual byte reads {} calls / {} KiB | residual byte writes {} calls / {} KiB | block-end ser {} calls / {} KiB",
            s_on.read_calls, s_on.read_bytes / 1024, s_on.write_calls, s_on.write_bytes / 1024, s_on.block_end_ser_calls, s_on.block_end_ser_bytes / 1024
        );
        let saved = t_off.saturating_sub(t_on);
        let pct = saved.as_nanos() as f64 / (t_off.as_nanos().max(1)) as f64 * 100.0;
        println!("  SAVED by deferring ser+deser to block end: {saved:?} ({pct:.1}% of per-call execution time)");
    }

    /// (e): placeOrderSigned taker, single fill — ed25519 verify path.
    /// Signatures are pre-generated OUTSIDE the timed loop so only the
    /// precompile-side cost (decode, api-key load, recvWindow, verify_strict,
    /// keccak(sig) orderId, duplicate check, matching) is measured. Compare the
    /// taker leg against perf_match_single_fill's direct taker to isolate the
    /// signed-path overhead.
    #[test]
    #[ignore = "perf baseline; run with --release --ignored --nocapture"]
    fn perf_match_single_fill_signed() {
        use ed25519_dalek::{Signer, SigningKey};

        let mut ctx = make_ctx();
        setup(&mut ctx);
        fund(&mut ctx, ALICE, BIG);
        fund(&mut ctx, BOB, BIG);

        let sk = SigningKey::from_bytes(&[7u8; 32]);
        storage::save_api_key(
            &mut ctx,
            ALICE,
            0,
            ApiKey {
                pubkey: sk.verifying_key().to_bytes(),
                expiry: 0,
            },
        )
        .unwrap();

        let block_ts: u64 = 1; // BlockEnv::default() timestamp
        let recv_window: u64 = 60;
        let warmup = 500u64;
        let iters = 5_000u64;

        // Pre-generate one uniquely-signed input per iteration (unique
        // clientOrderId => unique signature => unique orderId).
        let signed_inputs: Vec<Vec<u8>> = (0..(warmup + iters))
            .map(|i| {
                let mut client_id = [0u8; 16];
                client_id[8..].copy_from_slice(&i.to_be_bytes());
                // Canonical 96-byte message, layout from run_place_order_signed.
                let mut msg = [0u8; 96];
                msg[..16].copy_from_slice(b"perpdex_v1_order");
                msg[16..36].copy_from_slice(ALICE.as_slice());
                msg[36..44].copy_from_slice(&MARKET_ID.to_be_bytes());
                msg[44] = 0; // side = Buy
                msg[45..53].copy_from_slice(&PRICE.to_be_bytes());
                msg[53..61].copy_from_slice(&QTY.to_be_bytes());
                msg[61] = 0; // orderType = Limit
                msg[62] = 0; // tif = GTC
                msg[63..79].copy_from_slice(&client_id);
                msg[79..87].copy_from_slice(&block_ts.to_be_bytes());
                msg[87..95].copy_from_slice(&recv_window.to_be_bytes());
                msg[95] = 0; // keyId
                let sig = sk.sign(&msg);
                placeOrderSignedCall {
                    account: ALICE,
                    marketId: MARKET_ID,
                    side: 0,
                    price: PRICE,
                    quantity: QTY,
                    orderType: 0,
                    tif: 0,
                    clientOrderId: FixedBytes(client_id),
                    timestamp: block_ts,
                    recvWindow: recv_window,
                    keyId: 0,
                    signature: sig.to_bytes().to_vec().into(),
                }
                .abi_encode()
            })
            .collect();

        let maker = place_input(1, PRICE, QTY, 0, 0);
        let mut t_taker = Duration::ZERO;

        for (i, signed) in signed_inputs.iter().enumerate() {
            run_place_order(&maker, BOB, &mut ctx).unwrap(); // untimed maker rest
            let timed = (i as u64) >= warmup;
            let t0 = Instant::now();
            run_place_order_signed(signed, &mut ctx).unwrap();
            if timed {
                t_taker += t0.elapsed();
            }
        }
        report("placeOrderSigned taker, 1 fill (ed25519)", t_taker, iters);
    }

    /// (f): updateIndexPrice — oracle hot path. Timestamp advances by the
    /// market's price_update_interval (15s) per call so every call does full
    /// work (stale updates short-circuit). Test market has funding_interval=0,
    /// so premium-index accumulation is skipped in the base variant; the
    /// `_with_funding` variant enables it (production-like).
    #[test]
    #[ignore = "perf baseline; run with --release --ignored --nocapture"]
    fn perf_update_index_price() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        run_update_index_price_loop(&mut ctx, "updateIndexPrice (funding_interval=0)");
    }

    #[test]
    #[ignore = "perf baseline; run with --release --ignored --nocapture"]
    fn perf_update_index_price_with_funding() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        // Same market but with funding enabled (premium-index accumulation
        // every update + funding-rate computation every 3600/15 = 240 updates).
        storage::save_market(
            &mut ctx,
            &Market {
                market_id: MARKET_ID,
                base_decimals: 8,
                price_decimals: 9,
                tick_size: TICK,
                step_size: QTY,
                min_quantity: QTY,
                max_quantity: QTY * 1_000,
                max_price: PRICE * 1_000,
                price_update_interval: 15,
                active: true,
                funding_interval: 3_600,
                interest_rate: 0,
                liquidation_fee_rate_bps: 0,
                price_band_bps: 0,
                mark_price: 0,
                tiers: MarginTiers::default(),
            },
        )
        .unwrap();
        run_update_index_price_loop(
            &mut ctx,
            "updateIndexPrice (funding_interval=3600, premium accumulation)",
        );
    }

    fn run_update_index_price_loop(ctx: &mut TestCtx, label: &str) {
        let warmup = 1_000u64;
        let iters = 20_000u64;
        let mut ts = 15u64;
        let mut t = Duration::ZERO;

        for i in 0..(warmup + iters) {
            let input = updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: PRICE,
                timestamp: ts,
            }
            .abi_encode();
            ts += 15;
            let timed = i >= warmup;
            let t0 = Instant::now();
            run_update_index_price(&input, ADMIN, ctx).unwrap();
            if timed {
                t += t0.elapsed();
            }
        }
        report(label, t, iters);
    }

    /// Micro: raw keccak256 cost on this machine, by input size. Sizes map to
    /// hot-path uses: 32 B ≈ key-derivation input, 71 B = 64+7 scalar-blob fold
    /// preimage, 136 B = exactly one keccak rate block, 219 B = 64+155 Order
    /// fold, 584 B = 64+520 PriceBasisWindow fold (largest hot fold), 4 KiB =
    /// long-string reference for batched-fold designs.
    #[test]
    #[ignore = "perf measurement: cargo test --release perf_ -- --ignored --nocapture"]
    fn perf_keccak256_by_size() {
        use std::hint::black_box;
        for &(size, iters) in &[
            (32usize, 2_000_000u64),
            (71, 2_000_000),
            (136, 2_000_000),
            (219, 1_000_000),
            (584, 1_000_000),
            (4096, 200_000),
        ] {
            let buf = vec![0xA5u8; size];
            for _ in 0..10_000 {
                black_box(primitives::keccak256(black_box(&buf[..])));
            }
            let t0 = Instant::now();
            for _ in 0..iters {
                black_box(primitives::keccak256(black_box(&buf[..])));
            }
            report(&format!("keccak256 over {size} B"), t0.elapsed(), iters);
        }
    }

    /// Micro: BLAKE3 vs keccak256 at the same sizes, to justify #24 (the commitment
    /// hash swap). The commitment now hashes a per-call framed log (C_prev 32B +
    /// 1 ver byte + Σ framed writes), so the relevant sizes are the larger ones.
    #[test]
    #[ignore = "perf measurement: cargo test --release perf_ -- --ignored --nocapture"]
    fn perf_blake3_by_size() {
        use std::hint::black_box;
        for &(size, iters) in &[
            (32usize, 2_000_000u64),
            (71, 2_000_000),
            (136, 2_000_000),
            (219, 1_000_000),
            (584, 1_000_000),
            (4096, 200_000),
        ] {
            let buf = vec![0xA5u8; size];
            for _ in 0..10_000 {
                black_box(blake3::hash(black_box(&buf[..])));
            }
            let t0 = Instant::now();
            for _ in 0..iters {
                black_box(blake3::hash(black_box(&buf[..])));
            }
            report(&format!("blake3 over {size} B"), t0.elapsed(), iters);
        }
    }
}

#[test]
fn fill_settles_funding_for_taker_and_maker() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    // ALICE already holds a long with a stale funding anchor (index 0).
    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -(FILL_VALUE as i64),
            margin: FILL_VALUE as i64,
            leverage: 1,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();

    // Accrue funding (large index so the taker charge is non-trivial).
    let cfi: i128 = 10_000_000_000_000_000;
    storage::save_funding_state(
        &mut ctx,
        MARKET_ID,
        &FundingState {
            last_funding_rate: 100,
            next_funding_ts: 0,
            cumulative_funding_index: cfi,
        },
    )
    .unwrap();

    // BOB rests a sell (fresh maker); ALICE takes it (taker, adds to her long).
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    // Both positions were touched by the fill, so both must have settled funding
    // and re-anchored to the current cumulative index.
    assert_eq!(
        pos(&mut ctx, ALICE).last_funding_index,
        cfi,
        "taker funding settled on its pre-fill position"
    );
    assert_eq!(
        pos(&mut ctx, BOB).last_funding_index,
        cfi,
        "maker funding settled (fresh position anchored to current index)"
    );
}

// ── Golden commitment characterization (P0 guard rail) ─────────────────────
//
// Pins the final value of the on-trie commitment slot (keccak256(b"cmit") under
// 0x1003) over a rich, fully deterministic end-to-end scenario driven through
// `run_perp_dex_call`. See docs/perpdex-commitment优化执行计划.md (P0):
//
//   * SAFE refactors (P1 constant keys + streaming-keccak fold, P2 per-call
//     fold accumulator, P3 asm-keccak) MUST keep `GOLDEN_COMMITMENT`
//     bit-identical — the fold sequence C = keccak256(C ‖ key ‖ blob) over the
//     perp write-stream may not change in content or order.
//   * CHAIN changes (P4 call-level framing / hash swap, #17/#18/#20) change the
//     value by design: re-pin by running `commitment_golden_scenario`, copying
//     the printed `golden commitment =` value, and recording the re-pin in the
//     commit message + the plan doc. Each CHAIN commit re-pins separately.
//
// The business-state snapshot is the second line of defense: it survives a
// hash-function change, so a CHAIN re-pin is only legitimate when the snapshot
// still matches.
//
// Store paths intentionally NOT covered by this scenario (kept out for size /
// determinism; changes touching only these will not move the golden value):
//   * bankrupt liquidation: absorb_from_insurance_fund / InsuranceFundDepleted
//     / bad-debt write-off (solvent path with clearance fee IS covered)
//   * funding charge spilling past the position margin into the insurance fund
//     (the scenario's funding charge is fully covered by ALICE's position margin;
//     the spill path is covered by risk::tests)
//   * margin-shortfall auto-cancel cascade: taker-side
//     cancel_same_side_orders_until_wallet_covers — requires a taker
//     margin-cover cascade that would dominate the scenario
//     (maker-side auto-expire was removed with isolated-margin bad-debt
//     routing, so there is no maker expired-during-level path any more)
//   * FOK / PostOnly success-on-match permutations beyond the covered ones
//     (FOK infeasible revert, PostOnly crossing revert, PostOnly rest)
//   * open_interest (save_open_interest has no production caller today)
//   * multi-market state (single market id 1)
//
// Previously-listed gaps now COVERED by scenario extensions (2026-06-12):
// sell-side cancelOrder of a resting ask; liquidate cancel-all sell-side loop;
// matcher early-exit with surviving queue tail (both book sides); setLeverage
// resting-order margin rebalance (debit + credit) incl. setLeverageSigned;
// transferAdmin; user-facing Market orders (filled + expired remainder);
// liquidation residual settle at mark price; getBookPrices / getBookLevel.
mod golden {
    use super::*;
    use crate::{
        interface::IPerpDex::{
            addMarketCall, addPositionMarginCall, cancelOrderSignedCall, depositCall,
            depositInsuranceFundCall, getAccountCall, getApiKeysCall, getAveragePremiumIndexCall,
            getBookLevelCall, getBookPricesCall, getFundingStateCall, getIndexPriceCall,
            getInsuranceFundCall, getMarkPriceCall, getOpenOrdersCall, getPositionCall,
            initAdminCall, liquidateCall, placeOrderSignedCall, registerApiKeyCall,
            removePositionMarginCall, revokeApiKeyCall, setLeverageCall, setLeverageSignedCall,
            setMarketManagerAddressCall, setOracleAddressCall, setUserFeeRatesCall,
            transferAdminCall, transferFromPerpCall, transferToPerpCall, updateIndexPriceCall,
            updateMarketCall, withdrawCall, withdrawInsuranceFundCall,
        },
        run_perp_dex_call,
        storage::keys as storage_keys,
    };
    use ed25519_dalek::{Signer, SigningKey};
    use primitives::{b256, keccak256, Bytes, B256};

    const ORACLE: Address = address!("4444444444444444444444444444444444444444");
    const MANAGER: Address = address!("5555555555555555555555555555555555555555");
    /// Fresh admin the scenario hands control to at the very end (transferAdmin).
    const NEW_ADMIN: Address = address!("6666666666666666666666666666666666666666");

    /// Pinned final commitment-slot value of `run_golden_scenario`.
    /// Capture/re-pin procedure: run `commitment_golden_scenario` and copy the
    /// `golden commitment =` line it prints (also shown in the assert diff).
    /// Last re-pin 2026-06-14 (P4/#17 no-op skip): PriceBasisWindow re-store is
    /// skipped when the observation changed nothing (same-block timestamp) — CHAIN
    /// change (the skipped write leaves the commitment log), value re-pinned. The
    /// window value is identical whether stored or skipped, so business SNAPSHOT
    /// is unchanged.
    /// (Prior re-pins: P4/#20 bin+enum 0x8a0b7f…; #20-positional 0xcdeac1…; #18
    /// 0x58835a…; #24 0x995638…; 16b 0x69e699…; ext 0x2d5fa5…; P0.)
    /// #16d (per-block commitment, v3): folded ONCE at block end from the net delta
    /// (`take_perp_delta` → `finalize_block_commitment`), replacing the per-call v2 flush.
    /// BusinessSnapshot is UNCHANGED (pure commitment-representation change). Prior v2 value
    /// 0x3c80c530439970bd37d4bef6bcfd22411740241981764031154765a558bf9f47.
    /// Price-band field (2026-07): `Market` gained `price_band_bps` (serialized as "pb"),
    /// so every stored market blob is longer → write-stream commitment shifts. CHAIN change;
    /// BusinessSnapshot UNCHANGED. Prior value
    /// 0x24d9197680681d1b627f13e28ba971a3e1bf2589a979141ae48989efc797e7e6.
    /// Band enforcement (2026-07): this scenario's market band was set to a disabled value
    /// (1_000_000 bps) so the matching/settlement regression is unaffected by the new
    /// placement band; the changed "pb" value shifts the blob → commitment. CHAIN change;
    /// BusinessSnapshot UNCHANGED (band disabled → identical execution). Prior value
    /// 0x05ec6b7a77d6bc57750d92e5624c5dd8262c291fdfa56da0b8fe1cd312bf6aed.
    /// Position registry (2026-07, Phase B): save_position maintains a per-market
    /// open-position registry ("preg" blob) on amount zero-crossings, so opening/closing a
    /// position adds a registry write to the block net delta → commitment shifts. CHAIN
    /// change; BusinessSnapshot UNCHANGED (the registry mirrors open positions and is read
    /// by no view). Prior value
    /// 0x4bad03218af966c0aa1b30a0611eebc746d7b9e07ec33115c2d14ae1fe833af3.
    /// Liquidation fee waiver (2026-07, Phase C fix B): a liquidation close no longer
    /// charges the liquidated user a taker trading fee (they pay only the clearance fee to
    /// the IF), fixing an underwater user with no free wallet being un-liquidatable.
    /// CHANGES BusinessSnapshot: ALICE keeps the taker fee she used to pay on her Phase-9
    /// book close, and the market fee total drops by it. Prior value
    /// 0xfcc04c1f1e41e7e284ace52730d47aaeb47955a3fb1ce27d1d52ea2aa34eab1e.
    /// RE-PIN (catalog #12, direct-packed storage keys + `BLOCK_COMMITMENT_VERSION` 3→4): the
    /// business snapshot is unchanged; only the key bytes folded into the commitment differ, so
    /// this is a legitimate CHAIN re-pin (devnet wipe). Prior value
    /// 0x9b493f51b6dd2011ae1c8b7ebc5a2b2b3b4fc2cc9fd1b46878d5d1a2b85e3435.
    /// RE-PIN (catalog #22, price index Vec<u64>→BTreeSet<u64> + `BLOCK_COMMITMENT_VERSION` 4→5):
    /// business snapshot unchanged; the serialized price-level bytes change (container + order).
    /// Prior value 0x8bcfa8def86af905253c4de33a0ca43634683e94191de931bf9235a38be9597d.
    /// RE-PIN (order-lifecycle redesign, commit-only #23 + `BLOCK_COMMITMENT_VERSION` 5→6):
    /// delete-on-terminal removes filled/cancelled orders from the map (their blobs leave the net
    /// delta, replaced by empty-value deletes), a new per-level live-order count key is folded in,
    /// and the signed-order replay guard moved to the seen-signature namespace. The business
    /// snapshot is UNCHANGED except that terminal orders are no longer queryable (getOrder reverts
    /// → DELETED sentinel) — positions/accounts/fees/mark/funding are identical. Prior value
    /// 0xcfa7fe64e534bd9adf65df5a2e4529b95754e150519352daeed5ce77d01f71c9.
    /// RE-PIN (MarketHot grouping + `BLOCK_COMMITMENT_VERSION` 6→7): the five per-market hot
    /// scalars (mark price, best bid/ask, last traded, open interest) moved from five single-scalar
    /// keys into one grouped `MarketHot` blob, so the block-delta keys + framing regroup. Every
    /// value — and the business snapshot — is IDENTICAL. Prior value
    /// 0xcf5c23c371fd0661b74856c2d08e7df707b3b61831297a870e77276ae531a53f.
    /// RE-PIN (per-user scalar fold + `BLOCK_COMMITMENT_VERSION` 7→8): the fee-rate bps + order
    /// nonce moved from two standalone per-user keys into the account blob (those keys leave the
    /// delta; the account blob grows). Values + business snapshot IDENTICAL. Prior value
    /// 0x2d7a551bd8ef02c3d4933462314b1182be29f40fc284bf1b1c111eb1e423c956.
    /// RE-PIN (mark_price → Market + `BLOCK_COMMITMENT_VERSION` 8→9): mark_price moved out of the
    /// MarketHot blob into the Market blob (write-rare + co-read with config). Both blobs' bytes
    /// change (Market gains a field, MarketHot loses one); values + business snapshot IDENTICAL.
    /// Prior value 0x7288ee0d91edb642537fc632a47eb12d27542977b4ffb7bf97d6bf31a874336c.
    /// RE-PIN (level count merge + `BLOCK_COMMITMENT_VERSION` 9→10): each level's live-order count
    /// folded INTO its FIFO blob (`LevelBlob`, count(8 BE) prefix); the per-level count keys
    /// disappear + the level blob framing changes. Values + business snapshot IDENTICAL. Prior
    /// value 0x47f8225b07e2e1cc951203cc35b2aab40cc7801f43880c8ec3850344de3c78dc.
    /// RE-PIN (event-authored account balances + `BLOCK_COMMITMENT_VERSION` 10→11): total perp
    /// collateral was appended to the account blob and is now pinned alongside the exact available
    /// balance. Prior value 0x25389cb16beb55f0800734570a0ac4d48f9f6309452932b7204fa682dd397d45.
    /// RE-PIN (#A incremental-reservation aggregates + `BLOCK_COMMITMENT_VERSION` 11→12): the
    /// `tbq/tbn/tsq/tsn` maintained totals were appended to the position blob (behavior-identical —
    /// the business snapshot below is unchanged; only the persisted layout + commitment differ).
    /// Prior value 0x9989c4d3675808defbb3baba3f828cb43151b6ffb81f0a902a2ef68286717648.
    /// RE-PIN (drop the `total_perp_collateral` aggregate + `BLOCK_COMMITMENT_VERSION` 12→13): the
    /// derivable "TC" field was REMOVED from the account blob (it was maintained on every hot write
    /// but read by no protocol rule). Business snapshot below keeps every surviving value; only the
    /// persisted layout, the getAccount/AccountBalanceChanged arity and the commitment differ.
    /// Prior value 0x06ee401de8dd26982c5820f9263f67c349cb139ac9fc4d6b4fbfbb73a0e57a0e.
    /// RE-PIN (margin tiers Phase 1 + `BLOCK_COMMITMENT_VERSION` 13→14): `Market` gained the
    /// `tiers` table (appended last, serialised as "mt"), so every stored market blob grows by
    /// its default single-tier `[{0, 3}]` row. Arithmetically BEHAVIOUR-PRESERVING — the tier
    /// maintenance rate `1/(2*3)` is the deleted `MAINTENANCE_MARGIN_DENOMINATOR = 6` — and the
    /// only rule change (leverage cap 6 → 3) was pre-absorbed by the Phase-0.5 retune of this
    /// scenario, so the business snapshot below is UNCHANGED. Prior value
    /// 0xc2c839a6a4dc5fa20b64faa286e30e6b90e7e7905ebe070c5c91bfd0bb2314f1.
    /// RE-PIN (remove the `fee_reserved` escrow + `BLOCK_COMMITMENT_VERSION` 14→15): `PerpPosition`
    /// LOST its "fr" field (every position blob shortens + every later field shifts), placement no
    /// longer debits the order's prospective fee, and each fill charges the fee out of the margin
    /// it funds (`fee_from_margin = min(fee, opening_margin)`). This CHANGES the business snapshot
    /// — deliberately, it is the point of the change — in exactly three places, all conserving:
    ///   * `bob_account` 500_428_794 → 500_428_954 (+160): the 160 fee escrowed against BOB's still
    ///     resting tail bid is no longer withheld, so it sits in his AVAILABLE balance instead of
    ///     `pos.fee_reserved`. Pure bucket move — his total collateral is unchanged.
    ///   * `insurance_fund` 49_007_966 → 49_007_956 (−10) and `alice_account` 999_059_619 →
    ///     999_059_629 (+10): ALICE's opening taker fees now come out of her position margin, so
    ///     her pre-liquidation margin is 2_000 lower (1_593_200 → 1_591_200) and the 50 bps
    ///     clearance fee on it drops 7_966 → 7_956. She keeps that 10; the IF receives it no
    ///     longer. Σ(ALICE, IF) is unchanged.
    ///
    /// `admin_perp_wallet` (51_003_461) and `market_fee_total` (3_461) are UNCHANGED: every trading
    /// fee is still collected in full, only its funding source moved. Positions, CAROL, and every
    /// order status are identical. Prior value
    /// 0x677500b3559bb22e070c48c9134c3d33c37086b2249f3fb1e88e516a04a8fa04.
    /// RE-PIN (A1 isolated funding + `BLOCK_COMMITMENT_VERSION` 15→16): funding settles against
    /// `pos.margin` instead of the account-global perp wallet (credit AND charge; the wallet leg is
    /// gone, the charge falls straight through to the insurance fund once margin is exhausted).
    /// Restores isolated-margin containment and makes funding move the liquidation price, matching
    /// Binance's measured behaviour. `liquidate_position` also stops loading/writing the account —
    /// funding was its only reason to, so the write was a byte-identical re-store (and a spurious
    /// `AccountBalanceChanged`). This is an EXECUTION-RULE change, not a layout change, so the
    /// values folded into the delta differ. It CHANGES the business snapshot in exactly two places,
    /// which are the SAME two units of value:
    ///   * `alice_account` 999_059_629 → 999_059_631 (+2) and `insurance_fund` 49_007_956 →
    ///     49_007_954 (−2). ALICE's Phase-8 funding charge of 400 no longer leaves her wallet; it
    ///     comes out of her position margin instead, so her pre-liquidation margin is 400 thinner
    ///     (1_591_200 → 1_590_800) and the 50 bps clearance fee levied on it drops 7_956 → 7_954.
    ///     Her wallet keeps the 400 it was not charged and gets back 400 less margin at
    ///     liquidation (net 0), and keeps the 2 the IF no longer collects. Σ(ALICE, IF) is
    ///     unchanged — a pure 2-unit transfer, no phantom value.
    ///
    /// `bob_account` is UNCHANGED at 500_428_954 even though his +400 funding credit now lands in
    /// `pos.margin` rather than the wallet: he ends FLAT (`bob_position == (0, 0, 0)`), and closing
    /// a position to zero releases its entire remaining margin to the wallet, so the 400 arrives by
    /// a different route and no proportional-release floor strands any of it. CAROL, both other
    /// positions, `admin_perp_wallet`, `market_fee_total`, `mark_price`, `funding` and every order
    /// status are identical. Prior value
    /// 0xa3fcb1f0cccd86eacd6ec33cfae4604a98577cd669c1b2d7ff3997aa0bf6b3ad.
    /// RE-PIN (per-user market index, derived-ooIM Phase 0 + `BLOCK_COMMITMENT_VERSION` 16→17): a
    /// NEW off-trie namespace ("umkt") records, per user, the set of markets they are active in
    /// (non-zero position OR at least one resting order) — the inverse of the per-market position
    /// registry, which points the wrong way for the derived-ooIM sum and is blind to markets where
    /// a user holds only resting orders. Entering/leaving a market now adds a `umkt` write to the
    /// block net delta, so the commitment shifts. Purely ADDITIVE: nothing reads the index yet, no
    /// existing blob's layout or value changes, and no execution rule moves — the BusinessSnapshot
    /// below is UNCHANGED. (This scenario is single-market, so it exercises enter-on-first-order
    /// and leave-on-fully-flat; multi-market ordering and the cap are covered by the dedicated
    /// `user_market_index` tests.) Prior value
    /// 0xfd17be42969baf09c8e87f990e084ff83180f179c3118e13ed4ccf81e5d92828.
    /// RE-PIN (derived open-order margin, Phase 2 + `BLOCK_COMMITMENT_VERSION` 17→18): the
    /// open-order margin ESCROW is DELETED. `PerpPosition` loses its six reservation fields
    /// ("mr", "mrn", "br", "brn", "sr", "srn"), so every position blob shortens and its later
    /// fields shift; and placement, cancel, `setLeverage` and the fill paths no longer move the
    /// wallet for a reservation, so the account values folded into the delta differ as well. The
    /// requirement is now DERIVED on read (`ooIM = ROUND_UP(max(|N + Bid|, |N − Ask|) / L) −
    /// ROUND_UP(|N| / L)`) and subtracted at the admission gate rather than debited.
    ///
    /// The BusinessSnapshot below is **UNCHANGED, field for field**, which is worth stating
    /// because it is not the general case — the derived and escrow bases disagree on plenty of
    /// book shapes. This scenario avoids all of them: it is single-market, every party ends flat
    /// or with a one-sided book at leverage 1, and the ONE order still resting at the end (BOB's
    /// tail bid) sits against a FLAT position, where `ooIM = ROUND_UP(Bid / 1)` is exactly the
    /// 400_000 the escrow held. So BOB's `availablePerpBalance` arrives at the same 500_428_954
    /// by a different route: his wallet is 400_000 higher (never debited) and the derived
    /// requirement subtracts exactly that. Likewise every maker fill's opening margin, formerly
    /// converted 1:1 out of the placement escrow, is now drawn from the wallet at fill time — the
    /// same net movement in two steps instead of two. Prior value
    /// 0xb6b78e299b6b96f6dcc0c667c76cc4895ae4cd1932b305fdcda9c0907e998ee0.
    /// RE-PIN (Assuming-Price sell side + M1′ maker fills + `BLOCK_COMMITMENT_VERSION` 18→19). No
    /// layout change; two execution-rule changes plus a deliberate scenario EXTENSION.
    ///
    /// 1. **`Ask` is priced at each sell's ASSUMING PRICE** `max(ROUND_UP(lastTraded × 1.0015),
    ///    mark, limit)`, not at its limit price — the vendor Cost formula, measured twice on mainnet
    ///    (run9's eight admission probes; R10's reported `askNotional / q == limit × 1.0015` for a
    ///    sell resting below the floor). The buy side is untouched: a LONG order's Assuming Price IS
    ///    its limit price.
    /// 2. **A maker fill the wallet cannot fund now FILLS**, driving `perp_wallet_balance` negative,
    ///    instead of being cancelled through the K9 channel. Binance leaves an under-covered lien
    ///    alone and only kills orders at liquidation (measured); "don't fill" was model M2, and both
    ///    surviving candidates M1/M1′ fill.
    /// 3. **NEW Phase 9c**, because the previous snapshot was structurally BLIND to (1): its only
    ///    surviving order was a BUY against a FLAT position, so no `Ask` term reached it. An oracle
    ///    bump lifts the mark above the book and CAROL — short QTY — rests a sell inside it.
    ///
    /// BusinessSnapshot, field by field:
    ///   * `mark_price` 70_000_000_000 → **90_000_000_000**. Phase 9c's bump: `indexPrice = 90·TICK`
    ///     against a `lastTraded` of `80·TICK`, so `median(price1, price2, contract) = 90·TICK`.
    ///     Mechanism: the added `updateIndexPrice` call, nothing else.
    ///   * `carol_account.1` (availablePerpBalance) 4_200_000 → **3_300_000**, i.e. −900_000 = the
    ///     ooIM of the sell Phase 9c rests. `N = −900_000` at the new mark, `PIM = 900_000`,
    ///     `Ask = 1 QTY × max(80_120_000_000, 90_000_000_000) = 900_000` ⇒
    ///     `IM = |−900_000 − 900_000| = 1_800_000` ⇒ `ooIM = 900_000`. **Priced at the order's own
    ///     $81 limit `Ask` would be 810_000 and `ooIM` 810_000, leaving 3_390_000** — so the 90_000
    ///     difference between 3_300_000 and 3_390_000 IS the Assuming-Price markup, now pinned in
    ///     the snapshot rather than only in the dedicated tests.
    ///   * `carol_ask_status` is NEW: `Open`. The sell rests (it is above the surviving 80-tick bid).
    ///
    /// Everything else is IDENTICAL: all three positions, `alice_account`, `bob_account`,
    /// `bob_erc20`, `admin_perp_wallet`, `insurance_fund`, `market_fee_total`, `funding` and every
    /// other order status. Phase 9c neither trades nor liquidates (ALICE and BOB are flat; CAROL's
    /// equity at the new mark is 700_000 against a 150_000 maintenance requirement), and change (2)
    /// is not reachable in this scenario at all — no maker here is ever short of its opening margin,
    /// which is why it carries no snapshot movement and is covered by
    /// `an_unfundable_maker_fill_still_fills_and_the_silo_is_short_not_the_wallet` plus the
    /// conservation leg `an_underfunded_maker_fill_conserves_total_system_value` instead. Prior value
    /// 0x2c2ab72998e3f53edf5a6bcb3c7ad552f0543babf6830b8fc87ce707b213e8ac.
    /// RE-PIN (M1 maker fills + `BLOCK_COMMITMENT_VERSION` 19→20): an underfunded maker fill now
    /// funds its opening leg with `min(opening_value / L, cash at hand)` and lets `pos.margin` be
    /// SHORT by the remainder, instead of funding the silo in full and driving
    /// `perp_wallet_balance` negative (M1′ → M1; R11 measured M1, `derived-ooim-plan.md` §3a).
    ///
    /// **This scenario's write set is BYTE-IDENTICAL across that change, and the BusinessSnapshot is
    /// UNCHANGED, field for field** — for the same reason the previous re-pin gave for its own
    /// change (2): no maker here is ever short of its opening margin, so the capped and uncapped
    /// branches compute the same `opening_margin` everywhere in it. VERIFIED, not assumed: the whole
    /// suite including this pin passed at the OLD value with the new execution rule in place and the
    /// version byte still 19. The value below therefore moves for exactly ONE reason — the version
    /// byte is hashed into the commitment (`compute_block_commitment`) — and the underlying delta is
    /// unmoved. The rule change itself stays covered by
    /// `an_unfundable_maker_fill_still_fills_and_the_silo_is_short_not_the_wallet`,
    /// `an_underfunded_maker_fill_conserves_total_system_value` and
    /// `risk::tests::usdc_custody`. Prior value
    /// 0x4a3ed7121a77b1e482a3db667cbe90b0331aa3acd5a350c9508f53e11d33c061.
    /// RE-PIN (frozen per-order Assuming Price, R12 + `BLOCK_COMMITMENT_VERSION` 20→21). BOTH a
    /// layout and an execution-rule change:
    ///
    /// 1. **`OrderEntry` gains a trailing `assuming_price` ("ap") field**, so every per-user
    ///    order-list blob grows by one integer. This scenario writes those lists on every
    ///    place/cancel/fill, so the delta's bytes move on this ground alone.
    /// 2. **A resting order's contribution to `Bid`/`Ask` is FROZEN at placement** instead of being
    ///    re-derived at each read from the CURRENT `T = max(ROUND_UP(lastTraded × 1.0015), mark)`.
    ///    `total_sell_notional` consequently CARRIES the markup (it was the limit-price baseline
    ///    before), so the position blob's value changes for any user holding a marked-up resting
    ///    sell — CAROL's Phase-9c ask is exactly that, at 900_000 rather than 810_000.
    ///    MEASURED: R12, `misc/binance-flip-and-admission.md` §3.13 — 90 frames, the reported
    ///    `askNotional` never moved; `H_live` refused by 1939 quanta.
    ///
    /// **The BusinessSnapshot below is UNCHANGED, field for field.** That is not a general property
    /// of this change — the frozen and live bases disagree on any book read after a market move —
    /// and it holds here for a specific, checkable reason: the scenario has exactly ONE marked-up
    /// resting sell (CAROL's Phase-9c ask), it is placed AFTER the last mark/oracle move, and it is
    /// read back at that same mark. So the frozen value and the re-derived value coincide: `T` at
    /// placement was `max(⌈80·TICK × 1.0015⌉, 90·TICK) = 90·TICK`, dominating its own $81 limit, and
    /// `carol_account.1` stays 3_300_000 = 5_000_000 − 800_000 − 900_000 by both routes. Nothing
    /// else in the scenario holds a resting sell at all, so `Ask` is 0 for ALICE and BOB either way,
    /// and the buy side is definitionally unaffected (a long order's Assuming Price IS its limit
    /// price, so `assuming_price == price` on every buy entry ever written). VERIFIED, not assumed:
    /// the snapshot assertion passes unchanged, and the value below was captured with the version
    /// byte already at 21. Prior value
    /// 0x88dc1d5927c36e45585ce99ed8453bf7665a57c89ceb29ee10165fa4ccf41319 (and 0x68a881f4… is the
    /// same write set at version byte 20, recorded to separate the two contributions to the move).
    /// RE-PIN (out-of-band resting-order expiry + `BLOCK_COMMITMENT_VERSION` 21→22): `updateIndexPrice`
    /// now expires the contiguous NEAR-SIDE prefix of levels the moved price band has stranded (asks
    /// below the lower edge, bids above the upper edge), capped at `MAX_BAND_EXPIRIES_PER_UPDATE`
    /// orders, before the liquidation sweep. `OrderCancelled` also gains a `uint8 reason` so a
    /// protocol kill is distinguishable from the owner's own cancel.
    ///
    /// **This scenario's write set is BYTE-IDENTICAL across that change, and the BusinessSnapshot is
    /// UNCHANGED, field for field**, for a mechanical reason: the scenario deliberately runs with the
    /// band DISABLED (`priceBandBps: 1_000_000`, set when the fill-time band landed — see the 2026-07
    /// entry above), and `mark_band_bounds` collapses a disabled band to `lower = 0` / an enormous
    /// `upper`, so no level on either side is ever out of band and the new sweep expires nothing here.
    /// The two oracle updates it performs (Phase 8's funding epoch and Phase 9c's bump) therefore write
    /// exactly what they wrote at 21. The event change cannot reach the snapshot either — every field
    /// below is read back through a VIEW call, not from logs.
    ///
    /// VERIFIED, not assumed: the whole suite including this pin passed at the OLD value with the new
    /// execution rule in place and the version byte still 21. The value below therefore moves for
    /// exactly ONE reason — the version byte is hashed into the commitment — and the underlying delta
    /// is unmoved. The rule itself is covered by `risk::tests::band_expiry` (near-side prefix expired,
    /// far-side deep orders spared, cap respected + second update finishes, reason codes, frozen
    /// `assuming_price` basis, disabled band inert, fill-time band still the backstop). Prior value
    /// 0xc84dfc3ed57226c907fbac53e64dfc011e64be2254b092edffb4758a8eef6ba5.
    /// RE-PIN (stored `Σ pos.margin` + `BLOCK_COMMITMENT_VERSION` 22→23): `UserAccount` gains a
    /// trailing "PM" field (`total_position_margin`), so every stored account blob grows by one
    /// integer; and `storage::save_position` now maintains it from the delta of the position write,
    /// which adds the ACCOUNT key to the delta of any write that MOVES `pos.margin`. So the commitment
    /// moves for THREE mechanical reasons — longer account blobs, extra account keys, and the version
    /// byte — none of them an execution rule.
    ///
    /// **BusinessSnapshot UNCHANGED, field for field**, and necessarily so: no field of it reads the
    /// new aggregate (`alice/bob/carol_account` decode `getAccount`'s `availableBalance`, which is
    /// `totalCrossWalletBalance − totalOpenOrderInitialMargin` and contains no `Σ pos.margin` term;
    /// the three `*_position` triples read `getPosition`, which this change does not touch), no
    /// existing field of any blob changes value, and nothing conditions on the new field.
    /// `totalWalletBalance`, the field the aggregate actually serves, is not pinned by the snapshot —
    /// it is pinned against the wide fold by
    /// `margin_view::tests::the_event_and_get_account_agree_field_for_field_on_the_same_state` and by
    /// the `debug_assertions` cross-check inside `margin_view::index_account_scalars`, which compares
    /// the stored aggregate against a fresh walk on every `getAccount` and every published snapshot.
    /// Prior value 0xea28799e35db06c71ad95e1517df8e289028b79721bcdb27c183f7d594d72f0e.
    const GOLDEN_COMMITMENT: B256 =
        b256!("0x18acf659ad6effb859689eb2d43465c0bb8386496ce13369f3837d6455848101");

    /// Business end-state read back through view calls after the scenario.
    /// Pins semantics independently of the commitment hash construction.
    #[derive(Debug, PartialEq, Eq)]
    struct BusinessSnapshot {
        /// (amount, vQuoteBalance, margin)
        alice_position: (i64, i64, i64),
        bob_position: (i64, i64, i64),
        carol_position: (i64, i64, i64),
        /// (spot USDC balance, `getAccount().availableBalance`)
        ///
        /// The second element widened `u64` → `i64` when `getAccount`'s clamped
        /// `availablePerpBalance` was replaced by the signed `availableBalance`. Every VALUE this
        /// scenario pins is unchanged — the quantity is the same `perp_wallet_balance - SUM ooIM`,
        /// and none of these accounts is under water at the end of the scenario, so nothing was
        /// being floored away for the clamp to hide.
        alice_account: (U256, i64),
        bob_account: (U256, i64),
        carol_account: (U256, i64),
        bob_erc20: U256,
        /// Trading-fee sink (taker+maker fees credit the admin's perp wallet).
        admin_perp_wallet: i64,
        insurance_fund: u64,
        market_fee_total: u64,
        mark_price: u64,
        /// (lastFundingRate, nextFundingTs)
        funding: (i64, u64),
        signed_buy_status: u8,
        gtc_cancelled_status: u8,
        ioc_status: u8,
        /// Market-order remainder against an empty book (market path → Expired).
        mkt_expired_status: u8,
        /// PostOnly ask that rested, then was cancelled (sell-side cancel paths).
        po_ask_cancelled_status: u8,
        /// ALICE's resting bid cancelled by liquidate()'s cancel-all (buy side).
        liq_cancelled_bid_status: u8,
        /// ALICE's resting ask cancelled by liquidate()'s cancel-all (sell side).
        liq_cancelled_ask_status: u8,
        bob_bid_status: u8,
        /// BOB's same-price tail bid surviving the sell-side matcher early-exit.
        bob_tail_bid_status: u8,
        carol_close_status: u8,
        /// CAROL's resting ask against her SHORT — the scenario's only sell-side open-order
        /// requirement, and the only place the Assuming-Price markup reaches the snapshot.
        carol_ask_status: u8,
    }

    fn expected_snapshot() -> BusinessSnapshot {
        BusinessSnapshot {
            // ALICE fully closed by the liquidation round-trip; BOB flat after
            // CAROL's taker sell closes his last short QTY.
            alice_position: (0, 0, 0),
            bob_position: (0, 0, 0),
            // CAROL short QTY @ $80 at default leverage 1 (full-notional margin).
            // Her position is untouched by the leverage retune, but the deeper crash
            // leaves her holding 100_000 of UNREALISED gain (1e6 base × $10 of extra
            // downside). That is why the account-side total below is exactly 100_000
            // lower than the pre-retune snapshot — the value moved into her uPnL, which
            // these account fields do not carry. Conservation holds.
            carol_position: (-1_000_000, 800_000, 800_000),
            // ALICE. Two independent drivers changed this vs the pre-cap snapshot
            // (999_162_305), and nothing else did:
            //   1. leverage 5 → 3 (max-leverage cap): every fill posts thicker margin,
            //      so her pre-liquidation margin — and thus the 50 bps clearance fee —
            //      grew (1_056_000 → 1_593_200, fee 5_280 → 7_966).
            //   2. crash $80 → $70: the residual QTY settles at a worse mark, so the
            //      liquidation returns less.
            // The liquidation close taker fee is still WAIVED (fix B) — no −1_200 here.
            // The old term-by-term breakdown is superseded: it was pinned to the 5x
            // margins and the $80 settle, and every term moved.
            //   3. fee_reserved removal: her opening taker fees are charged to the
            //      position margin instead of the wallet, so the margin the clearance
            //      fee is levied on is 2_000 thinner and that fee is 10 smaller — the
            //      10 stays with her (and the IF below is 10 lower). Nothing else on
            //      her side moves: the 2_000 she does not pay from the wallet is
            //      exactly the 2_000 less that her margin returns at liquidation.
            //   4. A1 isolated funding: same shape, one step smaller. Her 400 funding
            //      charge is taken from the position margin instead of the wallet, so
            //      the pre-liquidation margin is a further 400 thinner (1_591_200 →
            //      1_590_800) and the clearance fee drops 7_956 → 7_954. The 400 she
            //      does not pay from the wallet is exactly the 400 less her margin
            //      returns, so the only net movement is the +2 she keeps (= the 2 the
            //      IF below no longer collects).
            alice_account: (U256::from(500_000_000u64), 999_059_631),
            // BOB perp = 1e9 + 830_000 short PnL (622_500 on the 3-QTY
            //   liquidation leg + 207_500 on the QTY closed via CAROL) + 400
            //   funding credit − 1_446 maker fees − 400_000 still reserved for
            //   the resting tail bid (margin only: the 160 fee that used to be
            //   escrowed alongside it is no longer withheld at placement)
            //   − 500_000_000 transferFromPerp.
            // A1 note: the 400 funding credit is now paid into his POSITION margin, not
            //   straight into the wallet. He ends flat, and closing to zero releases the
            //   whole remaining margin, so the 400 still lands here — unchanged total,
            //   different route.
            bob_account: (U256::from(500_000_000u64), 500_428_954),
            // CAROL perp = 5_000_000 funded − 800_000 short opening margin − 900_000 of DERIVED
            // ooIM for the Phase-9c resting sell. That 900_000 is the sell priced at the ASSUMING
            // PRICE (the $90 mark, which dominates both its own $81 limit and the
            // ROUND_UP(80·TICK × 1.0015) = $80.12 last-price term); at the limit price it would be
            // 810_000 and this field would read 3_390_000. Nothing is debited — ooIM is subtracted
            // on read, so her wallet is still 4_200_000.
            carol_account: (U256::from(5_000_000u64), 3_300_000),
            // 2e9 seed − 1.5e9 deposit + 0.5e9 withdraw.
            bob_erc20: U256::from(1_000_000_000u64),
            // 100M funding − 50M IF deposit + 1M IF withdraw + 3_461 fees
            //   (liquidation close taker fee waived — fix B).
            admin_perp_wallet: 51_003_461,
            // 50M deposit − 1M withdraw + 7_954 clearance fee
            //   (50 bps of ALICE's 1_590_800 pre-liquidation margin — thicker than the
            //   pre-cap 1_056_000 because she now runs at 3x instead of 5x, and 2_400
            //   thinner than before the fee_reserved removal + A1 because her opening
            //   taker fees (2_000) and her funding charge (400) are now both charged to
            //   that margin; the 12 the IF no longer collects is the 12 ALICE keeps
            //   above).
            insurance_fund: 49_007_954,
            // ALICE takers 2_015 + BOB maker 806 + 480 + 160 (CAROL's taker fee
            //   is 0 bps; the liquidation close taker fee is waived — fix B).
            market_fee_total: 3_461,
            // $90: Phase 9c's oracle bump lifts the mark ABOVE the book so the `Mark` branch of the
            // Assuming Price is reachable (a $1 tick cannot express the 15 bps `Last × 1.0015` gap).
            mark_price: 90_000_000_000,
            funding: (100, 7_215), // rate = interest-rate clamp; next epoch ts
            // delete-on-terminal (commit-only #23): every terminal order (Filled/Cancelled/Expired)
            // is removed from the map → getOrder reverts → DELETED sentinel. Only the still-resting
            // tail bid remains queryable (Open). Business outcomes are pinned by the position /
            // account / fee fields above, not these status bytes.
            signed_buy_status: DELETED,
            gtc_cancelled_status: DELETED,
            ioc_status: DELETED,
            mkt_expired_status: DELETED,
            po_ask_cancelled_status: DELETED,
            liq_cancelled_bid_status: DELETED,
            liq_cancelled_ask_status: DELETED,
            bob_bid_status: DELETED,
            bob_tail_bid_status: OrderStatus::Open as u8,
            carol_close_status: DELETED,
            carol_ask_status: OrderStatus::Open as u8,
        }
    }

    // ── Context & call plumbing ────────────────────────────────────────────

    /// Consensus-visible commitment anchor slot under 0x1003, derived locally
    /// (`keccak256(b"cmit")`) and NOT via `storage::keys::commitment_slot()`,
    /// so a refactor that consistently changes the production slot derivation
    /// (e.g. a typo'd precomputed constant) cannot pass self-consistently:
    /// the explicit slot-pin assert at the top of `run_golden_scenario` fails
    /// with a clear message instead.
    fn golden_commitment_slot() -> B256 {
        // Independent re-derivation of the packed anchor slot (#12): "cmit" ++ 28 zero.
        let mut b = [0u8; 32];
        b[..4].copy_from_slice(b"cmit");
        B256::new(b)
    }

    /// Standard OZ ERC-20 `_balances[user]` slot (mapping at slot 0), derived
    /// locally for the same anti-tautology reason as `golden_commitment_slot`.
    fn golden_erc20_balance_slot(user: Address) -> B256 {
        let mut buf = [0u8; 64];
        buf[12..32].copy_from_slice(user.as_slice());
        keccak256(buf)
    }

    /// Fresh context with ERC-20 USDC seeded for ALICE / BOB / CAROL / ADMIN so
    /// the full deposit → transferToPerp pipeline can run through the entry point.
    fn golden_ctx() -> TestCtx {
        let mut db = InMemoryDB::default();
        for (user, amount) in [
            (ALICE, 2_000_000_000u64), // $2,000
            (BOB, 2_000_000_000),
            (CAROL, 10_000_000),  // $10 — only used for the post-liquidation step
            (ADMIN, 500_000_000), // $500
        ] {
            db.insert_account_storage(
                USDC_ADDRESS,
                golden_erc20_balance_slot(user).into(),
                U256::from(amount),
            )
            .unwrap();
        }
        let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
        for addr in [
            USDC_ADDRESS,
            PERP_DEX_ADDRESS,
            ALICE,
            BOB,
            CAROL,
            ADMIN,
            ORACLE,
            MANAGER,
            NEW_ADMIN,
        ] {
            JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
        }
        // Fixed block timestamp for full determinism (signed-order recvWindow
        // checks and mid-price samples both read it).
        ctx.block.timestamp = U256::from(1);
        ctx
    }

    fn read_commitment(ctx: &mut TestCtx) -> U256 {
        ctx.journal_mut()
            .sload(PERP_DEX_ADDRESS, golden_commitment_slot().into())
            .unwrap()
            .data
    }

    fn decode_revert(bytes: &[u8]) -> String {
        if bytes.len() >= 68 && bytes[..4] == [0x08, 0xc3, 0x79, 0xa0] {
            let len = U256::from_be_slice(&bytes[36..68]).to::<usize>();
            String::from_utf8_lossy(&bytes[68..68 + len]).into_owned()
        } else {
            format!("0x{}", primitives::hex::encode(bytes))
        }
    }

    /// Mutating call through the full entry point; panics with the decoded
    /// revert reason on failure.
    fn dex_call(ctx: &mut TestCtx, caller: Address, input: &[u8]) -> Bytes {
        let out = run_perp_dex_call(input, 10_000_000, caller, U256::ZERO, false, ctx)
            .expect("perp dex call must not hard-fail");
        assert!(
            !out.reverted,
            "dex call reverted: {}",
            decode_revert(&out.bytes)
        );
        out.bytes
    }

    /// A call whose revert is the point. Mirrors the EVM frame: the call runs
    /// under a checkpoint that is rolled back on revert, and the commitment
    /// must come out unchanged.
    fn dex_call_expect_revert(ctx: &mut TestCtx, caller: Address, input: &[u8], expect: &str) {
        let c_before = read_commitment(ctx);
        let cp = ctx.journal_mut().checkpoint();
        let out = run_perp_dex_call(input, 10_000_000, caller, U256::ZERO, false, ctx)
            .expect("perp dex call must not hard-fail");
        assert!(
            out.reverted,
            "expected revert containing {expect:?}, call succeeded"
        );
        let reason = decode_revert(&out.bytes);
        assert!(
            reason.contains(expect),
            "revert reason {reason:?} does not contain {expect:?}"
        );
        ctx.journal_mut().checkpoint_revert(cp);
        assert_eq!(
            read_commitment(ctx),
            c_before,
            "reverted call must leave the commitment unchanged"
        );
    }

    /// Static (view) call through the entry point.
    fn dex_view(ctx: &mut TestCtx, input: &[u8]) -> Bytes {
        let out = run_perp_dex_call(input, 10_000_000, CAROL, U256::ZERO, true, ctx)
            .expect("view call must not hard-fail");
        assert!(
            !out.reverted,
            "view call reverted: {}",
            decode_revert(&out.bytes)
        );
        out.bytes
    }

    fn g_place(
        ctx: &mut TestCtx,
        caller: Address,
        side: u8,
        price: u64,
        qty: u64,
        order_type: u8,
        tif: u8,
    ) -> [u8; 32] {
        let input = placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: order_type,
            tif,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode();
        let ret = dex_call(ctx, caller, &input);
        ret[..32].try_into().unwrap()
    }

    /// Sentinel for a terminal order under delete-on-terminal (commit-only #23): a
    /// Filled/Cancelled/Expired order is removed from the map, so `getOrder` reverts "not found".
    /// The scenario asserts this sentinel where it used to assert the terminal status — the actual
    /// business outcome (fill/cancel) is pinned by the position/account/fee/event assertions, not
    /// by a queryable status byte.
    const DELETED: u8 = 0xFF;

    fn order_status(ctx: &mut TestCtx, id: [u8; 32]) -> u8 {
        let out = run_perp_dex_call(
            &getOrderCall {
                orderId: id.into(),
                marketId: MARKET_ID,
            }
            .abi_encode(),
            10_000_000,
            CAROL,
            U256::ZERO,
            true,
            ctx,
        )
        .expect("view call must not hard-fail");
        if out.reverted {
            return DELETED; // delete-on-terminal: terminal order was removed from the map
        }
        getOrderCall::abi_decode_returns(&out.bytes).unwrap().status
    }

    // ── ed25519 signed-call calldata (fixed key seed, fixed timestamps) ────

    const SIGNED_TS: u64 = 1; // == block timestamp
    const SIGNED_RECV: u64 = 60;

    fn signed_place_input(
        sk: &SigningKey,
        side: u8,
        price: u64,
        qty: u64,
        order_type: u8,
        tif: u8,
        client_id: [u8; 16],
    ) -> Vec<u8> {
        // Canonical 96-byte message, layout from run_place_order_signed.
        let mut msg = [0u8; 96];
        msg[..16].copy_from_slice(b"perpdex_v1_order");
        msg[16..36].copy_from_slice(ALICE.as_slice());
        msg[36..44].copy_from_slice(&MARKET_ID.to_be_bytes());
        msg[44] = side;
        msg[45..53].copy_from_slice(&price.to_be_bytes());
        msg[53..61].copy_from_slice(&qty.to_be_bytes());
        msg[61] = order_type;
        msg[62] = tif;
        msg[63..79].copy_from_slice(&client_id);
        msg[79..87].copy_from_slice(&SIGNED_TS.to_be_bytes());
        msg[87..95].copy_from_slice(&SIGNED_RECV.to_be_bytes());
        msg[95] = 0; // keyId
        let sig = sk.sign(&msg);
        placeOrderSignedCall {
            account: ALICE,
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: order_type,
            tif,
            clientOrderId: FixedBytes(client_id),
            timestamp: SIGNED_TS,
            recvWindow: SIGNED_RECV,
            keyId: 0,
            signature: sig.to_bytes().to_vec().into(),
        }
        .abi_encode()
    }

    fn signed_cancel_input(sk: &SigningKey, order_id: [u8; 32]) -> Vec<u8> {
        // Canonical 94-byte message, layout from run_cancel_order_signed.
        let mut msg = [0u8; 94];
        msg[..17].copy_from_slice(b"perpdex_v1_cancel");
        msg[17..37].copy_from_slice(ALICE.as_slice());
        msg[37..69].copy_from_slice(&order_id);
        msg[69..77].copy_from_slice(&MARKET_ID.to_be_bytes());
        msg[77..85].copy_from_slice(&SIGNED_TS.to_be_bytes());
        msg[85..93].copy_from_slice(&SIGNED_RECV.to_be_bytes());
        msg[93] = 0; // keyId
        let sig = sk.sign(&msg);
        cancelOrderSignedCall {
            account: ALICE,
            orderId: order_id.into(),
            marketId: MARKET_ID,
            timestamp: SIGNED_TS,
            recvWindow: SIGNED_RECV,
            keyId: 0,
            signature: sig.to_bytes().to_vec().into(),
        }
        .abi_encode()
    }

    fn signed_leverage_input(sk: &SigningKey, leverage: u64) -> Vec<u8> {
        // Canonical 72-byte message, layout from run_set_leverage_signed.
        let mut msg = [0u8; 72];
        msg[..19].copy_from_slice(b"perpdex_v1_leverage");
        msg[19..39].copy_from_slice(ALICE.as_slice());
        msg[39..47].copy_from_slice(&MARKET_ID.to_be_bytes());
        msg[47..55].copy_from_slice(&leverage.to_be_bytes());
        msg[55..63].copy_from_slice(&SIGNED_TS.to_be_bytes());
        msg[63..71].copy_from_slice(&SIGNED_RECV.to_be_bytes());
        msg[71] = 0; // keyId
        let sig = sk.sign(&msg);
        setLeverageSignedCall {
            account: ALICE,
            marketId: MARKET_ID,
            leverage,
            timestamp: SIGNED_TS,
            recvWindow: SIGNED_RECV,
            keyId: 0,
            signature: sig.to_bytes().to_vec().into(),
        }
        .abi_encode()
    }

    // ── The scenario ───────────────────────────────────────────────────────

    /// Plays the fixed script on a fresh context and returns the final
    /// commitment-slot value plus the business end-state.
    fn run_golden_scenario() -> (B256, BusinessSnapshot) {
        // Pin the consensus-visible slot LOCATIONS against the production
        // derivations: if either derivation ever changes, fail loudly here
        // rather than tautologically reading/writing a silently moved slot.
        assert_eq!(
            golden_commitment_slot(),
            storage_keys::commitment_slot(),
            "commitment anchor slot moved"
        );
        assert_eq!(
            golden_erc20_balance_slot(ALICE),
            storage_keys::erc20_balance_slot(ALICE),
            "erc20 balance slot derivation moved"
        );

        let mut ctx = golden_ctx();
        let sk = SigningKey::from_bytes(&[7u8; 32]);

        // Phase 0 — roles (admin / oracle / market-manager blobs).
        dex_call(
            &mut ctx,
            ADMIN,
            &initAdminCall { admin: ADMIN }.abi_encode(),
        );
        dex_call_expect_revert(
            &mut ctx,
            ALICE,
            &initAdminCall { admin: ALICE }.abi_encode(),
            "already initialised",
        );
        dex_call(
            &mut ctx,
            ADMIN,
            &setOracleAddressCall { oracle: ORACLE }.abi_encode(),
        );
        dex_call(
            &mut ctx,
            ADMIN,
            &setMarketManagerAddressCall { manager: MANAGER }.abi_encode(),
        );

        // Phase 1 — market via the manager role, then an admin updateMarket.
        dex_call(
            &mut ctx,
            MANAGER,
            &addMarketCall {
                marketId: MARKET_ID,
                baseDecimals: 8,
                priceDecimals: 9,
                tickSize: TICK,
                stepSize: QTY,
                minQuantity: QTY,
                maxQuantity: QTY * 1_000,
                maxPrice: PRICE * 10,
                priceUpdateInterval: 15,
                fundingInterval: 3_600,
                interestRate: 100,
                liquidationFeeRateBps: 50,
                initialMarkPrice: PRICE,
                // Disabled band (>> this scenario's max price = 10x mark): the golden
                // scenario is the matching/settlement/commitment regression, not a band
                // test. Band enforcement is exercised by dedicated tests below.
                priceBandBps: 1_000_000,
            }
            .abi_encode(),
        );
        dex_call(
            &mut ctx,
            ADMIN,
            &updateMarketCall {
                marketId: MARKET_ID,
                tickSize: TICK,
                stepSize: QTY,
                minQuantity: QTY,
                maxQuantity: QTY * 2_000,
                maxPrice: PRICE * 10,
                priceUpdateInterval: 15,
                active: true,
                fundingInterval: 3_600,
                interestRate: 100,
                liquidationFeeRateBps: 50,
                priceBandBps: 1_000_000, // keep the golden band disabled (see addMarket above)
            }
            .abi_encode(),
        );

        // Nonzero maker+taker fees for both traders (fee-sink paths need them).
        for user in [ALICE, BOB] {
            dex_call(
                &mut ctx,
                ADMIN,
                &setUserFeeRatesCall {
                    user,
                    makerFeeBps: 2,
                    takerFeeBps: 5,
                }
                .abi_encode(),
            );
        }
        dex_call_expect_revert(
            &mut ctx,
            BOB,
            &setUserFeeRatesCall {
                user: BOB,
                makerFeeBps: 0,
                takerFeeBps: 0,
            }
            .abi_encode(),
            "not admin",
        );

        // Phase 2 — oracle bootstrap: mark price must exist before trading.
        dex_call(
            &mut ctx,
            ORACLE,
            &updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: PRICE,
                timestamp: 15,
            }
            .abi_encode(),
        );

        // Phase 3 — fund accounts: ERC-20 → spot (deposit) → perp wallet.
        for user in [ALICE, BOB] {
            dex_call(
                &mut ctx,
                user,
                &depositCall {
                    amount: U256::from(1_500_000_000u64),
                }
                .abi_encode(),
            );
            dex_call(
                &mut ctx,
                user,
                &transferToPerpCall {
                    amount: 1_000_000_000,
                }
                .abi_encode(),
            );
        }
        dex_call(
            &mut ctx,
            ADMIN,
            &depositCall {
                amount: U256::from(200_000_000u64),
            }
            .abi_encode(),
        );
        dex_call(
            &mut ctx,
            ADMIN,
            &transferToPerpCall {
                amount: 100_000_000,
            }
            .abi_encode(),
        );

        // Phase 4 — leverage.
        dex_call(
            &mut ctx,
            ALICE,
            &setLeverageCall {
                marketId: MARKET_ID,
                leverage: 3,
            }
            .abi_encode(),
        );
        dex_call(
            &mut ctx,
            BOB,
            &setLeverageCall {
                marketId: MARKET_ID,
                leverage: 2,
            }
            .abi_encode(),
        );

        // Phase 5 — signed path: register fixed-seed key, signed resting buy
        // (relayed by CAROL — caller is irrelevant on the signed path).
        dex_call(
            &mut ctx,
            ALICE,
            &registerApiKeyCall {
                keyId: 0,
                pubkey: FixedBytes(sk.verifying_key().to_bytes()),
                expiry: 0,
            }
            .abi_encode(),
        );
        let signed_buy: [u8; 32] = {
            let input = signed_place_input(&sk, 0, PRICE - 5 * TICK, QTY, 0, 0, [0xA1; 16]);
            let ret = dex_call(&mut ctx, CAROL, &input);
            ret[..32].try_into().unwrap()
        };

        // While the signed buy rests and the position is still flat, retune
        // leverage both ways so rebalance_order_margin_for_leverage runs its
        // debit branch (3→2 grows the resting-order reserve; relayed by CAROL
        // through the signed path) and its credit branch (2→3 shrinks it back).
        dex_call(&mut ctx, CAROL, &signed_leverage_input(&sk, 2));
        dex_call(
            &mut ctx,
            ALICE,
            &setLeverageCall {
                marketId: MARKET_ID,
                leverage: 3,
            }
            .abi_encode(),
        );

        // View stretch #1 — pure reads must not fold the commitment.
        let c_views = read_commitment(&mut ctx);
        dex_view(
            &mut ctx,
            &getMarkPriceCall {
                marketId: MARKET_ID,
            }
            .abi_encode(),
        );
        dex_view(&mut ctx, &getAccountCall { user: ALICE }.abi_encode());
        dex_view(
            &mut ctx,
            &getOpenOrdersCall {
                user: ALICE,
                marketId: MARKET_ID,
            }
            .abi_encode(),
        );
        dex_view(&mut ctx, &getApiKeysCall { user: ALICE }.abi_encode());
        dex_view(
            &mut ctx,
            &getIndexPriceCall {
                marketId: MARKET_ID,
            }
            .abi_encode(),
        );
        assert_eq!(
            read_commitment(&mut ctx),
            c_views,
            "view calls must not fold the commitment"
        );

        // Phase 6 — direct trading.
        // BOB makes three ask levels, with TWO same-price orders at L1; ALICE
        // takes L1's head with a user-facing MARKET order (the same-price tail
        // must survive via the buy-side matcher early-exit), then sweeps the
        // rest (level clearing + price-list rewrite + best-ask refresh).
        let bob_l1a = g_place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        let bob_l1b = g_place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        let _bob_l2 = g_place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);
        let _bob_l3 = g_place(&mut ctx, BOB, 1, PRICE + 2 * TICK, QTY, 0, 0);

        // Book views while three ask levels exist — must not fold either.
        let c_book_views = read_commitment(&mut ctx);
        let asks = getBookPricesCall::abi_decode_returns(&dex_view(
            &mut ctx,
            &getBookPricesCall {
                marketId: MARKET_ID,
                side: 1,
            }
            .abi_encode(),
        ))
        .unwrap();
        assert_eq!(asks, vec![PRICE, PRICE + TICK, PRICE + 2 * TICK]);
        let bids = getBookPricesCall::abi_decode_returns(&dex_view(
            &mut ctx,
            &getBookPricesCall {
                marketId: MARKET_ID,
                side: 0,
            }
            .abi_encode(),
        ))
        .unwrap();
        assert_eq!(bids, vec![PRICE - 5 * TICK], "signed buy must be resting");
        let l1_queue = getBookLevelCall::abi_decode_returns(&dex_view(
            &mut ctx,
            &getBookLevelCall {
                marketId: MARKET_ID,
                side: 1,
                price: PRICE,
            }
            .abi_encode(),
        ))
        .unwrap();
        assert_eq!(l1_queue, vec![FixedBytes(bob_l1a), FixedBytes(bob_l1b)]);
        assert_eq!(
            read_commitment(&mut ctx),
            c_book_views,
            "book views must not fold the commitment"
        );

        // FOK that cannot fully fill (book depth is 4×QTY up to $102) and a
        // PostOnly that would cross — both revert under a rolled-back
        // checkpoint and must leave the commitment untouched.
        dex_call_expect_revert(
            &mut ctx,
            ALICE,
            &placeOrderCall {
                marketId: MARKET_ID,
                side: 0,
                price: PRICE + 2 * TICK,
                quantity: 5 * QTY,
                orderType: 0,
                tif: 2, // FOK
                clientOrderId: FixedBytes::default(),
            }
            .abi_encode(),
            "FOK order cannot be fully filled",
        );
        dex_call_expect_revert(
            &mut ctx,
            ALICE,
            &placeOrderCall {
                marketId: MARKET_ID,
                side: 0,
                price: PRICE,
                quantity: QTY,
                orderType: 0,
                tif: 3, // PostOnly
                clientOrderId: FixedBytes::default(),
            }
            .abi_encode(),
            "PostOnly order would match",
        );

        // Market taker (orderType = 1, price ignored): fills exactly L1's
        // head; the tail survives through the early-exit save_ask_level.
        let _alice_taker1 = g_place(&mut ctx, ALICE, 0, 0, QTY, 1, 1);
        assert_eq!(
            order_status(&mut ctx, bob_l1a),
            DELETED,
            "L1 head maker must be filled (delete-on-terminal → removed from map)"
        );
        assert_eq!(
            order_status(&mut ctx, bob_l1b),
            OrderStatus::Open as u8,
            "L1 tail maker must survive the buy-side early-exit"
        );
        let _alice_sweep = g_place(&mut ctx, ALICE, 0, PRICE + 2 * TICK, 3 * QTY, 0, 0);

        // Market order against the now-empty ask book: the full remainder
        // expires on the market path (execute_market_order →
        // cancel_unfilled_remainder), with no book or balance writes.
        let mkt = g_place(&mut ctx, ALICE, 0, 0, QTY, 1, 1);

        // PostOnly ask rests away from the market, then the only sell-side
        // user cancel (remove_ask_price + refresh_best_ask + save_ask_level +
        // save_sell_orders + sell-side margin release).
        let po_ask = g_place(&mut ctx, BOB, 1, PRICE + 10 * TICK, QTY, 0, 3);
        dex_call(
            &mut ctx,
            BOB,
            &cancelOrderCall {
                orderId: po_ask.into(),
                marketId: MARKET_ID,
            }
            .abi_encode(),
        );

        // GTC that rests, then explicit cancel; cancelling again must revert.
        let gtc = g_place(&mut ctx, ALICE, 0, PRICE - 10 * TICK, QTY, 0, 0);
        dex_call(
            &mut ctx,
            ALICE,
            &cancelOrderCall {
                orderId: gtc.into(),
                marketId: MARKET_ID,
            }
            .abi_encode(),
        );
        dex_call_expect_revert(
            &mut ctx,
            ALICE,
            &cancelOrderCall {
                orderId: gtc.into(),
                marketId: MARKET_ID,
            }
            .abi_encode(),
            // delete-on-terminal: the first cancel deleted the record, so re-cancelling it is now
            // "order not found" (was "not cancellable" when the Cancelled record lingered). Both
            // reject the replay.
            "order not found",
        );

        // IOC with no crossing liquidity → Expired.
        let ioc = g_place(&mut ctx, ALICE, 1, PRICE + 50 * TICK, QTY, 0, 1);

        // Signed cancel of the signed resting buy.
        dex_call(&mut ctx, CAROL, &signed_cancel_input(&sk, signed_buy));

        // Phase 7 — isolated margin ops.
        dex_call(
            &mut ctx,
            ALICE,
            &addPositionMarginCall {
                marketId: MARKET_ID,
                amount: 500_000,
            }
            .abi_encode(),
        );

        // Phase 8 — funding: one mid-epoch sample, then cross the epoch
        // boundary (FundingRateComputed + cumulative-index step).
        dex_call(
            &mut ctx,
            ORACLE,
            &updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: PRICE,
                timestamp: 30,
            }
            .abi_encode(),
        );
        dex_call(
            &mut ctx,
            ORACLE,
            &updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: PRICE,
                timestamp: 3_615,
            }
            .abi_encode(),
        );
        // Stale oracle update (same aligned timestamp): accepted but ignored —
        // it must not write, hence not fold.
        let c_stale = read_commitment(&mut ctx);
        dex_call(
            &mut ctx,
            ORACLE,
            &updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: PRICE + TICK,
                timestamp: 3_616,
            }
            .abi_encode(),
        );
        assert_eq!(
            read_commitment(&mut ctx),
            c_stale,
            "ignored stale oracle update must not fold"
        );

        // Margin op AFTER the epoch boundary → settle_position_funding runs
        // with a nonzero index delta (lazy funding settlement on ALICE's long).
        dex_call(
            &mut ctx,
            ALICE,
            &removePositionMarginCall {
                marketId: MARKET_ID,
                amount: 250_000,
            }
            .abi_encode(),
        );

        // Phase 9 — insurance fund, crash, liquidation.
        dex_call(
            &mut ctx,
            ADMIN,
            &depositInsuranceFundCall { amount: 50_000_000 }.abi_encode(),
        );
        dex_call(
            &mut ctx,
            ADMIN,
            &withdrawInsuranceFundCall { amount: 1_000_000 }.abi_encode(),
        );
        dex_call_expect_revert(
            &mut ctx,
            BOB,
            &withdrawInsuranceFundCall { amount: 1 }.abi_encode(),
            "not admin",
        );

        // ALICE leaves a resting bid AND a resting ask so the liquidation's
        // cancel-all clears both book sides. The ask is fully offset by her
        // 4×QTY long, so it reserves no margin — only the maker fee.
        let alice_resting_bid = g_place(&mut ctx, ALICE, 0, PRICE - 30 * TICK, QTY, 0, 0);
        let alice_resting_ask = g_place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);

        // BOB quotes only 3×QTY of closing liquidity — resting BEFORE the crash so
        // the auto-liquidation sweep has it to close against: the sweep closes 3×QTY
        // through the book and settles the residual QTY at mark price.
        let bob_bid = g_place(&mut ctx, BOB, 0, PRICE - 20 * TICK, 3 * QTY, 0, 0);

        // Crash: index $100 → $70; ALICE's 3x long drops under maintenance and the
        // sweep inside updateIndexPrice liquidates her automatically (liquidator = 0x0).
        // Depth note: at 3x the liquidation boundary sits at a 27% drop (equity
        // `margin − loss` vs the `notional/6` threshold), so $80 — which liquidated the
        // pre-cap 5x long exactly — no longer does. $70 clears it with margin rather than
        // sitting on the boundary, so the scenario is not knife-edge. BOB's bid at $80 is
        // still crossable (the golden market runs with the band disabled), so the sweep
        // keeps closing 3×QTY through the book and settling the residual at mark.
        dex_call(
            &mut ctx,
            ORACLE,
            &updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: PRICE - 30 * TICK,
                timestamp: 3_630,
            }
            .abi_encode(),
        );

        // The sweep already closed ALICE, so a manual liquidate now finds no position.
        dex_call_expect_revert(
            &mut ctx,
            CAROL,
            &liquidateCall {
                user: ALICE,
                marketId: MARKET_ID,
            }
            .abi_encode(),
            "liquidate: no open position",
        );

        // Phase 9b — CAROL funds up; BOB quotes two same-price bids and CAROL's
        // taker sell consumes exactly the head (closing BOB's residual short),
        // so the tail survives via the SELL-side matcher early-exit.
        dex_call(
            &mut ctx,
            CAROL,
            &depositCall {
                amount: U256::from(10_000_000u64),
            }
            .abi_encode(),
        );
        dex_call(
            &mut ctx,
            CAROL,
            &transferToPerpCall { amount: 5_000_000 }.abi_encode(),
        );
        let bob_head_bid = g_place(&mut ctx, BOB, 0, PRICE - 20 * TICK, QTY, 0, 0);
        let bob_tail_bid = g_place(&mut ctx, BOB, 0, PRICE - 20 * TICK, QTY, 0, 0);
        let carol_close = g_place(&mut ctx, CAROL, 1, PRICE - 20 * TICK, QTY, 0, 0);
        assert_eq!(
            order_status(&mut ctx, bob_head_bid),
            DELETED,
            "head bid must be filled by CAROL's taker sell (delete-on-terminal → removed)"
        );
        let tail_queue = getBookLevelCall::abi_decode_returns(&dex_view(
            &mut ctx,
            &getBookLevelCall {
                marketId: MARKET_ID,
                side: 0,
                price: PRICE - 20 * TICK,
            }
            .abi_encode(),
        ))
        .unwrap();
        assert_eq!(
            tail_queue,
            vec![FixedBytes(bob_tail_bid)],
            "tail bid must survive the sell-side early-exit"
        );

        // Phase 9c — a resting SELL against a NON-FLAT position, priced at the ASSUMING PRICE.
        //
        // This scenario had no such shape, which is why its BusinessSnapshot was structurally blind
        // to how `Ask` is priced: BOB's surviving tail bid is a BUY (a long order's Assuming Price IS
        // its limit price — no markup), and everyone else ends flat. Two calls fix that:
        //
        //   1. an oracle bump that lifts the MARK above the book. A resting sell must sit above the
        //      best bid, and the last print is AT the touch, so `ROUND_UP(lastTraded × 1.0015)` is
        //      only 15 bps above it — under this market's $1 tick on an $80 asset there is no
        //      placeable price in that gap. The `Mark` branch of `max(Last × 1.0015, Mark, limit)`
        //      is the reachable one, and it needs a mark above the book.
        //   2. CAROL — SHORT QTY, so a sell INCREASES her exposure and is genuinely charged — rests
        //      a sell inside the mark.
        //
        // Nobody is liquidated by the bump (ALICE and BOB are flat; CAROL's equity at the new mark
        // is far above her maintenance requirement), so the sweep is a no-op and only the mark, the
        // index blobs and CAROL's derived requirement move.
        dex_call(
            &mut ctx,
            ORACLE,
            &updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: PRICE - 10 * TICK,
                timestamp: 3_645,
            }
            .abi_encode(),
        );
        let carol_ask = g_place(&mut ctx, CAROL, 1, PRICE - 19 * TICK, QTY, 0, 0);
        assert_eq!(
            order_status(&mut ctx, carol_ask),
            OrderStatus::Open as u8,
            "CAROL's ask must REST (it is above the surviving 80-tick bid), not fill"
        );

        // Phase 10 — solvent exit + api-key delete (empty-blob fold).
        dex_call(
            &mut ctx,
            BOB,
            &transferFromPerpCall {
                amount: 500_000_000,
            }
            .abi_encode(),
        );
        dex_call(
            &mut ctx,
            BOB,
            &withdrawCall {
                amount: U256::from(500_000_000u64),
            }
            .abi_encode(),
        );
        dex_call(&mut ctx, ALICE, &revokeApiKeyCall { keyId: 0 }.abi_encode());

        // Hand over admin last, after all fee-sink activity (the snapshot reads
        // ADMIN's account explicitly, so the handover does not disturb it).
        // Auth + zero-address gates first, under rolled-back checkpoints.
        dex_call_expect_revert(
            &mut ctx,
            ADMIN,
            &transferAdminCall {
                newAdmin: Address::ZERO,
            }
            .abi_encode(),
            "cannot be zero address",
        );
        dex_call_expect_revert(
            &mut ctx,
            BOB,
            &transferAdminCall { newAdmin: BOB }.abi_encode(),
            "caller is not admin",
        );
        dex_call(
            &mut ctx,
            ADMIN,
            &transferAdminCall {
                newAdmin: NEW_ADMIN,
            }
            .abi_encode(),
        );

        // View stretch #2 — pure reads (under #16d the slot is untouched until the block-end
        // finalize below, so it stays at genesis throughout the scenario).
        dex_view(
            &mut ctx,
            &getAveragePremiumIndexCall {
                marketId: MARKET_ID,
            }
            .abi_encode(),
        );
        let snapshot = take_snapshot(
            &mut ctx,
            &ScenarioOrderIds {
                signed_buy,
                gtc,
                ioc,
                mkt,
                po_ask,
                alice_resting_bid,
                alice_resting_ask,
                bob_bid,
                bob_tail_bid,
                carol_close,
                carol_ask,
            },
        );
        // #16d: the commitment is folded ONCE at block end. The snapshot above was read while the
        // overlay was intact; now harvest the net delta and finalize it onto 0x1003, exactly as the
        // block executor does (take_perp_delta → finalize_block_commitment) before the state root.
        let delta = ctx.journal_mut().take_perp_delta();
        storage::finalize_block_commitment(&mut ctx, &delta).unwrap();
        let commitment = read_commitment(&mut ctx);

        (B256::from(commitment.to_be_bytes::<32>()), snapshot)
    }

    /// Order IDs collected while the scenario runs, queried by the snapshot.
    struct ScenarioOrderIds {
        signed_buy: [u8; 32],
        gtc: [u8; 32],
        ioc: [u8; 32],
        mkt: [u8; 32],
        po_ask: [u8; 32],
        alice_resting_bid: [u8; 32],
        alice_resting_ask: [u8; 32],
        bob_bid: [u8; 32],
        bob_tail_bid: [u8; 32],
        carol_close: [u8; 32],
        carol_ask: [u8; 32],
    }

    fn take_snapshot(ctx: &mut TestCtx, ids: &ScenarioOrderIds) -> BusinessSnapshot {
        let alice_pos = getPositionCall::abi_decode_returns(&dex_view(
            ctx,
            &getPositionCall {
                user: ALICE,
                marketId: MARKET_ID,
            }
            .abi_encode(),
        ))
        .unwrap();
        let bob_pos = getPositionCall::abi_decode_returns(&dex_view(
            ctx,
            &getPositionCall {
                user: BOB,
                marketId: MARKET_ID,
            }
            .abi_encode(),
        ))
        .unwrap();
        let carol_pos = getPositionCall::abi_decode_returns(&dex_view(
            ctx,
            &getPositionCall {
                user: CAROL,
                marketId: MARKET_ID,
            }
            .abi_encode(),
        ))
        .unwrap();
        let alice_acct = getAccountCall::abi_decode_returns(&dex_view(
            ctx,
            &getAccountCall { user: ALICE }.abi_encode(),
        ))
        .unwrap();
        let bob_acct = getAccountCall::abi_decode_returns(&dex_view(
            ctx,
            &getAccountCall { user: BOB }.abi_encode(),
        ))
        .unwrap();
        let carol_acct = getAccountCall::abi_decode_returns(&dex_view(
            ctx,
            &getAccountCall { user: CAROL }.abi_encode(),
        ))
        .unwrap();
        let admin_acct = getAccountCall::abi_decode_returns(&dex_view(
            ctx,
            &getAccountCall { user: ADMIN }.abi_encode(),
        ))
        .unwrap();
        let insurance_fund = getInsuranceFundCall::abi_decode_returns(&dex_view(
            ctx,
            &getInsuranceFundCall {}.abi_encode(),
        ))
        .unwrap();
        let market_fee_total = getMarketFeeTotalCall::abi_decode_returns(&dex_view(
            ctx,
            &getMarketFeeTotalCall {
                marketId: MARKET_ID,
            }
            .abi_encode(),
        ))
        .unwrap();
        let mark_price = getMarkPriceCall::abi_decode_returns(&dex_view(
            ctx,
            &getMarkPriceCall {
                marketId: MARKET_ID,
            }
            .abi_encode(),
        ))
        .unwrap();
        let funding = getFundingStateCall::abi_decode_returns(&dex_view(
            ctx,
            &getFundingStateCall {
                marketId: MARKET_ID,
            }
            .abi_encode(),
        ))
        .unwrap();
        let bob_erc20 = storage::load_erc20_balance(ctx, USDC_ADDRESS, BOB).unwrap();

        BusinessSnapshot {
            alice_position: (alice_pos.amount, alice_pos.vQuoteBalance, alice_pos.margin),
            bob_position: (bob_pos.amount, bob_pos.vQuoteBalance, bob_pos.margin),
            carol_position: (carol_pos.amount, carol_pos.vQuoteBalance, carol_pos.margin),
            alice_account: (alice_acct.usdcBalance, alice_acct.availableBalance),
            bob_account: (bob_acct.usdcBalance, bob_acct.availableBalance),
            carol_account: (carol_acct.usdcBalance, carol_acct.availableBalance),
            bob_erc20,
            admin_perp_wallet: admin_acct.availableBalance,
            insurance_fund,
            market_fee_total,
            mark_price,
            funding: (funding.lastFundingRate, funding.nextFundingTs),
            signed_buy_status: order_status(ctx, ids.signed_buy),
            gtc_cancelled_status: order_status(ctx, ids.gtc),
            ioc_status: order_status(ctx, ids.ioc),
            mkt_expired_status: order_status(ctx, ids.mkt),
            po_ask_cancelled_status: order_status(ctx, ids.po_ask),
            liq_cancelled_bid_status: order_status(ctx, ids.alice_resting_bid),
            liq_cancelled_ask_status: order_status(ctx, ids.alice_resting_ask),
            bob_bid_status: order_status(ctx, ids.bob_bid),
            bob_tail_bid_status: order_status(ctx, ids.bob_tail_bid),
            carol_close_status: order_status(ctx, ids.carol_close),
            carol_ask_status: order_status(ctx, ids.carol_ask),
        }
    }

    // ── The guards ─────────────────────────────────────────────────────────

    #[test]
    fn commitment_golden_scenario() {
        let (commitment, snapshot) = run_golden_scenario();
        // Printed for the (re-)pin procedure — visible with --nocapture or on failure.
        println!("golden commitment = {commitment}");
        println!("golden snapshot = {snapshot:#?}");
        assert_eq!(
            commitment, GOLDEN_COMMITMENT,
            "perp write-stream commitment drifted: SAFE refactors must keep it \
             bit-identical; only CHAIN changes may re-pin (see module docs)"
        );
        assert_eq!(
            snapshot,
            expected_snapshot(),
            "business end-state drifted — semantics changed, not just the hash"
        );
    }

    #[test]
    fn commitment_golden_deterministic() {
        let first = run_golden_scenario();
        let second = run_golden_scenario();
        assert_eq!(first.0, second.0, "commitment must be deterministic");
        assert_eq!(
            first.1, second.1,
            "business end-state must be deterministic"
        );
    }
}

// ── commit-only #23 conservation repro (realisticMix-shaped, EVM-framed) ─────────
#[cfg(test)]
mod commit_only_conservation {
    use super::*;
    use crate::{math::calc_value, run_perp_dex_call};

    const N_ACCT: u64 = 12;
    const ACCT_WALLET: u64 = 3_000_000; // TIGHT: forces wallet-cover cancels + insolvency rejects

    fn addr(i: u64) -> Address {
        let mut b = [0u8; 20];
        b[11] = 0xE0;
        b[12..20].copy_from_slice(&i.to_be_bytes());
        Address::from(b)
    }

    fn total_equity(ctx: &mut TestCtx) -> i128 {
        let mut s: i128 = 0;
        for i in 0..N_ACCT {
            let a = addr(i);
            let acc = storage::load_account(ctx, a).unwrap();
            s += acc.perp_wallet_balance as i128;
            let p = storage::load_position(ctx, a, MARKET_ID).unwrap();
            // Only `margin` is physically held now — the open-order requirement is derived and
            // was never taken out of the wallet, so there is no reservation term to add back.
            s += p.margin as i128;
            let mv = calc_value(PRICE, p.amount.unsigned_abs(), 8, 9).unwrap() as i128;
            s += if p.amount >= 0 { mv } else { -mv };
            s += p.v_quote_balance as i128;
        }
        s += storage::load_insurance_fund(ctx).unwrap() as i128;
        s += market_fee_total(ctx) as i128;
        s += storage::load_account(ctx, ADMIN)
            .unwrap()
            .perp_wallet_balance as i128;
        s
    }

    /// One order through the FULL entry point, EVM-framed exactly like on-chain: checkpoint,
    /// run, commit on success / revert on reverted-or-error, then commit_tx (tx boundary).
    /// Returns the order id on a successful place.
    fn framed_place(
        ctx: &mut TestCtx,
        c: Address,
        side: u8,
        price: u64,
        qty: u64,
        tif: u8,
    ) -> Option<[u8; 32]> {
        let input = placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: 0,
            tif,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode();
        let cp = ctx.journal_mut().checkpoint();
        let out = run_perp_dex_call(&input, 10_000_000, c, U256::ZERO, false, ctx)
            .expect("must not hard-fail");
        let id = if out.reverted {
            ctx.journal_mut().checkpoint_revert(cp);
            None
        } else {
            ctx.journal_mut().checkpoint_commit();
            Some(out.bytes[..32].try_into().unwrap())
        };
        ctx.journal_mut().commit_tx();
        id
    }

    fn framed_cancel(ctx: &mut TestCtx, c: Address, oid: [u8; 32]) {
        let input = cancelOrderCall {
            orderId: oid.into(),
            marketId: MARKET_ID,
        }
        .abi_encode();
        let cp = ctx.journal_mut().checkpoint();
        let out = run_perp_dex_call(&input, 10_000_000, c, U256::ZERO, false, ctx)
            .expect("must not hard-fail");
        if out.reverted {
            ctx.journal_mut().checkpoint_revert(cp);
        } else {
            ctx.journal_mut().checkpoint_commit();
        }
        ctx.journal_mut().commit_tx();
    }

    #[test]
    fn realistic_mix_conserves_equity() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
        for i in 0..N_ACCT {
            fund(&mut ctx, addr(i), ACCT_WALLET);
        }
        let initial = total_equity(&mut ctx);

        let mut s: u64 = 0x243F6A8885A308D3;
        let mut rng = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut resting: Vec<Option<[u8; 32]>> = vec![None; N_ACCT as usize];

        for op in 0..6000u32 {
            let ai = (rng() % N_ACCT) as usize;
            let c = addr(ai as u64);
            // Probabilistic requote (30%): otherwise let orders REST to build a deep multi-level book.
            if resting[ai].is_some() && rng() % 100 < 30 {
                framed_cancel(&mut ctx, c, resting[ai].take().unwrap());
            } else {
                let roll = rng() % 100;
                let qty = QTY * (1 + rng() % 8);
                // Wide bands (±6 ticks) → many distinct price levels → multi-level sweeps.
                let placed = if roll < 25 {
                    framed_place(&mut ctx, c, 0, PRICE - TICK * (1 + rng() % 6), qty, 3)
                } else if roll < 50 {
                    framed_place(&mut ctx, c, 1, PRICE + TICK * (1 + rng() % 6), qty, 3)
                } else if roll < 70 {
                    // IOC Buy taker crossing the WHOLE ask band (sweeps multiple sell levels)
                    framed_place(&mut ctx, c, 0, PRICE + TICK * 7, qty, 1)
                } else if roll < 85 {
                    framed_place(&mut ctx, c, 1, PRICE - TICK * 7, qty, 1)
                } else if roll < 95 {
                    let side = (rng() % 2) as u8;
                    let price = if side == 0 {
                        PRICE + TICK * (1 + rng() % 6)
                    } else {
                        PRICE - TICK * (1 + rng() % 6)
                    };
                    framed_place(&mut ctx, c, side, price, qty, 0)
                } else {
                    let side = (rng() % 2) as u8;
                    let price = if side == 0 {
                        PRICE + TICK * 7
                    } else {
                        PRICE - TICK * 7
                    };
                    framed_place(&mut ctx, c, side, price, qty, 2)
                };
                // Remember any order that may have rested (maker sides + non-crossing GTC).
                if let Some(id) = placed {
                    resting[ai] = Some(id);
                }
            }
            // Per-op conservation: total system equity is invariant (no funding / no mark move /
            // no bad debt). A residual write-then-error leaks value and breaks this immediately.
            let now = total_equity(&mut ctx);
            assert_eq!(
                now,
                initial,
                "equity drifted by {} at op {op}",
                now - initial
            );
        }
    }
}

// ── Batch cancel (Phase 1) ─────────────────────────────────────────────────
//
// Covers the batch SHELL that `batchPlaceOrders` (Phase 2) reuses: pre-decode length validation,
// dynamic gas, the numeric reason codes, the index-aligned 34-byte status blob, the abort-forward
// driver's runtime genuine-vs-abort classification, and the one-signature-per-batch replay guard.
mod batch_cancel {
    use super::*;
    use crate::{
                batch::{
            self, PerpBatchReason, PerpBatchTag, BASE_BATCH_GAS, BATCH_STATUS_RECORD_LEN,
            MAX_BATCH_CANCEL,
        },
        errors::{perp_err, perp_fatal_invariant_err, perp_invariant_err},
        interface::IPerpDex::{
            batchCancelOrdersCall, batchCancelOrdersSignedCall, batchPlaceOrdersCall,
            batchPlaceOrdersSignedCall, cancelOrderCall, cancelOrderSignedCall, OrderCancelled,
        },
        call::selectors_map,
        types::{ApiKey, Order, OrderStatus, OrderType, Side, TimeInForce},
        CANCEL_ORDER_GAS, PLACE_ORDER_GAS,
        PerpError,
    };
    use ed25519_dalek::{Signer, SigningKey};

    // Shared with the `batch_place` module below (same shell, same signed-call scaffolding).
    pub(super) const SIGNED_TS: u64 = 1; // == BlockEnv::default() timestamp
    pub(super) const SIGNED_RECV: u64 = 60;

    /// Serialises every test that asserts an exact delta on `PERP_BATCH_ABORT_COUNT`.
    ///
    /// That counter is deliberately PROCESS-WIDE (it is a diagnostic tripwire, not per-call
    /// state), and cargo runs tests in parallel — so two tests that each abort once can
    /// interleave between one test's `before` read and its `after` read, making a
    /// `before + 1` assertion observe `+2`. Holding this lock across the read-run-read
    /// window keeps the exact-delta assertion (which is the property worth pinning: the
    /// call aborted exactly once) instead of weakening it to "the counter moved".
    ///
    /// Poisoning is ignored: a panic in one of these tests is already a test failure, and
    /// propagating the poison would turn it into a cascade of unrelated failures.
    pub(super) static ABORT_COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes [`ABORT_COUNTER_LOCK`], ignoring poisoning. Hold the guard for the whole
    /// read-run-read window.
    pub(super) fn lock_abort_counter() -> std::sync::MutexGuard<'static, ()> {
        ABORT_COUNTER_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ── helpers ────────────────────────────────────────────────────────────

    fn direct_calldata(ids: &[[u8; 32]]) -> Vec<u8> {
        batchCancelOrdersCall {
            orderIds: ids.iter().map(|i| FixedBytes(*i)).collect(),
        }
        .abi_encode()
    }

    /// One decoded status record.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Status {
        pub(super) tag: u8,
        pub(super) order_id: [u8; 32],
        pub(super) reason: u8,
    }

    pub(super) fn decode_statuses(blob: &[u8]) -> Vec<Status> {
        assert_eq!(
            blob.len() % BATCH_STATUS_RECORD_LEN,
            0,
            "status blob must be a whole number of fixed-width records"
        );
        blob.chunks(BATCH_STATUS_RECORD_LEN)
            .map(|r| Status {
                tag: r[0],
                order_id: r[1..33].try_into().unwrap(),
                reason: r[33],
            })
            .collect()
    }

    pub(super) fn expect(tag: PerpBatchTag, id: [u8; 32], reason: PerpBatchReason) -> Status {
        Status {
            tag: tag as u8,
            order_id: id,
            reason: reason as u8,
        }
    }

    /// Runs a batch through the real entry point and returns the raw statuses blob.
    fn batch_cancel(ctx: &mut TestCtx, caller: Address, ids: &[[u8; 32]]) -> Vec<u8> {
        let out = run_perp_dex_call(
            &direct_calldata(ids),
            30_000_000,
            caller,
            U256::ZERO,
            false,
            ctx,
        )
        .expect("batch must not hard-fail");
        assert!(
            !out.reverted,
            "batch must return Ok once the loop has begun (reverted with {:?})",
            out.bytes
        );
        assert_eq!(
            out.gas_used,
            BASE_BATCH_GAS + ids.len() as u64 * CANCEL_ORDER_GAS,
            "the FULL dynamic cost must be what the output charges"
        );
        batchCancelOrdersCall::abi_decode_returns(&out.bytes)
            .unwrap()
            .to_vec()
    }

    /// Directly stores an order record that was never inserted into any book level.
    fn orphan_order(
        ctx: &mut TestCtx,
        id: [u8; 32],
        owner: Address,
        market_id: u64,
        side: Side,
        price: u64,
        status: OrderStatus,
    ) {
        storage::save_order(
            ctx,
            &id,
            &Order {
                owner: owner.0 .0,
                market_id,
                side,
                price,
                quantity: QTY,
                filled: 0,
                order_type: OrderType::Limit,
                tif: TimeInForce::Gtc,
                status,
            },
        )
        .unwrap();
    }

    fn cancelled_ids(ctx: &mut TestCtx) -> Vec<[u8; 32]> {
        JournalTr::take_logs(ctx.journal_mut())
            .into_iter()
            .filter(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
            .map(|l| {
                OrderCancelled::decode_raw_log(l.data.topics(), &l.data.data)
                    .unwrap()
                    .orderId
                    .0
            })
            .collect()
    }

    // ── 1. happy path ──────────────────────────────────────────────────────

    #[test]
    fn batch_cancel_all_accepted_and_index_aligned() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let b = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
        let c = place(&mut ctx, ALICE, 1, PRICE + TICK, QTY, 0, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let ids = [a, b, c];
        let blob = batch_cancel(&mut ctx, ALICE, &ids);

        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(PerpBatchTag::Accepted, a, PerpBatchReason::None),
                expect(PerpBatchTag::Accepted, b, PerpBatchReason::None),
                expect(PerpBatchTag::Accepted, c, PerpBatchReason::None),
            ]
        );
        for id in ids {
            assert_terminal(&mut ctx, id);
        }
        // Logs come out in processing order == calldata order.
        assert_eq!(cancelled_ids(&mut ctx), vec![a, b, c]);
        // Margin fully released.
        assert_eq!(wallet(&mut ctx, ALICE), WALLET);
    }

    // ── 2. mixed: genuine rejects do not stop the loop ─────────────────────

    #[test]
    fn batch_cancel_mixed_rejects_continue_the_loop() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let good_a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let good_b = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
        let bobs = place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);

        let missing = [0x11u8; 32];
        // status Filled → present in the map but not cancellable.
        let not_cancellable = [0x22u8; 32];
        orphan_order(
            &mut ctx,
            not_cancellable,
            ALICE,
            MARKET_ID,
            Side::Buy,
            PRICE,
            OrderStatus::Filled,
        );
        // market 999 was never registered → "unknown market".
        let unknown_market = [0x33u8; 32];
        orphan_order(
            &mut ctx,
            unknown_market,
            ALICE,
            999,
            Side::Buy,
            PRICE,
            OrderStatus::Open,
        );
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let ids = [
            missing,
            good_a,
            bobs,
            not_cancellable,
            unknown_market,
            good_b,
        ];
        let blob = batch_cancel(&mut ctx, ALICE, &ids);

        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(
                    PerpBatchTag::Rejected,
                    missing,
                    PerpBatchReason::OrderNotFound
                ),
                expect(PerpBatchTag::Accepted, good_a, PerpBatchReason::None),
                expect(PerpBatchTag::Rejected, bobs, PerpBatchReason::NotOwner),
                expect(
                    PerpBatchTag::Rejected,
                    not_cancellable,
                    PerpBatchReason::NotCancellable
                ),
                expect(
                    PerpBatchTag::Rejected,
                    unknown_market,
                    PerpBatchReason::UnknownMarket
                ),
                expect(PerpBatchTag::Accepted, good_b, PerpBatchReason::None),
            ]
        );
        // The two valid ids were still cancelled, in calldata order; nothing else was touched.
        assert_eq!(cancelled_ids(&mut ctx), vec![good_a, good_b]);
        assert_terminal(&mut ctx, good_a);
        assert_terminal(&mut ctx, good_b);
        assert!(storage::load_order(&mut ctx, &bobs).unwrap().is_some());
        assert!(storage::load_order(&mut ctx, &not_cancellable)
            .unwrap()
            .is_some());
        assert!(storage::load_order(&mut ctx, &unknown_market)
            .unwrap()
            .is_some());
    }

    /// A genuine reject must be write-clean — that is the property the driver's runtime
    /// classification keys off, so pin it directly for all four cancel rejects.
    #[test]
    fn every_genuine_cancel_reject_is_write_clean() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let bobs = place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);
        orphan_order(
            &mut ctx,
            [0x22u8; 32],
            ALICE,
            MARKET_ID,
            Side::Buy,
            PRICE,
            OrderStatus::Filled,
        );
        orphan_order(
            &mut ctx,
            [0x33u8; 32],
            ALICE,
            999,
            Side::Buy,
            PRICE,
            OrderStatus::Open,
        );

        for id in [[0x11u8; 32], bobs, [0x22u8; 32], [0x33u8; 32]] {
            let before = JournalTr::perp_write_count(ctx.journal_mut());
            let err = cancel_order_core(ALICE, id, &mut ctx).unwrap_err();
            assert_eq!(
                JournalTr::perp_write_count(ctx.journal_mut()),
                before,
                "reject {err:?} must not bump the perp write counter"
            );
        }
    }

    // ── 3. abort-forward ───────────────────────────────────────────────────

    /// Real post-write error: an order whose price level is absent from the book. `decr_level_count`
    /// WRITES the (materialised, count-0) level, and only then does the sell-side stale-BBO guard in
    /// `remove_from_book_after_cancel` raise `[INVARIANT]` because the cached best_ask is 0. Under
    /// commit-only that write cannot be undone → abort-forward.
    #[test]
    fn batch_cancel_aborts_forward_on_a_post_write_invariant() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        // Bids only, so best_ask stays 0.
        let good = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let untouched = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
        let orphan = [0x44u8; 32];
        orphan_order(
            &mut ctx,
            orphan,
            ALICE,
            MARKET_ID,
            Side::Sell,
            PRICE + TICK,
            OrderStatus::Open,
        );
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let _abort_guard = lock_abort_counter();
        let aborts_before = batch::perp_batch_abort_count();

        let ids = [good, orphan, untouched];
        // batch_cancel() already asserts the call did NOT revert.
        let blob = batch_cancel(&mut ctx, ALICE, &ids);

        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(PerpBatchTag::Accepted, good, PerpBatchReason::None),
                expect(PerpBatchTag::Aborted, orphan, PerpBatchReason::Invariant),
                expect(PerpBatchTag::NotAttempted, untouched, PerpBatchReason::None),
            ]
        );
        assert_eq!(batch::perp_batch_abort_count(), aborts_before + 1);
        // The committed prefix survives WITH its logs (the whole point of abort-forward).
        assert_eq!(cancelled_ids(&mut ctx), vec![good]);
        assert_terminal(&mut ctx, good);
        // The tail was never attempted: still resting.
        assert!(storage::load_order(&mut ctx, &untouched).unwrap().is_some());
    }

    /// Driver-level pin of the three classification branches, fed synthetic errors so the mapping
    /// from "did this item bump the perp write counter?" to Rejected/Aborted is tested in isolation
    /// (no reliance on any error string).
    #[test]
    fn driver_classifies_by_write_count_not_by_message() {
        // Produces aborts (so it bumps the global counter) even though it asserts no delta —
        // it must still hold the lock or it corrupts a concurrent test's exact-delta assertion.
        let _abort_guard = lock_abort_counter();
        let echo = |k: usize| [k as u8; 32];
        // `drive_batch` now asks for (index, tag) — the tag lets the place path report the id an
        // ABORT burned; the cancel path (and this driver test) is tag-agnostic.
        let echo_at = |k: usize, _tag: PerpBatchTag| echo(k);

        // (a) write-clean error → Rejected, loop continues.
        let mut ctx = make_ctx();
        let run = batch::drive_batch(&mut ctx, ALICE, 3, echo_at, |_ctx, k| {
            if k == 1 {
                Err(perp_invariant_err("write-clean invariant"))
            } else {
                Ok((PerpBatchTag::Accepted, [k as u8; 32]))
            }
        })
        .unwrap();
        assert_eq!((run.accepted, run.aborted_at), (2, None));
        assert_eq!(
            decode_statuses(&run.statuses),
            vec![
                expect(PerpBatchTag::Accepted, echo(0), PerpBatchReason::None),
                expect(PerpBatchTag::Rejected, echo(1), PerpBatchReason::Invariant),
                expect(PerpBatchTag::Accepted, echo(2), PerpBatchReason::None),
            ],
            "an error that wrote NOTHING is a genuine reject even when it is an [INVARIANT]"
        );

        // (b) error AFTER a perp write → Aborted + NotAttempted tail, still Ok.
        let mut ctx = make_ctx();
        let run = batch::drive_batch(&mut ctx, ALICE, 4, echo_at, |ctx, k| {
            if k == 1 {
                storage::save_user_nonce(ctx, ALICE, 7)?;
                Err(perp_err("plain reject, but it wrote first"))
            } else {
                Ok((PerpBatchTag::Accepted, [k as u8; 32]))
            }
        })
        .unwrap();
        // The place path advances the order nonce by `accepted` + the aborted item's own id.
        assert_eq!((run.accepted, run.aborted_at), (1, Some(1)));
        assert_eq!(
            decode_statuses(&run.statuses),
            vec![
                expect(PerpBatchTag::Accepted, echo(0), PerpBatchReason::None),
                expect(PerpBatchTag::Aborted, echo(1), PerpBatchReason::Other),
                expect(PerpBatchTag::NotAttempted, echo(2), PerpBatchReason::None),
                expect(PerpBatchTag::NotAttempted, echo(3), PerpBatchReason::None),
            ],
            "a plain perp_err that WROTE is an abort, regardless of its message"
        );

        // (c) Fatal propagates untouched, even though the loop had already begun.
        let mut ctx = make_ctx();
        let err = batch::drive_batch(&mut ctx, ALICE, 3, echo_at, |_ctx, k| {
            if k == 1 {
                Err(perp_fatal_invariant_err("node-level"))
            } else {
                Ok((PerpBatchTag::Accepted, [k as u8; 32]))
            }
        })
        .unwrap_err();
        assert!(matches!(err, PerpError::Fatal(_)), "got {err:?}");
    }

    // ── 4. pre-loop length faults revert the whole call ────────────────────

    #[test]
    fn empty_and_oversized_batches_revert_write_clean() {
        for ids in [
            Vec::<[u8; 32]>::new(),
            vec![[0x11u8; 32]; MAX_BATCH_CANCEL + 1],
        ] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            let _ = JournalTr::take_logs(ctx.journal_mut());
            let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

            let out = run_perp_dex_call(
                &direct_calldata(&ids),
                u64::MAX, // never let gas be the reason
                ALICE,
                U256::ZERO,
                false,
                &mut ctx,
            )
            .unwrap();
            assert!(out.reverted, "N = {} must revert the whole call", ids.len());
            assert_eq!(
                JournalTr::perp_write_count(ctx.journal_mut()),
                writes_before,
                "a pre-loop fault must be write-clean"
            );
            assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
        }
        // …and MAX itself is accepted (all ids missing → all Rejected).
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let ids: Vec<[u8; 32]> = (0..MAX_BATCH_CANCEL)
            .map(|i| {
                let mut b = [0u8; 32];
                b[..8].copy_from_slice(&(i as u64).to_be_bytes());
                b
            })
            .collect();
        let blob = batch_cancel(&mut ctx, ALICE, &ids);
        assert_eq!(blob.len(), MAX_BATCH_CANCEL * BATCH_STATUS_RECORD_LEN);
        assert!(decode_statuses(&blob)
            .iter()
            .all(|s| s.tag == PerpBatchTag::Rejected as u8
                && s.reason == PerpBatchReason::OrderNotFound as u8));
    }

    // ── 5. gas ─────────────────────────────────────────────────────────────

    #[test]
    fn batch_unit_matches_single_selector_cost() {
        // The per-item unit must BE the single-selector cost, not a re-invented number.
        assert_eq!(
            selectors_map()
                .get(&cancelOrderCall::SELECTOR)
                .expect("cancelOrder in table")
                .0,
            CANCEL_ORDER_GAS
        );
        assert_eq!(
            selectors_map()
                .get(&cancelOrderSignedCall::SELECTOR)
                .unwrap()
                .0,
            CANCEL_ORDER_GAS
        );
        // Same for place — and the value itself is pinned, since it is now a shared const.
        assert_eq!(CANCEL_ORDER_GAS, 80_000);
        assert_eq!(PLACE_ORDER_GAS, 200_000);
        assert_eq!(
            selectors_map()
                .get(&placeOrderCall::SELECTOR)
                .expect("placeOrder in table")
                .0,
            PLACE_ORDER_GAS
        );
        assert_eq!(
            selectors_map()
                .get(&placeOrderSignedCall::SELECTOR)
                .unwrap()
                .0,
            PLACE_ORDER_GAS
        );
        // …and the batch selectors' table entry is only the ENVELOPE FLOOR.
        for sel in [
            batchCancelOrdersCall::SELECTOR,
            batchCancelOrdersSignedCall::SELECTOR,
            batchPlaceOrdersCall::SELECTOR,
            batchPlaceOrdersSignedCall::SELECTOR,
        ] {
            let (cost, can_be_static) = *selectors_map().get(&sel).expect("batch sel in table");
            assert_eq!(cost, BASE_BATCH_GAS);
            assert!(!can_be_static);
        }
    }

    #[test]
    fn insufficient_gas_limit_is_out_of_gas_before_any_write() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let b = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

        let full = BASE_BATCH_GAS + 2 * CANCEL_ORDER_GAS;
        let err = run_perp_dex_call(
            &direct_calldata(&[a, b]),
            full - 1,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, PerpError::OutOfGas), "got {err:?}");
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "OutOfGas must precede every write"
        );
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
        assert!(storage::load_order(&mut ctx, &a).unwrap().is_some());

        // Exactly the computed cost succeeds and charges exactly that.
        let out = run_perp_dex_call(
            &direct_calldata(&[a, b]),
            full,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(!out.reverted);
        assert_eq!(out.gas_used, full);
    }

    /// A huge DECLARED length must be rejected by the pre-decode read, before
    /// `DynSeqToken::decode_from` gets to `vec_try_with_capacity(len)` — i.e. no allocation, no
    /// panic, no OOM. Only the envelope floor is charged.
    #[test]
    fn hostile_length_word_reverts_without_allocating() {
        // selector || offset(0x20) || length || <no elements>
        let mut huge_u32 = vec![0u8; 4 + 64];
        huge_u32[..4].copy_from_slice(&batchCancelOrdersCall::SELECTOR);
        huge_u32[35] = 0x20;
        huge_u32[64..68].copy_from_slice(&u32::MAX.to_be_bytes()); // 4 294 967 295 elements

        let mut huge_u256 = huge_u32.clone();
        huge_u256[36..68].fill(0xff); // does not even fit u32

        // Non-canonical head offset (0x40 instead of 0x20) is rejected too.
        let mut bad_offset = huge_u32.clone();
        bad_offset[35] = 0x40;
        bad_offset[64..68].copy_from_slice(&1u32.to_be_bytes());

        // Truncated: length says 2 but no element bytes follow.
        let mut truncated = huge_u32.clone();
        truncated[64..68].copy_from_slice(&2u32.to_be_bytes());

        for (name, input) in [
            ("u32::MAX length", huge_u32),
            ("u256 length", huge_u256),
            ("non-canonical offset", bad_offset),
            ("length exceeds calldata", truncated),
        ] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
            let out =
                run_perp_dex_call(&input, u64::MAX, ALICE, U256::ZERO, false, &mut ctx).unwrap();
            assert!(out.reverted, "{name} must revert");
            assert_eq!(
                out.gas_used, BASE_BATCH_GAS,
                "{name}: an unreadable length word charges only the envelope floor"
            );
            assert_eq!(
                JournalTr::perp_write_count(ctx.journal_mut()),
                writes_before,
                "{name} must be write-clean"
            );
        }
    }

    #[test]
    fn gas_math_saturates_instead_of_wrapping() {
        // N near u64::MAX / unit must not wrap into a small number that passes the gas check.
        assert_eq!(batch::batch_gas(usize::MAX, CANCEL_ORDER_GAS), u64::MAX);
        assert_eq!(
            batch::batch_gas(3, CANCEL_ORDER_GAS),
            BASE_BATCH_GAS + 3 * CANCEL_ORDER_GAS
        );
        // A length word that would wrap `len * 32` is rejected outright by the pre-decode read.
        assert!(batch::batch_dynamic_gas(batchCancelOrdersCall::SELECTOR, &[0u8; 8]).is_none());
    }

    /// An over-cap batch reverts in `check_batch_len` having done ZERO work, so it must be billed the
    /// envelope floor only. `N = MAX + 1 = 257` would otherwise cost `20_000 + 257 * 80_000` ≈ 20.6M
    /// gas for a call that performs nothing at all.
    #[test]
    fn over_cap_batch_is_billed_only_the_envelope_floor() {
        let over = direct_calldata(&vec![[0x11u8; 32]; MAX_BATCH_CANCEL + 1]);
        assert_eq!(
            batch::batch_dynamic_gas(batchCancelOrdersCall::SELECTOR, &over),
            None,
            "an over-cap declared length must not be billed per item"
        );

        let mut ctx = make_ctx();
        setup(&mut ctx);
        let out = run_perp_dex_call(&over, u64::MAX, ALICE, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted, "over-cap must still revert the whole call");
        assert!(String::from_utf8_lossy(&out.bytes).contains("batch too large"));
        assert_eq!(out.gas_used, BASE_BATCH_GAS);

        // N == MAX is billed in full, so the clamp cannot silently under-bill a legal batch.
        let at_cap = direct_calldata(&vec![[0x11u8; 32]; MAX_BATCH_CANCEL]);
        assert_eq!(
            batch::batch_dynamic_gas(batchCancelOrdersCall::SELECTOR, &at_cap),
            Some(BASE_BATCH_GAS + MAX_BATCH_CANCEL as u64 * CANCEL_ORDER_GAS)
        );
    }

    // ── 6. signed batch ────────────────────────────────────────────────────

    fn signed_calldata(
        sk: &SigningKey,
        account: Address,
        key_id: u8,
        timestamp: u64,
        recv_window: u64,
        ids: &[[u8; 32]],
        // Ids actually put on the wire; `None` = same as the signed set.
        wire_ids: Option<&[[u8; 32]]>,
    ) -> Vec<u8> {
        let signed: Vec<FixedBytes<32>> = ids.iter().map(|i| FixedBytes(*i)).collect();
        let msg = batch_cancel_message(account, key_id, timestamp, recv_window, &signed);
        let sig = sk.sign(&msg);
        batchCancelOrdersSignedCall {
            account,
            keyId: key_id,
            timestamp,
            recvWindow: recv_window,
            orderIds: wire_ids
                .unwrap_or(ids)
                .iter()
                .map(|i| FixedBytes(*i))
                .collect(),
            signature: sig.to_bytes().to_vec().into(),
        }
        .abi_encode()
    }

    pub(super) fn register_key(ctx: &mut TestCtx, user: Address, sk: &SigningKey) {
        storage::save_api_key(
            ctx,
            user,
            0,
            ApiKey {
                pubkey: sk.verifying_key().to_bytes(),
                expiry: 0,
            },
        )
        .unwrap();
    }

    #[test]
    fn signed_batch_cancel_happy_path() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register_key(&mut ctx, ALICE, &sk);
        let a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let b = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let input = signed_calldata(&sk, ALICE, 0, SIGNED_TS, SIGNED_RECV, &[a, b], None);
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(!out.reverted, "{:?}", out.bytes);
        assert_eq!(out.gas_used, BASE_BATCH_GAS + 2 * CANCEL_ORDER_GAS);
        let blob = batchCancelOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(PerpBatchTag::Accepted, a, PerpBatchReason::None),
                expect(PerpBatchTag::Accepted, b, PerpBatchReason::None),
            ]
        );
        assert_eq!(cancelled_ids(&mut ctx), vec![a, b]);
    }

    #[test]
    fn signed_batch_rejects_tampered_content() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let a = [0xa1u8; 32];
        let b = [0xb2u8; 32];
        let c = [0xc3u8; 32];

        // (i) one id swapped on the wire; (ii) an id appended (N changes); (iii) order permuted.
        for (name, signed, wire) in [
            ("swapped id", vec![a, b], vec![a, c]),
            ("N grew", vec![a, b], vec![a, b, c]),
            ("N shrank", vec![a, b], vec![a]),
            ("order permuted", vec![a, b], vec![b, a]),
        ] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            register_key(&mut ctx, ALICE, &sk);
            let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
            let input =
                signed_calldata(&sk, ALICE, 0, SIGNED_TS, SIGNED_RECV, &signed, Some(&wire));
            let out =
                run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
            assert!(out.reverted, "{name} must fail verification");
            assert_eq!(
                JournalTr::perp_write_count(ctx.journal_mut()),
                writes_before,
                "{name}: signature failure is pre-write"
            );
        }
    }

    /// The single-order rule "a rejected signature stays replayable in-window" must NOT carry over:
    /// a batch returns Ok, so the signature is burned unconditionally after verification — even for
    /// a batch in which EVERY item was rejected.
    #[test]
    fn signed_batch_signature_is_burned_even_when_every_item_was_rejected() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register_key(&mut ctx, ALICE, &sk);

        let ids = [[0x11u8; 32], [0x12u8; 32]];
        let input = signed_calldata(&sk, ALICE, 0, SIGNED_TS, SIGNED_RECV, &ids, None);

        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(!out.reverted);
        let blob = batchCancelOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
        assert!(
            decode_statuses(&blob)
                .iter()
                .all(|s| s.tag == PerpBatchTag::Rejected as u8),
            "every item should have been rejected (unknown ids)"
        );

        // Replay of the very same signature reverts the WHOLE call.
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted, "replay must revert");
        let reason = String::from_utf8_lossy(&out.bytes).to_string();
        assert!(reason.contains("duplicate signature"), "got {reason}");
    }

    #[test]
    fn signed_batch_replay_after_partial_acceptance_reverts() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register_key(&mut ctx, ALICE, &sk);
        let a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let input = signed_calldata(
            &sk,
            ALICE,
            0,
            SIGNED_TS,
            SIGNED_RECV,
            &[a, [0x11u8; 32]],
            None,
        );

        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(!out.reverted);
        let blob = batchCancelOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
        let st = decode_statuses(&blob);
        assert_eq!(st[0].tag, PerpBatchTag::Accepted as u8);
        assert_eq!(st[1].tag, PerpBatchTag::Rejected as u8);

        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(
            out.reverted,
            "partially-accepted batch must not be replayable"
        );
    }

    #[test]
    fn signed_batch_requires_key_window_and_expiry() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let ids = [[0x11u8; 32]];

        // No key registered.
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let input = signed_calldata(&sk, ALICE, 0, SIGNED_TS, SIGNED_RECV, &ids, None);
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted);
        assert!(String::from_utf8_lossy(&out.bytes).contains("no api key"));

        // Timestamp in the future → outside recvWindow.
        let mut ctx = make_ctx();
        setup(&mut ctx);
        register_key(&mut ctx, ALICE, &sk);
        let input = signed_calldata(&sk, ALICE, 0, 10_000, SIGNED_RECV, &ids, None);
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted);
        assert!(String::from_utf8_lossy(&out.bytes).contains("future"));

        // Expired key.
        let mut ctx = make_ctx();
        setup(&mut ctx);
        storage::save_api_key(
            &mut ctx,
            ALICE,
            0,
            ApiKey {
                pubkey: sk.verifying_key().to_bytes(),
                expiry: 1,
            },
        )
        .unwrap();
        let input = signed_calldata(&sk, ALICE, 0, SIGNED_TS, SIGNED_RECV, &ids, None);
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted);
        assert!(String::from_utf8_lossy(&out.bytes).contains("expired"));
    }

    // ── 7. determinism + reason-code table ─────────────────────────────────

    #[test]
    fn status_blob_is_byte_identical_across_runs() {
        let scenario = || {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            let a = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
            let bobs = place(&mut ctx, BOB, 1, PRICE + TICK, QTY, 0, 0);
            let b = place(&mut ctx, ALICE, 0, PRICE - TICK, QTY, 0, 0);
            orphan_order(
                &mut ctx,
                [0x22u8; 32],
                ALICE,
                999,
                Side::Buy,
                PRICE,
                OrderStatus::Open,
            );
            let ids = [[0x11u8; 32], a, bobs, [0x22u8; 32], b];
            batch_cancel(&mut ctx, ALICE, &ids)
        };
        let first = scenario();
        assert_eq!(first, scenario());
        assert_eq!(first, scenario());
        assert_eq!(first.len(), 5 * BATCH_STATUS_RECORD_LEN);
    }

    /// Pins the reason-code mapping so a message rename fails here instead of silently degrading
    /// every code to `Other`. (The mapping is reporting-only — control flow uses the write counter.)
    #[test]
    fn reason_code_table_is_pinned() {
        use batch::reason_code;
        assert_eq!(
            reason_code(&perp_err("cancelOrder: order not found")),
            PerpBatchReason::OrderNotFound
        );
        assert_eq!(
            reason_code(&perp_err("cancelOrder: not owner")),
            PerpBatchReason::NotOwner
        );
        assert_eq!(
            reason_code(&perp_err("cancelOrder: order not cancellable")),
            PerpBatchReason::NotCancellable
        );
        assert_eq!(
            reason_code(&perp_err("cancelOrder: unknown market")),
            PerpBatchReason::UnknownMarket
        );
        // Place path (Phase 2). Every message below is copied from `validate_place_order` /
        // `execute_*` / `rest_in_book` / `finalize_compute`; a rename must fail HERE.
        for (msg, want) in [
            ("unknown market", PerpBatchReason::PlaceUnknownMarket),
            ("market not active", PerpBatchReason::MarketNotActive),
            ("invalid side", PerpBatchReason::InvalidSide),
            ("invalid orderType", PerpBatchReason::InvalidOrderType),
            ("invalid tif", PerpBatchReason::InvalidTif),
            (
                "tif not allowed for market order",
                PerpBatchReason::TifNotAllowedForOrderType,
            ),
            (
                "quantity below minimum",
                PerpBatchReason::QuantityBelowMinimum,
            ),
            (
                "quantity exceeds maximum",
                PerpBatchReason::QuantityAboveMaximum,
            ),
            (
                "quantity not multiple of step_size",
                PerpBatchReason::QuantityStepSize,
            ),
            ("limit order price must be > 0", PerpBatchReason::PriceZero),
            ("price exceeds maximum", PerpBatchReason::PriceAboveMaximum),
            (
                "price not multiple of tick_size",
                PerpBatchReason::PriceTickSize,
            ),
            (
                "PostOnly order would match",
                PerpBatchReason::PostOnlyWouldMatch,
            ),
            (
                "FOK order cannot be fully filled",
                PerpBatchReason::FokUnfillable,
            ),
            (
                "insufficient perp wallet for margin",
                PerpBatchReason::InsufficientMargin,
            ),
            (
                "open would breach maintenance margin",
                PerpBatchReason::OpenIntoInsolvency,
            ),
            (
                "fee recipient not initialised",
                PerpBatchReason::FeeRecipientNotSet,
            ),
            ("reserve delta overflow", PerpBatchReason::ArithmeticGuard),
            ("fee reserve overflow", PerpBatchReason::ArithmeticGuard),
            (
                "fills+rest requirement overflow",
                PerpBatchReason::ArithmeticGuard,
            ),
            ("total required overflow", PerpBatchReason::ArithmeticGuard),
            ("order nonce overflow", PerpBatchReason::ArithmeticGuard),
            ("brand new place reject", PerpBatchReason::Other),
        ] {
            assert_eq!(
                reason_code(&perp_err(format!("placeOrder: {msg}"))),
                want,
                "placeOrder: {msg}"
            );
        }
        // Non-`placeOrder:`-prefixed engine guards reachable from the place path.
        assert_eq!(
            reason_code(&perp_err("settlement: realized PnL overflow")),
            PerpBatchReason::ArithmeticGuard
        );
        assert_eq!(
            reason_code(&perp_err("perp wallet: amount exceeds i64::MAX")),
            PerpBatchReason::ArithmeticGuard
        );
        assert_eq!(
            reason_code(&perp_err("split_position_fill: opening value underflow")),
            PerpBatchReason::ArithmeticGuard
        );
        // Reject codes are PARTITIONED by path: a place reject never lands in the cancel band and
        // vice versa (both selectors have an "unknown market", and they must not share a code).
        assert_ne!(
            reason_code(&perp_err("placeOrder: unknown market")),
            reason_code(&perp_err("cancelOrder: unknown market"))
        );
        assert_eq!(
            reason_code(&perp_invariant_err("anything")),
            PerpBatchReason::Invariant
        );
        // An `[INVARIANT] ` prefix wins over any place/cancel text inside it.
        assert_eq!(
            reason_code(&perp_invariant_err("placeOrder: unknown market")),
            PerpBatchReason::Invariant
        );
        assert_eq!(
            reason_code(&perp_err("something brand new")),
            PerpBatchReason::Other
        );
        assert_eq!(
            reason_code(&PerpError::OutOfGas),
            PerpBatchReason::Other
        );
        // Tag / reason wire values are consensus-adjacent client contract: pin them.
        assert_eq!(
            [
                PerpBatchTag::Rejected as u8,
                PerpBatchTag::Accepted as u8,
                PerpBatchTag::Filled as u8,
                PerpBatchTag::Aborted as u8,
                PerpBatchTag::NotAttempted as u8,
            ],
            [0, 1, 2, 3, 4]
        );
        assert_eq!(BATCH_STATUS_RECORD_LEN, 34);
        assert_eq!(MAX_BATCH_CANCEL, 256);
        assert_eq!(crate::batch::MAX_BATCH_PLACE, 64);
        // Wire values of every reason code: consensus-adjacent client contract.
        assert_eq!(
            [
                PerpBatchReason::None as u8,
                PerpBatchReason::OrderNotFound as u8,
                PerpBatchReason::NotOwner as u8,
                PerpBatchReason::NotCancellable as u8,
                PerpBatchReason::UnknownMarket as u8,
                PerpBatchReason::PlaceUnknownMarket as u8,
                PerpBatchReason::MarketNotActive as u8,
                PerpBatchReason::InvalidSide as u8,
                PerpBatchReason::InvalidOrderType as u8,
                PerpBatchReason::InvalidTif as u8,
                PerpBatchReason::QuantityBelowMinimum as u8,
                PerpBatchReason::QuantityAboveMaximum as u8,
                PerpBatchReason::QuantityStepSize as u8,
                PerpBatchReason::PriceZero as u8,
                PerpBatchReason::PriceAboveMaximum as u8,
                PerpBatchReason::PriceTickSize as u8,
                PerpBatchReason::PostOnlyWouldMatch as u8,
                PerpBatchReason::FokUnfillable as u8,
                PerpBatchReason::InsufficientMargin as u8,
                PerpBatchReason::OpenIntoInsolvency as u8,
                PerpBatchReason::FeeRecipientNotSet as u8,
                PerpBatchReason::TifNotAllowedForOrderType as u8,
                PerpBatchReason::ArithmeticGuard as u8,
                PerpBatchReason::Invariant as u8,
                PerpBatchReason::Other as u8,
            ],
            [
                0, 1, 2, 3, 4, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
                253, 254, 255
            ]
        );
    }

    /// Batch selectors are not view calls.
    #[test]
    fn batch_selectors_are_not_static_callable() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let err = run_perp_dex_call(
            &direct_calldata(&[[0x11u8; 32]]),
            30_000_000,
            ALICE,
            U256::ZERO,
            true,
            &mut ctx,
        )
        .unwrap_err();
        assert!(
            matches!(err, PerpError::StaticRestrictionViolation),
            "got {err:?}"
        );
    }
}

// ── Batch place (Phase 2) ──────────────────────────────────────────────────
//
// Reuses the Phase 1 shell verbatim (pre-decode length validation, dynamic gas, the 34-byte
// index-aligned status blob, abort-forward). What is NEW and therefore what these tests hammer:
// the orderId/nonce rule (`save_order` has no collision guard, so a wrong rule is silent state
// corruption), the Accepted-vs-Filled tag split, the place-path reason codes, the 224-byte element
// stride, and the Phase 0 synergy that makes a batch safe at all — a rejected item emits no
// `OrderPlaced`.
mod batch_place {
    use super::batch_cancel::{
        decode_statuses, expect, lock_abort_counter, register_key, Status, SIGNED_RECV, SIGNED_TS,
    };
    use super::*;
    use crate::{
                batch::{
            self, PerpBatchReason, PerpBatchTag, BASE_BATCH_GAS, BATCH_STATUS_RECORD_LEN,
            MAX_BATCH_PLACE, PLACE_ITEM_ENCODED_LEN,
        },
        interface::IPerpDex::{
            batchPlaceOrdersCall, batchPlaceOrdersSignedCall, OrderPlaced, PlaceItem, Trade,
        },
        PLACE_ORDER_GAS,
        PerpError,
    };
    use ed25519_dalek::{Signer, SigningKey};

    const ZERO_ID: [u8; 32] = [0u8; 32];
    /// A market id that was never registered.
    const NO_MARKET: u64 = 999;
    /// Registered but `active: false`.
    const INACTIVE_MARKET: u64 = 2;

    // ── helpers ────────────────────────────────────────────────────────────

    fn item_in(
        market_id: u64,
        side: u8,
        price: u64,
        qty: u64,
        order_type: u8,
        tif: u8,
    ) -> PlaceItem {
        PlaceItem {
            marketId: market_id,
            side,
            price,
            quantity: qty,
            orderType: order_type,
            tif,
            clientOrderId: FixedBytes::default(),
        }
    }

    /// Limit GTC item on the default market.
    fn gtc(side: u8, price: u64, qty: u64) -> PlaceItem {
        item_in(MARKET_ID, side, price, qty, 0, 0)
    }

    fn direct_calldata(items: &[PlaceItem]) -> Vec<u8> {
        batchPlaceOrdersCall {
            orders: items.to_vec(),
        }
        .abi_encode()
    }

    /// Runs a batch through the real entry point and returns the raw statuses blob.
    fn batch_place(ctx: &mut TestCtx, caller: Address, items: &[PlaceItem]) -> Vec<u8> {
        let out = run_perp_dex_call(
            &direct_calldata(items),
            30_000_000,
            caller,
            U256::ZERO,
            false,
            ctx,
        )
        .expect("batch must not hard-fail");
        assert!(
            !out.reverted,
            "batch must return Ok once the loop has begun (reverted with {:?})",
            String::from_utf8_lossy(&out.bytes)
        );
        assert_eq!(
            out.gas_used,
            BASE_BATCH_GAS + items.len() as u64 * PLACE_ORDER_GAS,
            "the FULL dynamic cost must be what the output charges"
        );
        batchPlaceOrdersCall::abi_decode_returns(&out.bytes)
            .unwrap()
            .to_vec()
    }

    fn nonce(ctx: &mut TestCtx, user: Address) -> u64 {
        storage::load_user_nonce(ctx, user).unwrap()
    }

    /// The id item `k` of a direct batch must use, given the nonce the batch started from.
    fn direct_id(user: Address, base_nonce: u64, ids_consumed_before: u64) -> [u8; 32] {
        derive_order_id(user, base_nonce + ids_consumed_before)
    }

    fn register_inactive_market(ctx: &mut TestCtx) {
        let mut m = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
        m.market_id = INACTIVE_MARKET;
        m.active = false;
        storage::save_market(ctx, &m).unwrap();
    }

    fn order_placed_ids(ctx: &mut TestCtx) -> Vec<[u8; 32]> {
        JournalTr::take_logs(ctx.journal_mut())
            .into_iter()
            .filter(|l| l.data.topics().first() == Some(&OrderPlaced::SIGNATURE_HASH))
            .map(|l| {
                OrderPlaced::decode_raw_log(l.data.topics(), &l.data.data)
                    .unwrap()
                    .orderId
                    .0
            })
            .collect()
    }

    fn trade_count(ctx: &mut TestCtx) -> usize {
        JournalTr::take_logs(ctx.journal_mut())
            .into_iter()
            .filter(|l| l.data.topics().first() == Some(&Trade::SIGNATURE_HASH))
            .count()
    }

    // ── 1. happy path: rest + fill in one batch ────────────────────────────

    #[test]
    fn batch_place_mixes_resting_and_filled_and_is_index_aligned() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        // One resting ask for item 0 to sweep.
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let base = nonce(&mut ctx, ALICE);

        let items = [
            gtc(0, PRICE, QTY),            // crosses BOB's ask → fully filled
            gtc(0, PRICE - TICK, QTY),     // rests as a bid
            gtc(1, PRICE + 2 * TICK, QTY), // rests as an ask
        ];
        let blob = batch_place(&mut ctx, ALICE, &items);

        let (id0, id1, id2) = (
            direct_id(ALICE, base, 0),
            direct_id(ALICE, base, 1),
            direct_id(ALICE, base, 2),
        );
        assert_eq!(
            decode_statuses(&blob),
            vec![
                // Terminal ⇒ Filled: it swept the book and left no resting record.
                expect(PerpBatchTag::Filled, id0, PerpBatchReason::None),
                expect(PerpBatchTag::Accepted, id1, PerpBatchReason::None),
                expect(PerpBatchTag::Accepted, id2, PerpBatchReason::None),
            ]
        );
        // Tags agree with storage: Filled ⇒ deleted, Accepted ⇒ live.
        assert_terminal(&mut ctx, id0);
        assert!(storage::load_order(&mut ctx, &id1).unwrap().is_some());
        assert!(storage::load_order(&mut ctx, &id2).unwrap().is_some());
        // Three accepted items consumed exactly three ids.
        assert_eq!(nonce(&mut ctx, ALICE), base + 3);
        // One OrderPlaced per accepted item, in calldata order.
        assert_eq!(order_placed_ids(&mut ctx), vec![id0, id1, id2]);
    }

    /// A partial fill that rests its remainder is `PartiallyFilled` — NOT terminal, so tag 1.
    #[test]
    fn partially_filled_remainder_reports_the_resting_tag() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // only QTY on the ask
        let base = nonce(&mut ctx, ALICE);

        let blob = batch_place(&mut ctx, ALICE, &[gtc(0, PRICE, QTY * 2)]);

        let id0 = direct_id(ALICE, base, 0);
        assert_eq!(
            decode_statuses(&blob),
            vec![expect(PerpBatchTag::Accepted, id0, PerpBatchReason::None)]
        );
        let order = get_order(&mut ctx, id0);
        assert_eq!(order.filled, QTY);
        assert_eq!(order.status, OrderStatus::PartiallyFilled);
        assert_eq!(nonce(&mut ctx, ALICE), base + 1);
    }

    /// An IOC that matches nothing is ACCEPTED but terminal (Expired) — tag 2, not tag 1, and no
    /// resting record. Pins the "terminal ⇒ Filled" rule against the non-fill flavour.
    #[test]
    fn accepted_but_expired_ioc_reports_the_terminal_tag() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let base = nonce(&mut ctx, ALICE);

        let items = [item_in(MARKET_ID, 0, PRICE, QTY, 0, 1)]; // IOC, empty book
        let blob = batch_place(&mut ctx, ALICE, &items);

        let id0 = direct_id(ALICE, base, 0);
        assert_eq!(
            decode_statuses(&blob),
            vec![expect(PerpBatchTag::Filled, id0, PerpBatchReason::None)]
        );
        assert_terminal(&mut ctx, id0);
        // Accepted ⇒ the id was consumed, and OrderPlaced was emitted.
        assert_eq!(nonce(&mut ctx, ALICE), base + 1);
        assert_eq!(order_placed_ids(&mut ctx), vec![id0]);
    }

    // ── 2. THE id/nonce rule ───────────────────────────────────────────────

    /// A rejected item must consume NO id: the accepted items' ids stay gapless, and the nonce
    /// advances by exactly the accepted count. (A drifting nonce would let a later batch re-derive an
    /// id a resting order already holds, and `save_order` has no collision guard.)
    #[test]
    fn rejected_items_consume_no_id_so_ids_are_gapless() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let base = nonce(&mut ctx, ALICE);

        let items = [
            gtc(0, PRICE - TICK, QTY),
            item_in(NO_MARKET, 0, PRICE, QTY, 0, 0), // reject
            gtc(0, PRICE - 2 * TICK, QTY),
            item_in(MARKET_ID, 7, PRICE, QTY, 0, 0), // reject: invalid side
            gtc(0, PRICE - 3 * TICK, QTY),
        ];
        let blob = batch_place(&mut ctx, ALICE, &items);

        let (d0, d1, d2) = (
            direct_id(ALICE, base, 0),
            direct_id(ALICE, base, 1),
            direct_id(ALICE, base, 2),
        );
        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(PerpBatchTag::Accepted, d0, PerpBatchReason::None),
                // A rejected item has NO id — the field is zero, not an echo.
                expect(
                    PerpBatchTag::Rejected,
                    ZERO_ID,
                    PerpBatchReason::PlaceUnknownMarket
                ),
                expect(PerpBatchTag::Accepted, d1, PerpBatchReason::None),
                expect(
                    PerpBatchTag::Rejected,
                    ZERO_ID,
                    PerpBatchReason::InvalidSide
                ),
                expect(PerpBatchTag::Accepted, d2, PerpBatchReason::None),
            ]
        );
        for id in [d0, d1, d2] {
            assert!(
                storage::load_order(&mut ctx, &id).unwrap().is_some(),
                "every accepted item must rest under its reported id"
            );
        }
        assert_eq!(
            nonce(&mut ctx, ALICE),
            base + 3,
            "the nonce must advance by the ACCEPTED count (3), not by N (5)"
        );
        // …and the very next single-order placement continues the same chain.
        assert_eq!(place(&mut ctx, ALICE, 0, PRICE - 4 * TICK, QTY, 0, 0), {
            direct_id(ALICE, base, 3)
        });
    }

    /// Two successive direct batches from the same account must never produce a duplicate live id.
    #[test]
    fn two_successive_direct_batches_never_reuse_a_live_order_id() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let base = nonce(&mut ctx, ALICE);

        let first = batch_place(
            &mut ctx,
            ALICE,
            &[
                gtc(0, PRICE - TICK, QTY),
                gtc(0, PRICE - 2 * TICK, QTY),
                gtc(0, PRICE - 3 * TICK, QTY),
            ],
        );
        let second = batch_place(
            &mut ctx,
            ALICE,
            &[
                gtc(0, PRICE - 4 * TICK, QTY),
                gtc(0, PRICE - 5 * TICK, QTY),
                gtc(0, PRICE - 6 * TICK, QTY),
            ],
        );

        let mut ids: Vec<[u8; 32]> = decode_statuses(&first)
            .iter()
            .chain(decode_statuses(&second).iter())
            .map(|s| {
                assert_eq!(s.tag, PerpBatchTag::Accepted as u8);
                s.order_id
            })
            .collect();
        assert_eq!(ids.len(), 6);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(*id, direct_id(ALICE, base, i as u64), "id {i} off-chain");
            assert!(
                storage::load_order(&mut ctx, id).unwrap().is_some(),
                "all six orders must be live at once — a reused id would have overwritten one"
            );
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 6, "ids must be pairwise distinct");
        assert_eq!(nonce(&mut ctx, ALICE), base + 6);
    }

    /// An ALL-rejected batch must leave the nonce key untouched (no redundant same-value write, so
    /// the batch contributes zero keys to the block delta).
    #[test]
    fn all_rejected_batch_writes_nothing_at_all() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let base = nonce(&mut ctx, ALICE);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

        let blob = batch_place(
            &mut ctx,
            ALICE,
            &[
                item_in(NO_MARKET, 0, PRICE, QTY, 0, 0),
                item_in(MARKET_ID, 9, PRICE, QTY, 0, 0),
            ],
        );

        assert!(decode_statuses(&blob)
            .iter()
            .all(|s| s.tag == PerpBatchTag::Rejected as u8 && s.order_id == ZERO_ID));
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "an all-rejected batch must be write-clean, nonce included"
        );
        assert_eq!(nonce(&mut ctx, ALICE), base);
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
    }

    // ── 3. reject reason codes ─────────────────────────────────────────────

    /// Every place-path reject the engine can raise from one call, each mapped to its code — and the
    /// whole batch write-clean, which is what the driver's runtime classification keys off.
    #[test]
    fn every_place_reject_maps_to_its_code_and_is_write_clean() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        register_inactive_market(&mut ctx);
        // A resting ask at PRICE gives the PostOnly-cross and FOK-unfillable cases something real.
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

        let cases: [(PlaceItem, PerpBatchReason); 14] = [
            (
                item_in(NO_MARKET, 0, PRICE, QTY, 0, 0),
                PerpBatchReason::PlaceUnknownMarket,
            ),
            (
                item_in(INACTIVE_MARKET, 0, PRICE, QTY, 0, 0),
                PerpBatchReason::MarketNotActive,
            ),
            (
                item_in(MARKET_ID, 7, PRICE, QTY, 0, 0),
                PerpBatchReason::InvalidSide,
            ),
            (
                item_in(MARKET_ID, 0, PRICE, QTY, 9, 0),
                PerpBatchReason::InvalidOrderType,
            ),
            (
                item_in(MARKET_ID, 0, PRICE, QTY, 0, 9),
                PerpBatchReason::InvalidTif,
            ),
            (
                gtc(0, PRICE, QTY / 2),
                PerpBatchReason::QuantityBelowMinimum,
            ),
            (
                gtc(0, PRICE, QTY * 2_000),
                PerpBatchReason::QuantityAboveMaximum,
            ),
            (gtc(0, PRICE, QTY + 1), PerpBatchReason::QuantityStepSize),
            (gtc(0, 0, QTY), PerpBatchReason::PriceZero),
            (
                gtc(0, PRICE * 2_000, QTY),
                PerpBatchReason::PriceAboveMaximum,
            ),
            (gtc(0, PRICE + 1, QTY), PerpBatchReason::PriceTickSize),
            (
                item_in(MARKET_ID, 0, PRICE, QTY, 0, 3), // PostOnly, would cross BOB's ask
                PerpBatchReason::PostOnlyWouldMatch,
            ),
            (
                item_in(MARKET_ID, 0, PRICE - 4 * TICK, QTY, 0, 2), // FOK, nothing to fill against
                PerpBatchReason::FokUnfillable,
            ),
            (
                // Rests (below the ask, so no match) but its margin dwarfs the 10-USDC wallet.
                gtc(0, PRICE - 5 * TICK, QTY * 1_000),
                PerpBatchReason::InsufficientMargin,
            ),
        ];

        let items: Vec<PlaceItem> = cases.iter().map(|(i, _)| i.clone()).collect();
        let blob = batch_place(&mut ctx, ALICE, &items);

        assert_eq!(
            decode_statuses(&blob),
            cases
                .iter()
                .map(|(_, code)| expect(PerpBatchTag::Rejected, ZERO_ID, *code))
                .collect::<Vec<Status>>()
        );
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "every place-path genuine reject must be write-clean"
        );
        assert!(
            JournalTr::take_logs(ctx.journal_mut()).is_empty(),
            "and log-clean: a rejected placement emits no OrderPlaced (Phase 0)"
        );
    }

    /// The Phase 0 synergy that makes a batch safe: a rejected item emits NO `OrderPlaced`, even
    /// though the batch returns `Ok` and therefore never truncates the log stream.
    #[test]
    fn rejected_item_emits_no_order_placed_while_the_accepted_one_does() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let base = nonce(&mut ctx, ALICE);

        let blob = batch_place(
            &mut ctx,
            ALICE,
            &[
                item_in(MARKET_ID, 0, PRICE, QTY, 0, 3), // PostOnly cross → rejected
                gtc(0, PRICE - TICK, QTY),               // rests
            ],
        );

        let accepted = direct_id(ALICE, base, 0);
        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(
                    PerpBatchTag::Rejected,
                    ZERO_ID,
                    PerpBatchReason::PostOnlyWouldMatch
                ),
                expect(PerpBatchTag::Accepted, accepted, PerpBatchReason::None),
            ]
        );
        assert_eq!(
            order_placed_ids(&mut ctx),
            vec![accepted],
            "exactly one OrderPlaced, for the accepted item only"
        );
    }

    // ── 4. intra-batch determinism ─────────────────────────────────────────

    /// Item `i` rests, item `i+1` matches it: allowed, and identical to submitting the two as
    /// separate transactions in that order (there is no self-trade guard).
    #[test]
    fn intra_batch_self_cross_fills_the_earlier_item() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let base = nonce(&mut ctx, ALICE);

        let blob = batch_place(&mut ctx, ALICE, &[gtc(1, PRICE, QTY), gtc(0, PRICE, QTY)]);

        let (maker, taker) = (direct_id(ALICE, base, 0), direct_id(ALICE, base, 1));
        assert_eq!(
            decode_statuses(&blob),
            vec![
                // `statuses` is the outcome AT THE TIME the item ran: item 0 DID rest. Item 1 then
                // consumed it, so the maker is terminal by end-of-call — the logs are the authority.
                expect(PerpBatchTag::Accepted, maker, PerpBatchReason::None),
                expect(PerpBatchTag::Filled, taker, PerpBatchReason::None),
            ]
        );
        assert_terminal(&mut ctx, maker);
        assert_terminal(&mut ctx, taker);
        assert_eq!(nonce(&mut ctx, ALICE), base + 2);
        // Self-trade nets out exactly as the equivalent two single-order txs do.
        assert_eq!(wallet(&mut ctx, ALICE), WALLET);
        assert_eq!(
            pos(&mut ctx, ALICE),
            PerpPosition {
                leverage: 1,
                ..PerpPosition::default()
            }
        );
        assert_eq!(trade_count(&mut ctx), 1);
    }

    #[test]
    fn status_blob_is_byte_identical_across_runs() {
        let scenario = || {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
            batch_place(
                &mut ctx,
                ALICE,
                &[
                    gtc(0, PRICE, QTY),                      // fills
                    item_in(NO_MARKET, 0, PRICE, QTY, 0, 0), // rejected
                    gtc(0, PRICE - TICK, QTY),               // rests
                    item_in(MARKET_ID, 0, PRICE, QTY, 0, 3), // PostOnly, book now empty → rests
                ],
            )
        };
        let first = scenario();
        assert_eq!(first, scenario());
        assert_eq!(first, scenario());
        assert_eq!(first.len(), 4 * BATCH_STATUS_RECORD_LEN);
    }

    // ── 5. pre-loop faults revert the whole call ───────────────────────────

    #[test]
    fn empty_and_oversized_batches_revert_write_clean() {
        for items in [
            Vec::<PlaceItem>::new(),
            vec![gtc(0, PRICE - TICK, QTY); MAX_BATCH_PLACE + 1],
        ] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            let _ = JournalTr::take_logs(ctx.journal_mut());
            let writes_before = JournalTr::perp_write_count(ctx.journal_mut());

            let out = run_perp_dex_call(
                &direct_calldata(&items),
                u64::MAX, // never let gas be the reason
                ALICE,
                U256::ZERO,
                false,
                &mut ctx,
            )
            .unwrap();
            assert!(
                out.reverted,
                "N = {} must revert the whole call",
                items.len()
            );
            assert_eq!(
                JournalTr::perp_write_count(ctx.journal_mut()),
                writes_before,
                "a pre-loop fault must be write-clean"
            );
            assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
        }
    }

    /// N == MAX is accepted: 64 real resting orders, 64 distinct ids, one nonce advance of 64.
    #[test]
    fn max_batch_place_is_accepted() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        fund(&mut ctx, ALICE, WALLET * 20); // 64 rests at ~1 USDC of margin each
        let base = nonce(&mut ctx, ALICE);

        let items: Vec<PlaceItem> = (0..MAX_BATCH_PLACE as u64)
            .map(|i| gtc(0, PRICE - i * TICK, QTY))
            .collect();
        let blob = batch_place(&mut ctx, ALICE, &items);

        assert_eq!(blob.len(), MAX_BATCH_PLACE * BATCH_STATUS_RECORD_LEN);
        let mut ids: Vec<[u8; 32]> = decode_statuses(&blob)
            .iter()
            .map(|s| {
                assert_eq!(s.tag, PerpBatchTag::Accepted as u8, "{s:?}");
                s.order_id
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MAX_BATCH_PLACE);
        assert_eq!(nonce(&mut ctx, ALICE), base + MAX_BATCH_PLACE as u64);
    }

    // ── 6. gas ─────────────────────────────────────────────────────────────

    #[test]
    fn insufficient_gas_limit_is_out_of_gas_before_any_write() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
        let items = [gtc(0, PRICE - TICK, QTY), gtc(0, PRICE - 2 * TICK, QTY)];

        let full = BASE_BATCH_GAS + 2 * PLACE_ORDER_GAS;
        let err = run_perp_dex_call(
            &direct_calldata(&items),
            full - 1,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, PerpError::OutOfGas), "got {err:?}");
        assert_eq!(
            JournalTr::perp_write_count(ctx.journal_mut()),
            writes_before,
            "OutOfGas must precede every write"
        );
        assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());

        // Exactly the computed cost succeeds and charges exactly that.
        let out = run_perp_dex_call(
            &direct_calldata(&items),
            full,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(!out.reverted);
        assert_eq!(out.gas_used, full);
    }

    /// The 224-byte element stride must be what backs the pre-decode length bound. The decisive case
    /// is `stride confusion`: a declared length that a 32-byte-per-element bound would accept and a
    /// 224-byte-per-element bound must reject.
    #[test]
    fn hostile_length_word_reverts_without_allocating() {
        assert_eq!(PLACE_ITEM_ENCODED_LEN, 224);
        // selector || offset(0x20) || length || <body>
        let head = |len: u32, body: usize| {
            let mut v = vec![0u8; 4 + 64 + body];
            v[..4].copy_from_slice(&batchPlaceOrdersCall::SELECTOR);
            v[35] = 0x20;
            v[64..68].copy_from_slice(&len.to_be_bytes());
            v
        };

        let huge_u32 = head(u32::MAX, 0);
        let mut huge_u256 = head(1, 0);
        huge_u256[36..68].fill(0xff);
        let mut bad_offset = head(1, PLACE_ITEM_ENCODED_LEN);
        bad_offset[35] = 0x40;
        // 7 items declared, 7*32 bytes of body: enough for a bytes32[] of 7, 7× short for PlaceItem[].
        let stride_confusion = head(7, 7 * 32);
        // One item declared, one word short of its 224 bytes.
        let truncated = head(1, PLACE_ITEM_ENCODED_LEN - 32);

        for (name, input) in [
            ("u32::MAX length", huge_u32),
            ("u256 length", huge_u256),
            ("non-canonical offset", bad_offset),
            ("32-byte-stride body for a 224-byte item", stride_confusion),
            ("truncated final item", truncated),
        ] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
            let out =
                run_perp_dex_call(&input, u64::MAX, ALICE, U256::ZERO, false, &mut ctx).unwrap();
            assert!(out.reverted, "{name} must revert");
            assert_eq!(
                out.gas_used, BASE_BATCH_GAS,
                "{name}: an unreadable length word charges only the envelope floor"
            );
            assert_eq!(
                JournalTr::perp_write_count(ctx.journal_mut()),
                writes_before,
                "{name} must be write-clean"
            );
        }

        // A well-formed one-item call is exactly selector + offset + length + one 224-byte item, and
        // the pre-decode read agrees with the decoder.
        let good = direct_calldata(&[gtc(0, PRICE, QTY)]);
        assert_eq!(good.len(), 4 + 32 + 32 + PLACE_ITEM_ENCODED_LEN);
        assert_eq!(
            batch::PLACE_DIRECT_LAYOUT.checked_len(&good).unwrap(),
            1,
            "the pre-decode read must accept the canonical encoding"
        );
        assert_eq!(
            batch::batch_dynamic_gas(batchPlaceOrdersCall::SELECTOR, &good),
            Some(BASE_BATCH_GAS + PLACE_ORDER_GAS)
        );
    }

    #[test]
    fn gas_math_saturates_instead_of_wrapping() {
        assert_eq!(batch::batch_gas(usize::MAX, PLACE_ORDER_GAS), u64::MAX);
        assert_eq!(
            batch::batch_gas(MAX_BATCH_PLACE, PLACE_ORDER_GAS),
            BASE_BATCH_GAS + MAX_BATCH_PLACE as u64 * PLACE_ORDER_GAS
        );
        assert!(batch::batch_dynamic_gas(batchPlaceOrdersCall::SELECTOR, &[0u8; 8]).is_none());
    }

    // ── 7. signed batch ────────────────────────────────────────────────────

    fn sign(
        items: &[PlaceItem],
        sk: &SigningKey,
        account: Address,
        ts: u64,
        recv: u64,
    ) -> [u8; 64] {
        let msg = batch_place_message(account, 0, ts, recv, items);
        sk.sign(&msg).to_bytes()
    }

    fn signed_calldata(
        sk: &SigningKey,
        account: Address,
        timestamp: u64,
        recv_window: u64,
        items: &[PlaceItem],
        // Items actually put on the wire; `None` = same as the signed set.
        wire_items: Option<&[PlaceItem]>,
    ) -> Vec<u8> {
        batchPlaceOrdersSignedCall {
            account,
            keyId: 0,
            timestamp,
            recvWindow: recv_window,
            orders: wire_items.unwrap_or(items).to_vec(),
            signature: sign(items, sk, account, timestamp, recv_window)
                .to_vec()
                .into(),
        }
        .abi_encode()
    }

    /// `orderId[k] = keccak256(signature || u32BE(k))`.
    fn expected_signed_id(signature: &[u8; 64], k: u32) -> [u8; 32] {
        let mut buf = [0u8; 68];
        buf[..64].copy_from_slice(signature);
        buf[64..].copy_from_slice(&k.to_be_bytes());
        primitives::keccak256(buf).0
    }

    #[test]
    fn signed_batch_place_happy_path_with_index_distinct_ids() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register_key(&mut ctx, ALICE, &sk);
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let base = nonce(&mut ctx, ALICE);

        let items = [
            gtc(0, PRICE, QTY),        // fills against BOB
            gtc(0, PRICE - TICK, QTY), // rests
            gtc(1, PRICE + TICK, QTY), // rests
        ];
        let input = signed_calldata(&sk, ALICE, SIGNED_TS, SIGNED_RECV, &items, None);
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(!out.reverted, "{:?}", String::from_utf8_lossy(&out.bytes));
        assert_eq!(out.gas_used, BASE_BATCH_GAS + 3 * PLACE_ORDER_GAS);

        let sig = sign(&items, &sk, ALICE, SIGNED_TS, SIGNED_RECV);
        let ids = [
            expected_signed_id(&sig, 0),
            expected_signed_id(&sig, 1),
            expected_signed_id(&sig, 2),
        ];
        let blob = batchPlaceOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(PerpBatchTag::Filled, ids[0], PerpBatchReason::None),
                expect(PerpBatchTag::Accepted, ids[1], PerpBatchReason::None),
                expect(PerpBatchTag::Accepted, ids[2], PerpBatchReason::None),
            ]
        );
        // Pairwise distinct — the raw keccak256(signature) of the single-order path would have
        // handed all three the SAME id and silently overwritten two orders.
        let mut sorted = ids;
        sorted.sort_unstable();
        assert!(sorted.windows(2).all(|w| w[0] != w[1]));
        assert!(storage::load_order(&mut ctx, &ids[1]).unwrap().is_some());
        assert!(storage::load_order(&mut ctx, &ids[2]).unwrap().is_some());
        // The signed path does NOT touch the per-user nonce.
        assert_eq!(nonce(&mut ctx, ALICE), base);
        assert_eq!(order_placed_ids(&mut ctx), ids.to_vec());
    }

    #[test]
    fn signed_batch_rejects_tampered_content() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let a = gtc(0, PRICE - TICK, QTY);
        let b = gtc(0, PRICE - 2 * TICK, QTY);
        let c = gtc(0, PRICE - 3 * TICK, QTY);
        // Same item as `a` with one field changed.
        let a_price = gtc(0, PRICE - 5 * TICK, QTY);
        let a_qty = gtc(0, PRICE - TICK, QTY * 2);
        let mut a_cloid = a.clone();
        a_cloid.clientOrderId = FixedBytes([9u8; 16]);

        for (name, signed, wire) in [
            (
                "item price changed",
                vec![a.clone(), b.clone()],
                vec![a_price, b.clone()],
            ),
            (
                "item quantity changed",
                vec![a.clone(), b.clone()],
                vec![a_qty, b.clone()],
            ),
            (
                "clientOrderId changed",
                vec![a.clone(), b.clone()],
                vec![a_cloid, b.clone()],
            ),
            (
                "N grew",
                vec![a.clone(), b.clone()],
                vec![a.clone(), b.clone(), c.clone()],
            ),
            ("N shrank", vec![a.clone(), b.clone()], vec![a.clone()]),
            (
                "order permuted",
                vec![a.clone(), b.clone()],
                vec![b.clone(), a.clone()],
            ),
        ] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            register_key(&mut ctx, ALICE, &sk);
            let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
            // Drain setup's own events: account funding now emits AccountBalanceChanged at the
            // write site, so only the logs produced by the call below are under test here.
            let _ = JournalTr::take_logs(ctx.journal_mut());
            let input = signed_calldata(&sk, ALICE, SIGNED_TS, SIGNED_RECV, &signed, Some(&wire));
            let out =
                run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
            assert!(out.reverted, "{name} must fail verification");
            assert_eq!(
                JournalTr::perp_write_count(ctx.journal_mut()),
                writes_before,
                "{name}: signature failure is pre-write"
            );
            assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
        }
    }

    /// A replayed signed batch must REVERT rather than re-derive its (live) ids: the ids are a pure
    /// function of the signature, so without the burn the second run would `save_order` over the
    /// first run's resting orders.
    #[test]
    fn signed_batch_replay_reverts_instead_of_re_deriving_live_ids() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register_key(&mut ctx, ALICE, &sk);

        let items = [
            gtc(0, PRICE - TICK, QTY),
            item_in(NO_MARKET, 0, PRICE, QTY, 0, 0), // rejected → partial acceptance
        ];
        let input = signed_calldata(&sk, ALICE, SIGNED_TS, SIGNED_RECV, &items, None);
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(!out.reverted);
        let blob = batchPlaceOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
        let st = decode_statuses(&blob);
        assert_eq!(st[0].tag, PerpBatchTag::Accepted as u8);
        assert_eq!(st[1].tag, PerpBatchTag::Rejected as u8);
        let live = st[0].order_id;

        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted, "a replayed batch must revert");
        assert!(String::from_utf8_lossy(&out.bytes).contains("duplicate signature"));
        assert!(
            storage::load_order(&mut ctx, &live).unwrap().is_some(),
            "the first run's order must still be there, untouched"
        );
    }

    #[test]
    fn signed_batch_requires_key_window_and_expiry() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let items = [gtc(0, PRICE - TICK, QTY)];

        // No key registered.
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let input = signed_calldata(&sk, ALICE, SIGNED_TS, SIGNED_RECV, &items, None);
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted);
        assert!(String::from_utf8_lossy(&out.bytes).contains("no api key"));

        // Timestamp in the future.
        let mut ctx = make_ctx();
        setup(&mut ctx);
        register_key(&mut ctx, ALICE, &sk);
        let input = signed_calldata(&sk, ALICE, 10_000, SIGNED_RECV, &items, None);
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted);
        assert!(String::from_utf8_lossy(&out.bytes).contains("future"));
    }

    /// The digest is the TIGHT 43-bytes-per-item packing, not the 224-byte ABI encoding.
    #[test]
    fn signed_digest_layout_is_pinned() {
        let items = [gtc(0, PRICE, QTY), gtc(1, PRICE + TICK, QTY * 2)];
        let msg = batch_place_message(ALICE, 3, 111, 22, &items);
        assert_eq!(msg.len(), 22 + 20 + 1 + 8 + 8 + 4 + 2 * 43);
        assert_eq!(&msg[..22], b"perpdex_v1_batch_order");
        assert_eq!(&msg[22..42], ALICE.as_slice());
        assert_eq!(msg[42], 3);
        assert_eq!(&msg[43..51], &111u64.to_be_bytes());
        assert_eq!(&msg[51..59], &22u64.to_be_bytes());
        assert_eq!(&msg[59..63], &2u32.to_be_bytes());
        // First item, tightly packed.
        assert_eq!(&msg[63..71], &MARKET_ID.to_be_bytes());
        assert_eq!(msg[71], 0);
        assert_eq!(&msg[72..80], &PRICE.to_be_bytes());
        assert_eq!(&msg[80..88], &QTY.to_be_bytes());
        assert_eq!(msg[88], 0);
        assert_eq!(msg[89], 0);
        assert_eq!(&msg[90..106], &[0u8; 16]);
        // Second item starts right after.
        assert_eq!(&msg[106..114], &MARKET_ID.to_be_bytes());
        assert_eq!(msg[114], 1);
    }

    #[test]
    fn batch_place_selectors_are_not_static_callable() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let err = run_perp_dex_call(
            &direct_calldata(&[gtc(0, PRICE, QTY)]),
            30_000_000,
            ALICE,
            U256::ZERO,
            true,
            &mut ctx,
        )
        .unwrap_err();
        assert!(
            matches!(err, PerpError::StaticRestrictionViolation),
            "got {err:?}"
        );
    }

    // ── 8. abort-forward on the PLACE path ─────────────────────────────────

    /// Arms the one post-write error the place path still has: the fee recipient is cleared while the
    /// taker still owes a taker fee, so `credit_fee_recipient` — which runs in `finalize_apply`,
    /// AFTER `registry.flush` committed the fill — raises "fee recipient not initialised".
    ///
    /// ALICE pays a 1% taker fee; BOB (the maker) pays none, which is what keeps the walk's own
    /// admin check (`settle_maker_fill_registry`, only reached when `maker_fee > 0`) from catching it
    /// pre-flush. Leaves a resting ask at PRICE for the crossing item to consume.
    fn arm_post_write_place_abort(ctx: &mut TestCtx) {
        storage::save_user_fee_rates(
            ctx,
            ALICE,
            UserFeeRates {
                maker_fee_bps: 0,
                taker_fee_bps: 100,
            },
        )
        .unwrap();
        place(ctx, BOB, 1, PRICE, QTY, 0, 0);
        storage::save_admin(ctx, Address::ZERO).unwrap();
    }

    /// End-to-end abort-forward on `batchPlaceOrders` (the cancel-path analogue is
    /// `batch_cancel_aborts_forward_on_a_post_write_invariant`): the crossing item writes and THEN
    /// fails, so it is `Aborted`, the tail is `NotAttempted`, the call still returns `Ok`, the
    /// committed prefix keeps its order — and the nonce advances PAST the aborted item, so its id can
    /// never be re-derived (`save_order` has no collision guard). The aborted record reports that
    /// burned id, which is the only way a caller can learn which one was consumed.
    #[test]
    fn aborts_forward_on_a_post_write_place_error() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        arm_post_write_place_abort(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let base = nonce(&mut ctx, ALICE);
        let _abort_guard = lock_abort_counter();
        let aborts_before = batch::perp_batch_abort_count();

        let items = [
            gtc(0, PRICE - TICK, QTY),     // rests → the committed prefix
            gtc(0, PRICE, QTY),            // crosses BOB's ask → writes, then the fee reject
            gtc(0, PRICE - 2 * TICK, QTY), // never attempted
        ];
        // `batch_place` asserts the call did NOT revert.
        let blob = batch_place(&mut ctx, ALICE, &items);

        let (id0, id1) = (direct_id(ALICE, base, 0), direct_id(ALICE, base, 1));
        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(PerpBatchTag::Accepted, id0, PerpBatchReason::None),
                expect(
                    PerpBatchTag::Aborted,
                    id1,
                    PerpBatchReason::FeeRecipientNotSet
                ),
                expect(PerpBatchTag::NotAttempted, ZERO_ID, PerpBatchReason::None),
            ]
        );
        assert_eq!(batch::perp_batch_abort_count(), aborts_before + 1);
        assert!(
            storage::load_order(&mut ctx, &id0).unwrap().is_some(),
            "the committed prefix must survive the abort, with its order intact"
        );
        assert_eq!(
            nonce(&mut ctx, ALICE),
            base + 2,
            "consumed = 1 accepted + 1 aborted: the aborted id must be BURNED, not reusable"
        );
        // The next placement continues past the burned id rather than re-deriving it.
        let next = place(&mut ctx, ALICE, 0, PRICE - 5 * TICK, QTY, 0, 0);
        assert_ne!(next, id1, "an aborted item's id must never be re-derived");
        assert_eq!(next, direct_id(ALICE, base, 2));
    }

    /// A zero-fill GTC whose rest is unaffordable is a ROUTINE user reject, and must stay one: the
    /// match walk enters the crossing level (its only queued id is stale, so nothing fills) and
    /// records a `SaveLevel`, but that write must not commit — otherwise the driver's write-counter
    /// witness sees movement and mis-classifies the reject as an `Aborted`, dropping the rest of the
    /// batch. Third-party influenceable (anyone can leave a stale id at the victim's price), which is
    /// why it is pinned end-to-end.
    #[test]
    fn zero_fill_rest_reject_is_a_reject_not_an_abort() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let stale = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        storage::delete_order(&mut ctx, &stale).unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let base = nonce(&mut ctx, ALICE);
        let _abort_guard = lock_abort_counter();
        let aborts_before = batch::perp_batch_abort_count();

        let blob = batch_place(
            &mut ctx,
            ALICE,
            &[
                // Crosses the stale level, fills nothing, then wants 11 USDC of margin for the rest
                // out of a 10 USDC wallet.
                gtc(0, PRICE, QTY * 11),
                gtc(0, PRICE - TICK, QTY), // must still be attempted
            ],
        );

        let id1 = direct_id(ALICE, base, 0);
        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(
                    PerpBatchTag::Rejected,
                    ZERO_ID,
                    PerpBatchReason::InsufficientMargin
                ),
                expect(PerpBatchTag::Accepted, id1, PerpBatchReason::None),
            ]
        );
        assert_eq!(
            batch::perp_batch_abort_count(),
            aborts_before,
            "the reject is write-clean, so nothing may be counted as an abort"
        );
        assert_eq!(nonce(&mut ctx, ALICE), base + 1);
    }

    /// The nonce advance must be CHECKED: a saturating add would clamp at `u64::MAX` and leave the
    /// nonce on a value already spent, so the next placement would re-derive a live id.
    #[test]
    fn nonce_overflow_is_rejected_rather_than_clamped() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let err = commit_batch_order_nonce(&mut ctx, ALICE, u64::MAX, 1, None).unwrap_err();
        assert!(err.to_string().contains("order nonce overflow"), "{err}");
        assert_eq!(nonce(&mut ctx, ALICE), 0, "and nothing was written");
    }

    /// An over-cap batch reverts in `check_batch_len` having done ZERO work, so it must be billed the
    /// envelope floor only — never `BASE + N * unit` for work it never performs.
    #[test]
    fn over_cap_batch_is_billed_only_the_envelope_floor() {
        let over = direct_calldata(&vec![gtc(0, PRICE - TICK, QTY); MAX_BATCH_PLACE + 1]);
        assert_eq!(
            batch::batch_dynamic_gas(batchPlaceOrdersCall::SELECTOR, &over),
            None,
            "an over-cap declared length must not be billed per item"
        );

        let mut ctx = make_ctx();
        setup(&mut ctx);
        let out = run_perp_dex_call(&over, u64::MAX, ALICE, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted, "over-cap must still revert the whole call");
        assert!(String::from_utf8_lossy(&out.bytes).contains("batch too large"));
        assert_eq!(out.gas_used, BASE_BATCH_GAS);

        // N == MAX is billed in full, so the clamp cannot silently under-bill a legal batch.
        let at_cap = direct_calldata(&vec![gtc(0, PRICE - TICK, QTY); MAX_BATCH_PLACE]);
        assert_eq!(
            batch::batch_dynamic_gas(batchPlaceOrdersCall::SELECTOR, &at_cap),
            Some(BASE_BATCH_GAS + MAX_BATCH_PLACE as u64 * PLACE_ORDER_GAS)
        );
    }

    // ── 9. signed: header tampering + the burn under an abort ──────────────

    /// Every HEADER field is inside the digest, not just the items: `account`, `keyId`, `timestamp`
    /// and `recvWindow` (item fields / N / permutation live in `signed_batch_rejects_tampered_content`).
    /// A key is registered for BOTH accounts under BOTH key ids so each case fails on VERIFICATION and
    /// not for a boring reason like "no api key", and the tampered timestamp/recvWindow are chosen to
    /// still pass `check_recv_window` (skew 5s, window clamped to 60s).
    #[test]
    fn signed_batch_rejects_tampered_header_fields() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let items = [gtc(0, PRICE - TICK, QTY)];
        // Signed under ALICE / keyId 0 / SIGNED_TS / SIGNED_RECV.
        let sig = sign(&items, &sk, ALICE, SIGNED_TS, SIGNED_RECV);
        let wire = |account: Address, key_id: u8, ts: u64, recv: u64| {
            batchPlaceOrdersSignedCall {
                account,
                keyId: key_id,
                timestamp: ts,
                recvWindow: recv,
                orders: items.to_vec(),
                signature: sig.to_vec().into(),
            }
            .abi_encode()
        };

        for (name, input) in [
            ("account swapped", wire(BOB, 0, SIGNED_TS, SIGNED_RECV)),
            ("keyId changed", wire(ALICE, 1, SIGNED_TS, SIGNED_RECV)),
            (
                "timestamp changed",
                wire(ALICE, 0, SIGNED_TS + 1, SIGNED_RECV),
            ),
            (
                "recvWindow changed",
                wire(ALICE, 0, SIGNED_TS, SIGNED_RECV + 1),
            ),
        ] {
            let mut ctx = make_ctx();
            setup(&mut ctx);
            for user in [ALICE, BOB] {
                for key_id in [0u8, 1] {
                    storage::save_api_key(
                        &mut ctx,
                        user,
                        key_id,
                        ApiKey {
                            pubkey: sk.verifying_key().to_bytes(),
                            expiry: 0,
                        },
                    )
                    .unwrap();
                }
            }
            let writes_before = JournalTr::perp_write_count(ctx.journal_mut());
            // Drain setup's own events (funding emits AccountBalanceChanged at the write site) so
            // only the logs produced by the call below are under test.
            let _ = JournalTr::take_logs(ctx.journal_mut());
            let out =
                run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
            assert!(out.reverted, "{name} must fail verification");
            let reason = String::from_utf8_lossy(&out.bytes).to_string();
            assert!(
                reason.contains("signature verification failed"),
                "{name} must fail on the SIGNATURE, got {reason}"
            );
            assert_eq!(
                JournalTr::perp_write_count(ctx.journal_mut()),
                writes_before,
                "{name}: signature failure is pre-write"
            );
            assert!(JournalTr::take_logs(ctx.journal_mut()).is_empty());
        }
    }

    /// The signature is burned BEFORE the loop, unconditionally — so a batch that ABORTS mid-way
    /// (returning `Ok`, with committed writes) can never be replayed to re-run its tail.
    #[test]
    fn signed_batch_abort_still_burns_the_signature() {
        // Produces an abort (bumps the global counter) without asserting a delta — same
        // reason as `driver_classifies_by_write_count_not_by_message`: it must hold the lock.
        let _abort_guard = lock_abort_counter();
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        register_key(&mut ctx, ALICE, &sk);
        arm_post_write_place_abort(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let items = [
            gtc(0, PRICE, QTY),        // crosses BOB's ask → writes, then the fee reject
            gtc(0, PRICE - TICK, QTY), // never attempted
        ];
        let input = signed_calldata(&sk, ALICE, SIGNED_TS, SIGNED_RECV, &items, None);
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(!out.reverted, "an abort must still return Ok");
        let blob = batchPlaceOrdersSignedCall::abi_decode_returns(&out.bytes).unwrap();
        let st = decode_statuses(&blob);
        let sig = sign(&items, &sk, ALICE, SIGNED_TS, SIGNED_RECV);
        assert_eq!(
            st,
            vec![
                // The signed path reports the aborted item's derived id too.
                expect(
                    PerpBatchTag::Aborted,
                    expected_signed_id(&sig, 0),
                    PerpBatchReason::FeeRecipientNotSet
                ),
                expect(PerpBatchTag::NotAttempted, ZERO_ID, PerpBatchReason::None),
            ]
        );

        // Replay of the very same signature reverts the whole call.
        let out =
            run_perp_dex_call(&input, 30_000_000, CAROL, U256::ZERO, false, &mut ctx).unwrap();
        assert!(
            out.reverted,
            "an aborted batch must still burn its signature"
        );
        assert!(String::from_utf8_lossy(&out.bytes).contains("duplicate signature"));
    }

    // ── 10. batch single-user working-set equivalence (this PR) ─────────────
    //
    // The working-set relocates the initiator's account/position/order-lists into a batch-scoped
    // local, flushed to the main store ONCE at end-of-batch. These three tests pin the property
    // that makes it a pure perf refactor: the net state — and the off-trie perp delta / block
    // commitment — a batch produces is IDENTICAL to the same operations run as separate
    // single-order txs, including the abort-forward no-undo case and a same-initiator self-match
    // (maker == taker == the working-set owner), which is the split-copy double-spend risk.

    /// Places one order through the SAME entry point a batch uses (`run_perp_dex_call`), so a
    /// batch-vs-per-item comparison differs only in the batching, never in the entry path.
    fn single_place(ctx: &mut TestCtx, caller: Address, item: &PlaceItem) {
        let input = placeOrderCall {
            marketId: item.marketId,
            side: item.side,
            price: item.price,
            quantity: item.quantity,
            orderType: item.orderType,
            tif: item.tif,
            clientOrderId: item.clientOrderId,
        }
        .abi_encode();
        let out = run_perp_dex_call(&input, 30_000_000, caller, U256::ZERO, false, ctx)
            .expect("single place must not hard-fail");
        assert!(
            !out.reverted,
            "single place reverted: {}",
            String::from_utf8_lossy(&out.bytes)
        );
    }

    /// Full observable state the working-set hoists: position, visible + total perp collateral, and
    /// both order-entry lists.
    fn user_state(
        ctx: &mut TestCtx,
        user: Address,
    ) -> (
        PerpPosition,
        u64,
        Vec<crate::types::OrderEntry>,
        Vec<crate::types::OrderEntry>,
    ) {
        let p = storage::load_position(ctx, user, MARKET_ID).unwrap();
        let acct = storage::load_account(ctx, user).unwrap();
        let buy = storage::load_buy_orders(ctx, user, MARKET_ID).unwrap();
        let sell = storage::load_sell_orders(ctx, user, MARKET_ID).unwrap();
        (
            p,
            acct.visible_perp_wallet_balance(),
            Vec::from(buy),
            Vec::from(sell),
        )
    }

    /// The block's net off-trie perp delta folded into a commitment (drains the dirty set — call
    /// last). Equal commitments ⇒ byte-identical net write-set ⇒ golden-neutral.
    fn perp_commitment(ctx: &mut TestCtx) -> U256 {
        let delta = ctx.journal_mut().take_perp_delta();
        storage::compute_block_commitment(U256::ZERO, &delta)
    }

    /// A same-initiator self-match INSIDE one batch: item 0 rests ALICE's bid, item 1 is ALICE's
    /// crossing sell that consumes it. ALICE is BOTH the resting maker and the taker, so the
    /// working-set must serve ONE coherent copy to both sides of the match — a split ws/main copy
    /// would double-spend. The batch must net out to EXACTLY the two-separate-tx result.
    #[test]
    fn initiator_self_match_within_batch() {
        let items = [
            gtc(0, PRICE, QTY), // rests as ALICE's bid
            gtc(1, PRICE, QTY), // ALICE sells into her own bid → self-match fill
        ];

        // Batch path (one call, working-set active).
        let mut cb = make_ctx();
        setup(&mut cb);
        let base = nonce(&mut cb, ALICE);
        let blob = batch_place(&mut cb, ALICE, &items);
        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(
                    PerpBatchTag::Accepted,
                    direct_id(ALICE, base, 0),
                    PerpBatchReason::None
                ),
                expect(
                    PerpBatchTag::Filled,
                    direct_id(ALICE, base, 1),
                    PerpBatchReason::None
                ),
            ],
            "item 0 rested, item 1 self-matched it (fully filled)"
        );

        // Per-item path (same entry point, two separate txs).
        let mut cs = make_ctx();
        setup(&mut cs);
        assert_eq!(nonce(&mut cs, ALICE), base, "same starting nonce");
        for it in &items {
            single_place(&mut cs, ALICE, it);
        }

        // Single coherent copy: the batch state equals the two-tx state — no double-spend.
        assert_eq!(
            user_state(&mut cb, ALICE),
            user_state(&mut cs, ALICE),
            "self-match batch state must equal two separate single-order txs"
        );
        assert_eq!(nonce(&mut cb, ALICE), nonce(&mut cs, ALICE));
        // And the off-trie perp delta folds to the same commitment (golden-neutral).
        assert_eq!(
            perp_commitment(&mut cb),
            perp_commitment(&mut cs),
            "self-match batch perp delta must equal the two-tx delta"
        );
    }

    /// A post-write abort on the INITIATOR inside a batch (reusing `arm_post_write_place_abort`):
    /// the crossing item fills ALICE against BOB — writing ALICE's working-set position/account —
    /// and THEN the fee-recipient reject fires. It must be `Aborted`, the tail `NotAttempted`, the
    /// call still `Ok`, the committed prefix present, and — the working-set-specific angle — the
    /// aborted item's PARTIAL writes must be flushed and KEPT (commit-only has no undo: the ws is
    /// flushed on the abort path, never restored).
    #[test]
    fn batch_post_write_abort_still_caught() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        arm_post_write_place_abort(&mut ctx); // ALICE taker-fee 100bps, BOB rests ask @ PRICE, admin=ZERO
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let base = nonce(&mut ctx, ALICE);
        let _abort_guard = lock_abort_counter();
        let aborts_before = batch::perp_batch_abort_count();

        let items = [
            gtc(0, PRICE - TICK, QTY),     // item 0: rests → the committed prefix
            gtc(0, PRICE, QTY),            // item 1: crosses BOB, writes, then fee reject
            gtc(0, PRICE - 2 * TICK, QTY), // item 2: never attempted
        ];
        // `batch_place` asserts the call did NOT revert (abort-forward returns Ok).
        let blob = batch_place(&mut ctx, ALICE, &items);

        let (id0, id1) = (direct_id(ALICE, base, 0), direct_id(ALICE, base, 1));
        assert_eq!(
            decode_statuses(&blob),
            vec![
                expect(PerpBatchTag::Accepted, id0, PerpBatchReason::None),
                expect(
                    PerpBatchTag::Aborted,
                    id1,
                    PerpBatchReason::FeeRecipientNotSet
                ),
                expect(PerpBatchTag::NotAttempted, ZERO_ID, PerpBatchReason::None),
            ],
            "post-write error on the initiator → Aborted, tail NotAttempted"
        );
        assert_eq!(batch::perp_batch_abort_count(), aborts_before + 1);

        // Committed prefix (item 0's resting bid) survives with its order intact.
        assert!(
            storage::load_order(&mut ctx, &id0).unwrap().is_some(),
            "committed prefix order must survive the abort"
        );
        // The ABORTED item's partial writes are KEPT, not restored: item 1's fill opened ALICE +QTY
        // long in the working-set, which the abort path flushed to the main store.
        assert_eq!(
            pos(&mut ctx, ALICE).amount,
            QTY as i64,
            "the aborted item's partial fill must remain committed (flushed, not restored)"
        );
        assert_eq!(
            pos(&mut ctx, BOB).amount,
            -(QTY as i64),
            "BOB (non-initiator maker, written straight to main) took the other side"
        );
        // Nonce advanced past the burned aborted id (1 accepted + 1 aborted both consume an id).
        assert_eq!(nonce(&mut ctx, ALICE), base + 2);
    }

    /// A fully-accepted multi-item batch (one crossing a maker, two resting) must produce the
    /// identical final state — and the identical off-trie perp delta / commitment — as the same
    /// operations run as N single-order txs: the flush-once ≡ write-through equivalence.
    #[test]
    fn batch_all_accepted_flush_equals_per_item() {
        let items = [
            gtc(0, PRICE, QTY),            // crosses BOB's resting ask → fills
            gtc(0, PRICE - TICK, QTY),     // rests as a bid
            gtc(1, PRICE + 2 * TICK, QTY), // rests as an ask
        ];

        let mut cb = make_ctx();
        setup(&mut cb);
        place(&mut cb, BOB, 1, PRICE, QTY, 0, 0); // maker ALICE's item 0 sweeps
        let _ = JournalTr::take_logs(cb.journal_mut());
        batch_place(&mut cb, ALICE, &items);

        let mut cs = make_ctx();
        setup(&mut cs);
        place(&mut cs, BOB, 1, PRICE, QTY, 0, 0);
        let _ = JournalTr::take_logs(cs.journal_mut());
        for it in &items {
            single_place(&mut cs, ALICE, it);
        }

        // Both the initiator and the (non-hoisted) maker end identically.
        assert_eq!(
            user_state(&mut cb, ALICE),
            user_state(&mut cs, ALICE),
            "ALICE (initiator) state must match per-item"
        );
        assert_eq!(
            user_state(&mut cb, BOB),
            user_state(&mut cs, BOB),
            "BOB (maker) state must match per-item"
        );
        assert_eq!(nonce(&mut cb, ALICE), nonce(&mut cs, ALICE));
        // The whole net off-trie perp delta folds to the same commitment (golden-neutral).
        assert_eq!(
            perp_commitment(&mut cb),
            perp_commitment(&mut cs),
            "batch block commitment must equal the per-item commitment"
        );
    }
}

// ── Risk-reducing admission (B1 zero-cost debit + B3 characterisation) ──────
//
// Invariant under test: **a user must never be blocked from REDUCING risk.**
//
// B1 was the fix for `UserAccount::has_available_perp(0)` returning `false` on a negative wallet
// (`-5 >= 0`); its derived-basis restatement is `derived_can_afford`'s "a non-positive
// requirement is always affordable". B3 was a CHARACTERISATION of the audit claim "Binance
// ADMITS, we REJECT: a partly-closing sell on a long".
//
// ⚠️ REWRITTEN BY THE DERIVED-ooIM SWITCH. B3's whole subject was the ESCROW's answer, and the
// escrow is gone — so the audit claim it characterised is now largely RESOLVED rather than
// merely pinned, and two of the cases flip from REJECT to ACCEPT. Every case below states what
// moved and why. Two mechanical changes run through all of them:
//
//   * every test sets a LIVE MARK. `setup()` leaves `mark_price` at 0, which makes `N = 0` and
//     so blinds the derived basis to the position — under which NOTHING here is risk-reducing
//     and the module would be testing a fixture artefact. Production cannot reach a zero mark
//     (`addMarket` rejects it), and the Phase-1 census pinned this exact confusion.
//   * "fully deployed" now means AVAILABLE = 0, not wallet = 0: placing an order no longer
//     debits, so the pressure lives in `wallet − Σ ooIM`.
mod risk_reducing_admission {
    use super::*;

    /// $110 — one QTY lot is worth 1_100_000 here (calc_value(110e9, 1e6, 8, 9)).
    const P_HIGH: u64 = 110 * TICK;
    /// $120 — one QTY lot is worth 1_200_000.
    const P_HIGHER: u64 = 120 * TICK;
    /// $105 — one QTY lot is worth 1_050_000.
    const P_MID: u64 = 105 * TICK;
    /// $90 — one QTY lot is worth 900_000.
    const P_LOW: u64 = 90 * TICK;

    /// Raw SIGNED wallet balance. The `wallet()` helper reports
    /// `visible_perp_wallet_balance()`, which clamps negatives to 0 — useless here.
    fn raw_wallet(ctx: &mut TestCtx, user: Address) -> i64 {
        storage::load_account(ctx, user).unwrap().perp_wallet_balance
    }

    /// Overwrite the signed perp wallet directly (same pattern as the existing
    /// `alice.perp_wallet_balance = …` tests). A negative value is REACHABLE in
    /// production — a close-path fee or funding charge can drive it below zero.
    fn set_raw_wallet(ctx: &mut TestCtx, user: Address, v: i64) {
        let mut a = storage::load_account(ctx, user).unwrap();
        a.perp_wallet_balance = v;
        storage::save_account(ctx, user, a, AccountUpdateReason::Adjustment).unwrap();
    }

    /// Seed a flat long of `lots` QTY-sized lots opened at PRICE (margin at leverage 1).
    /// `setup()` plus a live mark at $100 — see the module header for why every test needs it.
    fn setup_marked(ctx: &mut TestCtx) {
        setup(ctx);
        set_mark(ctx, PRICE);
    }

    fn seed_long(ctx: &mut TestCtx, user: Address, lots: i64) {
        let notional = lots * FILL_VALUE as i64;
        storage::save_position(
            ctx,
            user,
            MARKET_ID,
            &PerpPosition {
                amount: lots * QTY as i64,
                v_quote_balance: -notional,
                margin: notional,
                leverage: 1,
                ..PerpPosition::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
    }

    fn try_place(
        ctx: &mut TestCtx,
        caller: Address,
        side: u8,
        price: u64,
        qty: u64,
        order_type: u8,
        tif: u8,
    ) -> Result<Bytes, PerpError> {
        let input = placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: order_type,
            tif,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode();
        run_place_order(&input, caller, ctx)
    }

    fn cancel(ctx: &mut TestCtx, caller: Address, id: [u8; 32]) -> Result<Bytes, PerpError> {
        let input = cancelOrderCall {
            orderId: id.into(),
            marketId: MARKET_ID,
        }
        .abi_encode();
        run_cancel_order(&input, caller, ctx)
    }

    // ── B1 ─────────────────────────────────────────────────────────────────

    /// REGRESSION (B1). Pinned bug: `has_available_perp(0)` evaluated `-5 >= 0`
    /// == `false`, so a NEGATIVE perp wallet refused a debit of ZERO. This
    /// placement reserves nothing (pure reduce ⇒ delta 0) yet was rejected with
    /// "insufficient perp wallet for margin" — locking a negative-balance user
    /// out of exactly the order that would REDUCE their risk.
    #[test]
    fn b1_negative_wallet_admits_a_zero_delta_reduce_only_rest() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        seed_long(&mut ctx, ALICE, 2);
        set_raw_wallet(&mut ctx, ALICE, -5);

        let ret = try_place(&mut ctx, ALICE, 1, P_HIGH, QTY, 0, 0)
            .expect("a zero-cost risk-reducing order must be admitted on a negative wallet");
        let id: [u8; 32] = ret[..32].try_into().unwrap();

        assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open);
        // N = +2e6 (2 lots at mark $100), Ask = 1.1e6 ⇒
        // IM = max(|2e6 + 0|, |2e6 − 1.1e6|) = 2e6 = PIM ⇒ ooIM = 0. Same answer the escrow gave.
        assert_eq!(
            oo_im(&mut ctx, ALICE),
            0,
            "a sell fully covered by the long adds no exposure"
        );
        assert_eq!(
            raw_wallet(&mut ctx, ALICE),
            -5,
            "a zero debit must leave the wallet untouched"
        );
    }

    /// REGRESSION (B1), the sharper half: the CLOSE path, which is gated by a
    /// `has_available_perp` site the audit's list of six does NOT contain —
    /// `finalize_compute` in `trading/settlement.rs`.
    ///
    /// A pure close with a zero taker fee has `total_required == 0`
    /// (`opening_margin + fee_from_wallet + mr_extra`, all zero). The gate is
    /// evaluated on the POST-fill working copy, so a shallow deficit is masked:
    /// the closing cashflow lifts the wallet positive before the check and the
    /// close goes through even pre-fix. The bug only bites when the deficit is
    /// DEEPER than the margin the close releases — which is exactly the user who
    /// most needs to de-risk. Here: wallet −2_000_000, a 1-lot long releasing
    /// 1_000_000, so the post-fill wallet is still −1_000_000 and
    /// `has_available_perp(0)` returned false. Pre-fix that dropped into the
    /// wallet-cover branch, found no same-side order to cancel, and rejected the
    /// close with "insufficient perp wallet for margin" — the deepest-underwater
    /// user was the one locked out of closing.
    #[test]
    fn b1_deeply_negative_wallet_user_can_still_close_a_position() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        seed_long(&mut ctx, ALICE, 1);
        place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0); // BOB rests a bid at $100
                                                   // Deficit deeper than the 1_000_000 the close will release.
        set_raw_wallet(&mut ctx, ALICE, -2_000_000);

        try_place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 1)
            .expect("closing a position must never be blocked by a negative wallet");

        assert_eq!(pos(&mut ctx, ALICE).amount, 0, "position closed");
        assert_eq!(
            raw_wallet(&mut ctx, ALICE),
            -2_000_000 + INIT_MARGIN as i64,
            "the released position margin reduces the deficit; still negative, still allowed"
        );
    }

    /// REGRESSION (B1) on the audit's missed site, and the ugliest shape of it.
    /// `finalize_compute`'s cover branch runs a SIMULATED LIFO cancel loop,
    /// `while !sim_account.has_available_perp(core.total_required)`, which a
    /// negative wallet entered even at `total_required == 0`. Here the resting
    /// sell is fully covered by the long, so it reserves NOTHING: cancelling it
    /// in-sim credits 0, the wallet stays negative, the list is exhausted and the
    /// close was REJECTED — the user was told to liquidate orders that could not
    /// possibly help. (Had the order reserved something the sim could instead
    /// have "succeeded" while the apply half, guarded by
    /// `ensure_taker_wallet_can_cover_margin`'s own `required_margin == 0` early
    /// return, performed no real cancel — the two halves disagreeing.)
    /// Post-fix both halves take the zero fast path: the close is admitted and
    /// the resting order is untouched. (Under the derived basis the cover loop's
    /// guard is `derived_can_afford(available, 0)`, which is unconditionally true —
    /// the same fast path, restated.)
    #[test]
    fn b1_zero_cost_close_does_not_disturb_resting_same_side_orders() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        seed_long(&mut ctx, ALICE, 2);
        // A resting sell fully covered by the long: costs nothing, and is the
        // LIFO victim the sim cover loop would have reached for.
        let resting = place(&mut ctx, ALICE, 1, P_HIGHER, QTY, 0, 0);
        assert_eq!(oo_im(&mut ctx, ALICE), 0);
        place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0); // BOB rests a bid at $100
        set_raw_wallet(&mut ctx, ALICE, -5_000_000); // deeper than anything released

        try_place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 1).expect("zero-cost close admitted");

        assert_eq!(
            pos(&mut ctx, ALICE).amount,
            QTY as i64,
            "1 of 2 lots closed"
        );
        assert_eq!(
            get_order(&mut ctx, resting).status,
            OrderStatus::Open,
            "the resting same-side order must survive a zero-cost close"
        );
    }

    /// Companion to the above: the SHALLOW-deficit close was already fine pre-fix
    /// (the closing cashflow lifts the working copy positive before the gate).
    /// Pinned so the two cases are not conflated.
    #[test]
    fn b1_shallow_negative_wallet_close_was_already_admitted() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        seed_long(&mut ctx, ALICE, 1);
        place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0);
        set_raw_wallet(&mut ctx, ALICE, -5);

        try_place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 1).expect("close admitted");

        assert_eq!(pos(&mut ctx, ALICE).amount, 0);
        assert_eq!(raw_wallet(&mut ctx, ALICE), INIT_MARGIN as i64 - 5);
    }

    /// B1 companion: the cancel path has NO balance gate by design. Pinning that
    /// so nobody "helpfully" adds one.
    ///
    /// CHANGED BY THE ESCROW REMOVAL: a cancel no longer CREDITS the wallet (it used to return
    /// the 900_000 reservation, so the wallet went `-5` → `899_995`). It moves no money at all;
    /// what it returns is HEADROOM, by dropping this market's `Bid` to 0 and with it the
    /// requirement. Both halves are asserted.
    #[test]
    fn b1_negative_wallet_user_can_still_cancel() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        let id = place(&mut ctx, ALICE, 0, P_LOW, QTY, 0, 0);
        assert_eq!(oo_im(&mut ctx, ALICE), 900_000);
        set_raw_wallet(&mut ctx, ALICE, -5);
        assert_eq!(super::available(&mut ctx, ALICE), -900_005);

        cancel(&mut ctx, ALICE, id).expect("cancel is ungated and must stay ungated");

        assert_eq!(oo_im(&mut ctx, ALICE), 0);
        assert_eq!(
            raw_wallet(&mut ctx, ALICE),
            -5,
            "the wallet does not move — there was never anything escrowed to give back"
        );
        assert_eq!(
            super::available(&mut ctx, ALICE),
            -5,
            "but the 900_000 of headroom the order was holding is released"
        );
    }

    /// B1 must NOT open a funding hole: a genuinely margin-requiring order is
    /// still refused on a negative wallet, and the ≥ boundary is unchanged.
    #[test]
    fn b1_negative_wallet_still_refuses_a_nonzero_debit() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        set_raw_wallet(&mut ctx, ALICE, -5);

        let err = try_place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0).unwrap_err();
        assert!(
            err.to_string()
                .contains("insufficient perp wallet for margin"),
            "{err}"
        );
        assert_eq!(oo_im(&mut ctx, ALICE), 0, "reject wrote nothing");

        // Boundary is untouched: one unit short still fails, exactly enough passes.
        // (Flat position ⇒ N = 0 ⇒ the requirement is the full 1e6 notional at leverage 1.)
        set_raw_wallet(&mut ctx, ALICE, INIT_MARGIN as i64 - 1);
        assert!(try_place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0).is_err());
        set_raw_wallet(&mut ctx, ALICE, INIT_MARGIN as i64);
        try_place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0).expect("exactly enough must pass");
        // CHANGED: the wallet is no longer debited, so it is the AVAILABLE that lands on 0.
        assert_eq!(raw_wallet(&mut ctx, ALICE), INIT_MARGIN as i64);
        assert_eq!(super::available(&mut ctx, ALICE), 0);
    }

    // ── B3 ① pure reduce ───────────────────────────────────────────────────

    /// B3 ①: a sell against a long adds no EXPOSURE, so it adds no requirement. At mark $100 with
    /// a 2-lot long, `N = 2e6`; one lot sold at $110 gives `Ask = 1.1e6` and
    /// `IM = max(|2e6|, |2e6 − 1.1e6|) = 2e6 = PIM ⇒ ooIM = 0`; the second lot at $120 takes
    /// `Ask` to 2.3e6 and `IM = max(2e6, 3e5)` is still 2e6 ⇒ ooIM still 0.
    /// Admitted at a POSITIVE, ZERO and NEGATIVE wallet alike.
    ///
    /// VERDICT UNCHANGED by the migration — the escrow reached 0 here too (by a different route:
    /// its per-side cover scan found every entry covered). ① was already fine.
    #[test]
    fn b3_case1_pure_reduce_is_zero_delta_at_positive_zero_and_negative_balance() {
        for balance in [WALLET as i64, 0i64, -5i64] {
            let mut ctx = make_ctx();
            setup_marked(&mut ctx);
            seed_long(&mut ctx, ALICE, 2);
            set_raw_wallet(&mut ctx, ALICE, balance);

            // Partial reduce: 1 lot against a 2-lot long.
            try_place(&mut ctx, ALICE, 1, P_HIGH, QTY, 0, 0)
                .unwrap_or_else(|e| panic!("partial reduce refused at balance {balance}: {e}"));
            assert_eq!(oo_im(&mut ctx, ALICE), 0);
            assert_eq!(raw_wallet(&mut ctx, ALICE), balance, "delta == 0");

            // Full reduce: the second lot, still exactly covered by the long.
            try_place(&mut ctx, ALICE, 1, P_HIGHER, QTY, 0, 0)
                .unwrap_or_else(|e| panic!("full reduce refused at balance {balance}: {e}"));
            let p = pos(&mut ctx, ALICE);
            assert_eq!(p.total_sell_qty, QTY * 2);
            assert_eq!(
                oo_im(&mut ctx, ALICE),
                0,
                "aggregate sells still net inside the long"
            );
            assert_eq!(raw_wallet(&mut ctx, ALICE), balance, "delta == 0");
        }
    }

    // ── B3 ② flip ──────────────────────────────────────────────────────────

    /// B3 ②: a sell LARGER than the long genuinely opens a short, so it costs something —
    /// but far less than the escrow charged.
    ///
    /// CHANGED BY THE DERIVED-ooIM SWITCH: requirement **1_100_000 → 200_000**. Long 1 lot at
    /// mark $100 (`N = 1e6`), sell 2 lots at $110 (`Ask = 2.2e6`):
    ///   `IM  = ROUND_UP(max(|1e6 + 0|, |1e6 − 2.2e6|) / 1) = 1.2e6`
    ///   `PIM = 1e6`  ⇒  `ooIM = 200_000`
    /// The escrow charged the excess lot's whole opening notional (1 lot @ $110 = 1_100_000) and
    /// netted nothing, because it priced the two sides separately in QUANTITY space. Binance's
    /// joint `max()` measures the exposure that would REMAIN after the flip — a 1.2e6 short
    /// against a 1e6 long already margined — so it charges only the 200_000 of extra exposure.
    /// The old comment called crediting the released position margin "A3, a separate product
    /// decision"; adopting Binance's formula settles it.
    ///
    /// The SHAPE is unchanged: fully deployed ⇒ refused; one unit short ⇒ refused; exactly the
    /// requirement ⇒ admitted. Only the threshold moved.
    #[test]
    fn b3_case2_flip_is_charged_only_the_residual_exposure_not_the_whole_opening_leg() {
        // Fully deployed ⇒ refused.
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        seed_long(&mut ctx, ALICE, 1);
        set_raw_wallet(&mut ctx, ALICE, 0);
        let err = try_place(&mut ctx, ALICE, 1, P_HIGH, QTY * 2, 0, 0).unwrap_err();
        assert!(
            err.to_string()
                .contains("insufficient perp wallet for margin"),
            "{err}"
        );
        assert_eq!(oo_im(&mut ctx, ALICE), 0, "reject wrote nothing");

        // One unit short of the requirement: still refused.
        set_raw_wallet(&mut ctx, ALICE, 200_000 - 1);
        assert!(try_place(&mut ctx, ALICE, 1, P_HIGH, QTY * 2, 0, 0).is_err());

        // Exactly the residual exposure is admitted.
        set_raw_wallet(&mut ctx, ALICE, 200_000);
        try_place(&mut ctx, ALICE, 1, P_HIGH, QTY * 2, 0, 0)
            .expect("admitted at exactly the derived requirement");
        let p = pos(&mut ctx, ALICE);
        assert_eq!(p.total_sell_notional, 2_200_000, "Ask");
        assert_eq!(oo_im(&mut ctx, ALICE), 200_000);
        assert_eq!(raw_wallet(&mut ctx, ALICE), 200_000, "nothing was debited");
        assert_eq!(super::available(&mut ctx, ALICE), 0);
        assert_eq!(
            p.margin, 1_000_000,
            "the long's own margin is untouched — it is NETTED in the requirement, not moved"
        );
    }

    // ── B3 ③ reduce with other resting orders ──────────────────────────────

    /// B3 ③a: an OPPOSITE-side resting order does NOT change the answer.
    ///
    /// Long 2 lots (`N = 2e6`) with a buy of 1 lot @ $90 resting (`Bid = 900_000`):
    /// `IM = max(|2e6 + 9e5|, |2e6|) = 2.9e6`, `PIM = 2e6` ⇒ `ooIM = 900_000`, the same number
    /// the escrow held. Adding a reducing sell of 1 lot @ $110 (`Ask = 1.1e6`) leaves the BID
    /// branch winning at 2.9e6, so `ooIM` is unchanged ⇒ Δ = 0 and it is free.
    ///
    /// VERDICT UNCHANGED by the migration. "Fully deployed" is now expressed as AVAILABLE = 0.
    #[test]
    fn b3_case3a_opposite_side_resting_order_does_not_block_a_reduce() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        seed_long(&mut ctx, ALICE, 2);

        // Every buy OPENS on top of a long, so this costs its full notional.
        place(&mut ctx, ALICE, 0, P_LOW, QTY, 0, 0);
        assert_eq!(oo_im(&mut ctx, ALICE), 900_000);

        set_available(&mut ctx, ALICE, 0); // fully deployed

        try_place(&mut ctx, ALICE, 1, P_HIGH, QTY, 0, 0)
            .expect("a reduce must be admitted despite a resting opposite-side order");
        assert_eq!(
            oo_im(&mut ctx, ALICE),
            900_000,
            "UNCHANGED — the bid branch still wins"
        );
        assert_eq!(super::available(&mut ctx, ALICE), 0, "delta == 0");
    }

    /// B3 ③b: **FLIPS FROM REJECT TO ACCEPT.** This is the audit claim ("Binance ADMITS, we
    /// REJECT: a partly-closing sell on a long") resolved rather than characterised.
    ///
    /// Long 2 lots (`N = 2e6`). A first sell of 2 lots @ $110 exactly covers it; a second sell of
    /// 1 lot @ $120 takes the AGGREGATE to 3 lots against a 2-lot long — a genuine 1-lot short in
    /// QUANTITY space, which is what the escrow charged for (1_200_000, the excess lot at the
    /// dearest price). In NOTIONAL space at mark it is not:
    ///   `Ask = 2.2e6 + 1.2e6 = 3.4e6`, `IM = max(|2e6|, |2e6 − 3.4e6|) = max(2e6, 1.4e6) = 2e6`
    ///   `= PIM` ⇒ `ooIM = 0`.
    /// If every sell filled, the account would hold 1.4e6 of SHORT exposure — strictly less than
    /// the 2e6 of LONG exposure it is already margined for. No additional initial margin is
    /// required, and Binance charges none.
    ///
    /// EXTRA SCRUTINY (this admits an order we used to refuse): the loosening is bounded and
    /// guarded. `pos.margin` still fully backs the existing 2-lot long; the sells' own opening
    /// margin is charged at FILL time out of the wallet (which is allowed to go negative if it
    /// cannot cover it — Binance never sweeps an under-covered lien); and K9 still refuses any fill
    /// that would leave the resulting position below maintenance. What is no longer charged is a
    /// worst-case exposure SMALLER than the one already margined — which was never a risk.
    #[test]
    fn b3_case3b_same_side_resting_orders_that_stay_inside_the_long_are_free() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx);
        seed_long(&mut ctx, ALICE, 2);

        // First sell exactly covers the long ⇒ costs nothing.
        place(&mut ctx, ALICE, 1, P_HIGH, QTY * 2, 0, 0);
        assert_eq!(oo_im(&mut ctx, ALICE), 0);

        set_available(&mut ctx, ALICE, 0); // fully deployed

        // The escrow REFUSED this (it wanted 1_200_000). The derived basis admits it for free.
        try_place(&mut ctx, ALICE, 1, P_HIGHER, QTY, 0, 0)
            .expect("aggregate short exposure 1.4e6 < the 2e6 long already margined");
        let p = pos(&mut ctx, ALICE);
        assert_eq!(p.total_sell_qty, QTY * 3);
        assert_eq!(p.total_sell_notional, 3_400_000, "Ask");
        assert_eq!(oo_im(&mut ctx, ALICE), 0);
        assert_eq!(super::available(&mut ctx, ALICE), 0, "nothing was charged");

        // The boundary is real, not vacuous: one more lot at $120 takes Ask to 4.6e6, the ask
        // branch overtakes (|2e6 − 4.6e6| = 2.6e6 > 2e6) and the excess IS charged.
        let err = try_place(&mut ctx, ALICE, 1, P_HIGHER, QTY, 0, 0).unwrap_err();
        assert!(
            err.to_string()
                .contains("insufficient perp wallet for margin"),
            "{err}"
        );
        let w = raw_wallet(&mut ctx, ALICE);
        set_raw_wallet(&mut ctx, ALICE, w + 600_000);
        try_place(&mut ctx, ALICE, 1, P_HIGHER, QTY, 0, 0)
            .expect("2.6e6 − 2e6 = 600_000 is exactly the marginal requirement");
        assert_eq!(oo_im(&mut ctx, ALICE), 600_000);
        assert_eq!(super::available(&mut ctx, ALICE), 0);
    }

    /// B3 ③c: **THE PHENOMENON THIS TEST PINNED NO LONGER EXISTS.**
    ///
    /// It used to record that once the aggregate over-covers, the sell side is scanned ASC, so
    /// the CHEAPEST sells absorb the position's cover and the dearest are pushed into the opening
    /// bucket — meaning a reduce-only order could be charged at ANOTHER order's price, and more
    /// than its own notional was worth. That was an artefact of a per-order cover scan.
    ///
    /// `ooIM` has no such scan: it is a function of `(N, Bid, Ask, L)` alone, and `Bid`/`Ask` are
    /// plain sums. So the requirement cannot depend on which order is "the new one", and cannot
    /// depend on the ORDER in which two orders were placed. That is what this test now pins —
    /// the property that replaced the anomaly.
    #[test]
    fn b3_case3c_the_requirement_is_order_independent_not_priced_at_a_particular_order() {
        // Place the $110 order first, then the $105 one ...
        let mut a = make_ctx();
        setup_marked(&mut a);
        seed_long(&mut a, ALICE, 2);
        place(&mut a, ALICE, 1, P_HIGH, QTY * 2, 0, 0); // 2 lots @ $110
        assert_eq!(oo_im(&mut a, ALICE), 0);
        try_place(&mut a, ALICE, 1, P_MID, QTY, 0, 0).expect("admitted");

        // ... and the other way round.
        let mut b = make_ctx();
        setup_marked(&mut b);
        seed_long(&mut b, ALICE, 2);
        place(&mut b, ALICE, 1, P_MID, QTY, 0, 0); // 1 lot @ $105
        try_place(&mut b, ALICE, 1, P_HIGH, QTY * 2, 0, 0).expect("admitted");

        // Same book, same requirement, whichever order was "the new one".
        assert_eq!(pos(&mut a, ALICE).total_sell_notional, 3_250_000);
        assert_eq!(
            pos(&mut b, ALICE).total_sell_notional,
            pos(&mut a, ALICE).total_sell_notional
        );
        assert_eq!(oo_im(&mut b, ALICE), oo_im(&mut a, ALICE));
        // And the value: Ask = 2.2e6 + 1.05e6 = 3.25e6, |2e6 − 3.25e6| = 1.25e6 < |N| = 2e6,
        // so the bid branch still wins and NOTHING is charged. Under the escrow this book cost
        // 1_100_000 — "charged at $110, more than the $105 order's own notional".
        assert_eq!(oo_im(&mut a, ALICE), 0);
    }

    // ── The admission gate's case table, all four rows explicitly ──────────

    /// `(Δ ooIM, available(after))` for the hypothetical "rest `qty` at `price` on `side`" — the
    /// two quantities `rest_in_book`'s gate is a function of, computed here the way the gate
    /// computes them (`margin_view`, same helpers, same overrides) so the table below indexes the
    /// real inputs and not a paraphrase.
    fn gate_inputs(
        ctx: &mut TestCtx,
        user: Address,
        side: u8,
        price: u64,
        qty: u64,
    ) -> (i128, i128) {
        let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
        let before = storage::load_position(ctx, user, MARKET_ID).unwrap();
        let mut after = before.clone();
        // A buy's Assuming Price IS its limit price; a sell's is `max(T, limit)`, frozen now.
        let assuming = if side == 0 {
            price
        } else {
            price.max(crate::margin_view::assuming_price_floor(ctx, MARKET_ID, &market).unwrap())
        };
        let notional =
            crate::math::calc_value(assuming, qty, market.base_decimals, market.price_decimals)
                .unwrap();
        if side == 0 {
            after.total_buy_qty += qty;
            after.total_buy_notional += notional;
        } else {
            after.total_sell_qty += qty;
            after.total_sell_notional += notional;
        }
        let delta =
            crate::margin_view::derived_requirement_delta(&market, &before, &after).unwrap();
        let available_after = crate::margin_view::derived_available_balance_with(
            ctx,
            user,
            None,
            Some((MARKET_ID, &after)),
        )
        .unwrap();
        (delta, available_after)
    }

    /// **The full case table `rest_in_book`'s admission gate is defined over**, one row per cell,
    /// each row pinned by asserting BOTH gate inputs and then the verdict:
    ///
    /// ```text
    ///   Δ ooIM │ available(after) │ verdict
    ///   ───────┼──────────────────┼─────────
    ///     ≤ 0  │       ≥ 0        │ ACCEPT
    ///     ≤ 0  │       < 0        │ ACCEPT   ← the B1 escape; the ONLY row that needs Δ
    ///     > 0  │       ≥ 0        │ ACCEPT
    ///     > 0  │       < 0        │ REJECT
    /// ```
    ///
    /// The gate evaluates `available(after) ≥ 0` first and consults `Δ ooIM`'s SIGN only on the
    /// negative branch, so rows 1/3 must be indistinguishable to it (they are: both accept without
    /// the delta) and rows 2/4 must be separated by the sign alone. Delete the `Δ > 0` guard and
    /// row 2 flips to REJECT — that is the mutation this test exists to catch.
    ///
    /// Every row rests via PostOnly, so no matching runs and the gate under test is exactly
    /// `rest_in_book`'s (the taker path has its own, in `settlement.rs`).
    ///
    /// Note `Δ ooIM < 0` is unreachable HERE — `max(|N + Bid|, |N − Ask|)` is non-decreasing in
    /// each aggregate at fixed `N` — so the `≤ 0` rows are `Δ == 0`, asserted as such.
    #[test]
    fn admission_gate_case_table_over_delta_sign_and_post_availability() {
        const POST_ONLY: u8 = 3;

        // ── Row 1: Δ == 0, available(after) ≥ 0 ⇒ ACCEPT ──
        // A 2-lot long at mark $100 (N = 2e6) and a 1-lot sell at $110 (Ask = 1.1e6):
        // IM = max(2e6, |2e6 − 1.1e6|) = 2e6 = PIM ⇒ ooIM unchanged. Wallet is the funded 10e6.
        {
            let mut ctx = make_ctx();
            setup_marked(&mut ctx);
            seed_long(&mut ctx, ALICE, 2);
            assert_eq!(
                gate_inputs(&mut ctx, ALICE, 1, P_HIGH, QTY),
                (0, 10_000_000),
                "row 1 inputs: zero delta, positive post-availability"
            );
            try_place(&mut ctx, ALICE, 1, P_HIGH, QTY, 0, POST_ONLY).expect("row 1 must ACCEPT");
            assert_eq!(oo_im(&mut ctx, ALICE), 0);
        }

        // ── Row 2: Δ == 0, available(after) < 0 ⇒ ACCEPT (the B1 escape) ──
        // Same risk-reducing order, but the mark has already left the account under-covered
        // (modelled by the negative wallet, which is what a close-path fee or funding charge
        // produces). The order costs nothing yet the post-availability is still negative.
        {
            let mut ctx = make_ctx();
            setup_marked(&mut ctx);
            seed_long(&mut ctx, ALICE, 2);
            set_raw_wallet(&mut ctx, ALICE, -5);
            assert_eq!(
                gate_inputs(&mut ctx, ALICE, 1, P_HIGH, QTY),
                (0, -5),
                "row 2 inputs: zero delta, NEGATIVE post-availability"
            );
            try_place(&mut ctx, ALICE, 1, P_HIGH, QTY, 0, POST_ONLY).expect(
                "row 2 must ACCEPT — a risk-reducing order is never gated on the balance (B1)",
            );
            assert_eq!(oo_im(&mut ctx, ALICE), 0);
            assert_eq!(raw_wallet(&mut ctx, ALICE), -5, "and moves no money");
        }

        // ── Row 3: Δ > 0, available(after) ≥ 0 ⇒ ACCEPT ──
        // Flat position (N = 0), so a 1-lot buy at $100 costs its full notional at leverage 1.
        // 10e6 wallet − 1e6 requirement leaves 9e6.
        {
            let mut ctx = make_ctx();
            setup_marked(&mut ctx);
            assert_eq!(
                gate_inputs(&mut ctx, ALICE, 0, PRICE, QTY),
                (INIT_MARGIN as i128, 9_000_000),
                "row 3 inputs: positive delta, positive post-availability"
            );
            try_place(&mut ctx, ALICE, 0, PRICE, QTY, 0, POST_ONLY).expect("row 3 must ACCEPT");
            assert_eq!(oo_im(&mut ctx, ALICE), INIT_MARGIN);
            assert_eq!(super::available(&mut ctx, ALICE), 9_000_000);
        }

        // ── Row 4: Δ > 0, available(after) < 0 ⇒ REJECT ──
        // The same buy, one unit short of affordable: post-availability is exactly −1.
        {
            let mut ctx = make_ctx();
            setup_marked(&mut ctx);
            set_raw_wallet(&mut ctx, ALICE, INIT_MARGIN as i64 - 1);
            assert_eq!(
                gate_inputs(&mut ctx, ALICE, 0, PRICE, QTY),
                (INIT_MARGIN as i128, -1),
                "row 4 inputs: positive delta, NEGATIVE post-availability"
            );
            let err = try_place(&mut ctx, ALICE, 0, PRICE, QTY, 0, POST_ONLY)
                .expect_err("row 4 must REJECT");
            assert!(
                err.to_string()
                    .contains("insufficient perp wallet for margin"),
                "{err}"
            );
            assert_eq!(oo_im(&mut ctx, ALICE), 0, "the reject wrote nothing");
        }
    }
}

// ── Per-user market index (derived-ooIM Phase 0) ─────────────────────────────
//
// The index (`umkt`) answers "which markets is this user active in?", where ACTIVE means a
// non-zero position OR at least one resting order. Nothing reads it yet — these tests are the
// whole proof that it is maintained correctly, so the centrepiece is the ground-truth property
// test: after EVERY step of a randomised operation sequence, the STORED index of each user must
// equal the set recomputed independently from the position + order lists.
mod user_market_index {
    use super::*;
    use crate::{
        interface::IPerpDex::{
            batchCancelOrdersCall, batchPlaceOrdersCall, liquidateCall, setLeverageCall, PlaceItem,
        },
        risk::{run_liquidate, run_set_leverage},
        run_perp_dex_call,
        storage::keys as storage_keys,
        types::MAX_USER_MARKETS,
    };

    /// One `PlaceItem` (the batch analogue of [`try_place_in`]'s arguments).
    fn item_in(
        market_id: u64,
        side: u8,
        price: u64,
        qty: u64,
        order_type: u8,
        tif: u8,
    ) -> PlaceItem {
        PlaceItem {
            marketId: market_id,
            side,
            price,
            quantity: qty,
            orderType: order_type,
            tif,
            clientOrderId: FixedBytes::default(),
        }
    }

    /// Markets the property test roams over.
    const MARKETS: [u64; 3] = [1, 2, 3];
    /// Deep pockets: the point of these tests is membership, never a margin reject.
    const RICH: u64 = WALLET * 10_000;

    fn market_at(id: u64) -> Market {
        Market {
            market_id: id,
            base_decimals: 8,
            price_decimals: 9,
            tick_size: TICK,
            step_size: QTY,
            min_quantity: QTY,
            max_quantity: QTY * 1_000,
            max_price: PRICE * 1_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            // mark 0 disables the fill-time band, so matching in these tests is driven purely by
            // the book. The liquidation op sets a mark deliberately and puts it back.
            price_band_bps: 0,
            mark_price: 0,
            tiers: MarginTiers::default(),
        }
    }

    fn setup_markets(ctx: &mut TestCtx, ids: &[u64], users: &[Address], wallet: u64) {
        storage::save_admin(ctx, ADMIN).unwrap();
        for &id in ids {
            storage::save_market(ctx, &market_at(id)).unwrap();
        }
        for &u in users {
            fund(ctx, u, wallet);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn try_place_in(
        ctx: &mut TestCtx,
        caller: Address,
        market: u64,
        side: u8,
        price: u64,
        qty: u64,
        order_type: u8,
        tif: u8,
    ) -> Result<Bytes, PerpError> {
        let input = placeOrderCall {
            marketId: market,
            side,
            price,
            quantity: qty,
            orderType: order_type,
            tif,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode();
        run_place_order(&input, caller, ctx)
    }

    fn place_in(ctx: &mut TestCtx, caller: Address, market: u64, side: u8, price: u64) -> [u8; 32] {
        let ret = try_place_in(ctx, caller, market, side, price, QTY, 0, 0).expect("placement");
        ret[..32].try_into().unwrap()
    }

    fn cancel_in(
        ctx: &mut TestCtx,
        caller: Address,
        market: u64,
        id: [u8; 32],
    ) -> Result<Bytes, PerpError> {
        let input = cancelOrderCall {
            orderId: id.into(),
            marketId: market,
        }
        .abi_encode();
        run_cancel_order(&input, caller, ctx)
    }

    /// The STORED index.
    fn index(ctx: &mut TestCtx, user: Address) -> Vec<u64> {
        storage::load_user_markets(ctx, user).unwrap()
    }

    /// The index recomputed from scratch out of the state it mirrors — the ground truth the
    /// property test compares against. Deliberately re-derives the predicate from the three
    /// underlying reads rather than reusing any engine helper.
    fn ground_truth(ctx: &mut TestCtx, user: Address, markets: &[u64]) -> Vec<u64> {
        markets
            .iter()
            .copied()
            .filter(|&m| {
                storage::load_position(ctx, user, m).unwrap().amount != 0
                    || !storage::load_buy_orders(ctx, user, m).unwrap().is_empty()
                    || !storage::load_sell_orders(ctx, user, m).unwrap().is_empty()
            })
            .collect()
    }

    fn assert_index_matches(ctx: &mut TestCtx, users: &[Address], markets: &[u64], step: &str) {
        for &u in users {
            assert_eq!(
                index(ctx, u),
                ground_truth(ctx, u, markets),
                "user {u} index diverged from ground truth after {step}"
            );
        }
    }

    // ── Step 0 of the derived-ooIM migration: are the maintained aggregates Bid/Ask? ────────
    //
    // The derived open-order requirement is
    //     ooIM = ROUND_UP(max(|N + Bid|, |N − Ask|) / L) − ROUND_UP(|N| / L)
    // where Binance defines `Bid` as Σ over the user's CURRENTLY RESTING buy orders of
    // (REMAINING quantity × that order's ASSUMING PRICE, frozen at placement), and `Ask` the same
    // over sells. The whole migration rests on `PerpPosition::total_buy_notional` /
    // `total_sell_notional` being EXACTLY those two quantities, maintained incrementally.
    //
    // `assert_side_totals_are_bid_and_ask` below is the proof obligation. It does not reuse
    // `math::sum_side_totals` (the engine's own fold — reusing it would only prove the engine
    // agrees with itself); it re-derives the sum inline, and additionally re-derives each term
    // from the authoritative `Order` record so that "remaining quantity" and "LIMIT price" are
    // checked as CLAIMS about the order, not just as fields of the mirror.
    //
    // ⚠️ A sell's FROZEN assuming price deliberately cannot be re-derived from current state — that
    // is what freezing means, and it is why the mark moves freely in the sweep below without the
    // ground truth having to track it. What the fold pins instead is that the maintained total is
    // the Σ of exactly the per-entry terms present in the list, at whatever price each entry
    // carries; the freeze RULE (buy: `assuming_price == price`; sell: `>= price`, and `== price`
    // above the floor) is asserted per entry alongside it.

    /// The test market's fixed-point widths (see [`market_at`]).
    const BD: u32 = 8;
    const PD: u32 = 9;

    /// Assert, for every `(user, market)`, that the four maintained per-side aggregates equal a
    /// ground truth recomputed independently from the user's actual resting orders, AND that
    /// each resting entry really is `(remaining qty, limit price)` of a live order.
    ///
    /// Five distinct claims, each of which a maintenance bug breaks differently:
    /// 1. every entry in an order LIST is backed by an `Order` that is still resting
    ///    (`Open`/`PartiallyFilled`) — so the list is "currently resting orders", not a graveyard;
    /// 2. `entry.price` is that order's LIMIT price, untouched — the margin freeze may not leak
    ///    into the field matching, sorting and the fill price all key on;
    /// 3. `entry.amount` is `quantity − filled`, the REMAINING quantity (so a partial fill really
    ///    does shrink the term);
    /// 4. the freeze RULE holds per entry: a BUY's assuming price IS its limit price (no markup,
    ///    measured), a SELL's is at least its limit price and exactly it when the order rests above
    ///    the floor;
    /// 5. the stored aggregates equal the inline Σ over those entries, at each entry's assuming
    ///    price.
    fn assert_side_totals_are_bid_and_ask(
        ctx: &mut TestCtx,
        users: &[Address],
        markets: &[u64],
        step: &str,
    ) -> u32 {
        // Sell entries whose FROZEN assuming price sits strictly above their own limit — the shape
        // that makes this whole assertion discriminating (with none of them, folding at the limit
        // price would pass too). Returned so the driving test can assert the sweep reaches them.
        let mut marked_up_sells = 0u32;
        for &u in users {
            for &m in markets {
                let mut truth = [0u64; 4]; // tbq, tbn, tsq, tsn
                for (side, buy) in [(Side::Buy, true), (Side::Sell, false)] {
                    let entries = if buy {
                        storage::load_buy_orders(ctx, u, m).unwrap()
                    } else {
                        storage::load_sell_orders(ctx, u, m).unwrap()
                    };
                    for e in entries.iter() {
                        // (1) backed by a live, still-resting order in THIS market on THIS side.
                        let o = storage::load_order(ctx, &e.order_id)
                            .unwrap()
                            .unwrap_or_else(|| {
                                panic!(
                                "{step}: user {u} market {m} {side:?} list holds entry {:?} with \
                                 no order record — a filled/cancelled order was left in the list",
                                e.order_id
                            )
                            });
                        assert!(
                            matches!(o.status, OrderStatus::Open | OrderStatus::PartiallyFilled),
                            "{step}: user {u} market {m} {side:?} list holds entry {:?} whose \
                             order is terminal ({:?}) — it is not a RESTING order",
                            e.order_id,
                            o.status
                        );
                        assert_eq!(
                            (o.market_id, o.side),
                            (m, side),
                            "{step}: entry {:?} filed under the wrong (market, side)",
                            e.order_id
                        );
                        // (2) `entry.price` is still the order's LIMIT price. This is the guard
                        // against `assuming_price` leaking into the execution field: matching,
                        // price priority, the sorted insert, the book level and the fill price all
                        // read `entry.price`, so a markup landing here would be silent corruption.
                        assert_eq!(
                            e.price, o.price,
                            "{step}: user {u} market {m} entry {:?} price {} is not the order's \
                             LIMIT price {}",
                            e.order_id, e.price, o.price
                        );
                        // (3) the quantity is what is still RESTING, i.e. net of partial fills.
                        assert_eq!(
                            e.amount,
                            o.quantity - o.filled,
                            "{step}: user {u} market {m} entry {:?} amount {} != remaining \
                             (quantity {} − filled {}) — a partial fill did not shrink the term",
                            e.order_id,
                            e.amount,
                            o.quantity,
                            o.filled
                        );
                        // (4) the freeze rule, per entry.
                        if buy {
                            assert_eq!(
                                e.assuming_price, e.price,
                                "{step}: user {u} market {m} BUY entry {:?} carries a marked-up \
                                 assuming price {} over limit {} — the buy side has NO markup",
                                e.order_id, e.assuming_price, e.price
                            );
                        } else {
                            assert!(
                                e.assuming_price >= e.price,
                                "{step}: user {u} market {m} SELL entry {:?} assuming price {} is \
                                 BELOW its limit {} — max(T, limit) can never be",
                                e.order_id,
                                e.assuming_price,
                                e.price
                            );
                            if e.assuming_price > e.price {
                                marked_up_sells += 1;
                            }
                        }
                        // (5) fold, inline — deliberately NOT `math::sum_side_totals`, and at the
                        // entry's FROZEN assuming price, which is the basis every maintenance site
                        // adds and subtracts.
                        let i = if buy { 0 } else { 2 };
                        truth[i] += e.amount;
                        truth[i + 1] +=
                            crate::math::calc_value(e.assuming_price, e.amount, BD, PD).unwrap();
                    }
                }
                let p = storage::load_position(ctx, u, m).unwrap();
                assert_eq!(
                    (
                        p.total_buy_qty,
                        p.total_buy_notional,
                        p.total_sell_qty,
                        p.total_sell_notional
                    ),
                    (truth[0], truth[1], truth[2], truth[3]),
                    "{step}: user {u} market {m}: maintained (tbq, tbn=Bid, tsq, tsn=Ask) diverged \
                     from the resting-order ground truth (Σ at each entry's FROZEN assuming price)"
                );
            }
        }
        marked_up_sells
    }

    // ── The stored `Σ pos.margin` aggregate: is it exactly the sum? ─────────────────────────
    //
    // `AccountBalanceChanged.totalWalletBalance` is `perp_wallet_balance + Σ pos.margin`, and the Σ
    // leg is no longer walked — it is the STORED `UserAccount::total_position_margin`, maintained
    // incrementally by `storage::save_position` from `pos.margin − old.margin`. A stored aggregate can
    // drift where the walked one could not, so it needs the same treatment the side aggregates got:
    // recomputed ground truth after EVERY transition.
    //
    // Like `assert_side_totals_are_bid_and_ask`, this deliberately does NOT ask the engine for its own
    // fold (`margin_view::index_account_scalars` / `fold_account_margin`) — that would only prove the
    // engine agrees with itself. It sums `pos.margin` inline out of the position blobs, over ALL the
    // sweep's markets rather than the per-user index, which additionally pins the identity the
    // published surfaces depend on: `Σ_all pos.margin == Σ_index pos.margin`, i.e. no margin is ever
    // stranded on a position outside the index (`getAccount` sums over the index, this field over
    // everything).

    /// Assert, for every user, that the stored `total_position_margin` equals `Σ pos.margin`
    /// recomputed from the position blobs. Returns the number of users whose aggregate is currently
    /// non-zero, so the driving test can prove the sweep is not asserting `0 == 0`.
    fn assert_stored_total_position_margin(
        ctx: &mut TestCtx,
        users: &[Address],
        markets: &[u64],
        step: &str,
    ) -> u32 {
        let mut non_zero = 0u32;
        for &u in users {
            let mut truth: i128 = 0;
            for &m in markets {
                let p = storage::load_position(ctx, u, m).unwrap();
                // The invariant that makes the index-driven `getAccount` fold and this all-markets
                // sum the same number — and which `storage::save_position` asserts on every write.
                assert!(
                    p.amount != 0 || p.margin == 0,
                    "{step}: user {u} market {m} is flat but holds margin {} — margin stranded \
                     outside the per-user market index would make getAccount's totalWalletBalance \
                     disagree with the event's",
                    p.margin
                );
                truth += p.margin as i128;
            }
            let stored = storage::load_account(ctx, u).unwrap().total_position_margin;
            assert_eq!(
                stored as i128, truth,
                "{step}: user {u} stored total_position_margin diverged from Σ pos.margin \
                 recomputed from the position blobs"
            );
            if truth != 0 {
                non_zero += 1;
            }
        }
        non_zero
    }

    /// Deterministic xorshift (same generator as the settlement conservation fuzz).
    fn next(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    // ── 1. The deliverable: ground-truth property test ──────────────────────

    /// What one randomised pass actually reached. A property test that never gets to the
    /// interesting transitions proves nothing, so the pass returns its coverage and the test
    /// asserts on it.
    #[derive(Debug, Default)]
    struct FuzzCoverage {
        enters: u32,
        leaves: u32,
        liquidations: u32,
        flips: u32,
        /// Max number of resting SELLS seen in one sweep whose FROZEN assuming price is strictly
        /// above their own limit price. Zero would make the Bid/Ask fold non-discriminating: with no
        /// marked-up sell anywhere, folding at the limit price would agree too.
        marked_up_sells: u32,
        /// Max number of users seen in one sweep whose stored `Σ pos.margin` is non-zero. Zero would
        /// make [`assert_stored_total_position_margin`] assert `0 == 0` for the whole pass.
        margin_bearing_users: u32,
    }

    /// One randomised pass: a mix of place / cancel / fill / partial fill / close / flip /
    /// liquidate across 3 markets and 2 users, asserting after EVERY step that each user's STORED
    /// index equals `{ market : position != 0 OR buy_orders non-empty OR sell_orders non-empty }`,
    /// recomputed independently from storage.
    ///
    /// This is what proves the maintenance hooks cover every transition: any write path that moves
    /// `pos.amount` or an order list without going through them desynchronises the index within a
    /// step or two of being exercised. Rejected operations are kept in the mix on purpose — a
    /// reject must leave the index untouched too (validate-then-apply).
    fn run_fuzz(seed: u64, wallet: u64, steps: u32) -> FuzzCoverage {
        let users = [ALICE, BOB];
        let mut ctx = make_ctx();
        setup_markets(&mut ctx, &MARKETS, &users, wallet);
        // Leverage 3 (the default tier cap) so an adverse mark can actually breach maintenance —
        // a leverage-1 position is mathematically never liquidatable.
        for &u in &users {
            for &m in &MARKETS {
                run_set_leverage(
                    &setLeverageCall {
                        marketId: m,
                        leverage: 3,
                    }
                    .abi_encode(),
                    u,
                    &mut ctx,
                )
                .unwrap();
            }
        }
        assert_index_matches(&mut ctx, &users, &MARKETS, "setup");

        // Live order ids per (user index, market) so cancels have something real to aim at.
        let mut live: Vec<Vec<[u8; 32]>> = vec![Vec::new(); users.len() * MARKETS.len()];
        let slot = |ui: usize, mi: usize| ui * MARKETS.len() + mi;

        let mut cov = FuzzCoverage::default();
        let mut s: u64 = seed;
        for step in 0..steps {
            let r = next(&mut s);
            let ui = (r % users.len() as u64) as usize;
            let mi = ((r >> 8) % MARKETS.len() as u64) as usize;
            let user = users[ui];
            let market = MARKETS[mi];
            let op = (r >> 16) % 8;
            // Prices straddle $100 in $5 steps so orders both rest and cross.
            let price = (90 + ((r >> 24) % 5) * 5) * TICK;
            // 1..=4 lots: partial fills, full fills and over-fills (flips) all occur.
            let qty = QTY * (1 + ((r >> 32) % 4));

            let before: Vec<Vec<u64>> = users.iter().map(|&u| index(&mut ctx, u)).collect();
            let amount_before = storage::load_position(&mut ctx, user, market)
                .unwrap()
                .amount;

            let what: &str = match op {
                // Resting / crossing GTC limits on both sides (the bread and butter: rest,
                // full fill, partial fill, position open/increase/decrease/close/flip).
                0..=3 => {
                    let side = (op % 2) as u8;
                    if let Ok(ret) = try_place_in(&mut ctx, user, market, side, price, qty, 0, 0) {
                        let id: [u8; 32] = ret[..32].try_into().unwrap();
                        // Only a still-live (resting) order is a future cancel target.
                        if storage::load_order(&mut ctx, &id).unwrap().is_some() {
                            live[slot(ui, mi)].push(id);
                        }
                    }
                    "limit place"
                }
                // Market order: fills or expires, never rests.
                4 => {
                    let side = ((r >> 40) % 2) as u8;
                    let _ = try_place_in(&mut ctx, user, market, side, 0, qty, 1, 1);
                    "market place"
                }
                // Cancel one live order (the last-cancel is what removes an order-only member).
                5 | 6 => {
                    if let Some(id) = live[slot(ui, mi)].pop() {
                        let _ = cancel_in(&mut ctx, user, market, id);
                    }
                    "cancel"
                }
                // Liquidate: cancel-all + close, the widest single-step transition there is.
                // Crash/spike the mark against the holder's side, liquidate, then restore mark 0
                // (which is also what keeps the fill-time band out of the other ops' way).
                _ => {
                    let amount = storage::load_position(&mut ctx, user, market)
                        .unwrap()
                        .amount;
                    if amount != 0 {
                        let mark = if amount > 0 { 10 * TICK } else { 400 * TICK };
                        storage::save_mark_price(&mut ctx, market, mark).unwrap();
                        let _ = run_liquidate(
                            &liquidateCall {
                                user,
                                marketId: market,
                            }
                            .abi_encode(),
                            users[1 - ui],
                            &mut ctx,
                        );
                        storage::save_mark_price(&mut ctx, market, 0).unwrap();
                        // Liquidation cancel-all kills every resting order of that user here.
                        live[slot(ui, mi)].clear();
                        if storage::load_position(&mut ctx, user, market)
                            .unwrap()
                            .amount
                            == 0
                        {
                            cov.liquidations += 1;
                        }
                    }
                    "liquidate"
                }
            };

            let amount_after = storage::load_position(&mut ctx, user, market)
                .unwrap()
                .amount;
            if amount_before.signum() * amount_after.signum() == -1 {
                cov.flips += 1;
            }
            for (i, &u) in users.iter().enumerate() {
                let after = index(&mut ctx, u);
                cov.enters += after.iter().filter(|m| !before[i].contains(m)).count() as u32;
                cov.leaves += before[i].iter().filter(|m| !after.contains(m)).count() as u32;
            }

            let where_ = format!("step {step} ({what}, user {ui}, market {market})");
            assert_index_matches(&mut ctx, &users, &MARKETS, &where_);
            // Derived-ooIM Step 0: the SAME sweep also proves the per-side reservation
            // aggregates are exactly Binance's Bid/Ask after every transition. One pass, two
            // invariants — the op mix (rest / partial fill / full fill / flip / cancel /
            // mid-match auto-cancel / liquidation cancel-all / reject) is precisely the set of
            // transitions that can desynchronise them.
            cov.marked_up_sells = cov.marked_up_sells.max(assert_side_totals_are_bid_and_ask(
                &mut ctx, &users, &MARKETS, &where_,
            ));
            // ...and the THIRD invariant on the same sweep: the stored `Σ pos.margin` aggregate the
            // `AccountBalanceChanged` payload is now read from. Same op mix, and it is precisely the
            // set of transitions that move `pos.margin` — open, partial/full fill, flip, funding,
            // liquidation close, ADL, add/removePositionMargin.
            cov.margin_bearing_users =
                cov.margin_bearing_users
                    .max(assert_stored_total_position_margin(
                        &mut ctx, &users, &MARKETS, &where_,
                    ));
            // The index is a SET, always ascending, and never exceeds the cap.
            for &u in &users {
                let ix = index(&mut ctx, u);
                assert!(
                    ix.windows(2).all(|w| w[0] < w[1]),
                    "index not strictly ascending at step {step}: {ix:?}"
                );
                assert!(ix.len() <= MAX_USER_MARKETS, "cap breached at step {step}");
            }
        }
        cov
    }

    /// The centrepiece. Two passes with deliberately different money:
    ///
    /// * **deep pockets** — placements are never margin-rejected, so the mix is dominated by clean
    ///   rests, fills, flips and liquidations;
    /// * **thin wallets** — the paths that only appear when money runs out: the taker wallet-cover
    ///   LIFO cancel cascade and the maker open-into-insolvency reject. Both empty an order list
    ///   *mid-match*, through the match registry's flush, with the holder flat before AND after —
    ///   the one shape no other hook can repair, so it is the pass that makes the order-list
    ///   `save_*` hooks load-bearing.
    #[test]
    fn index_matches_ground_truth_after_every_operation() {
        let deep = run_fuzz(0x0d_e4_11_ed_5e_ed_00_1f, RICH, 800);
        let thin = run_fuzz(0x5c_a4_ce_11_a7_10_00_23, WALLET, 800);
        println!("user-market-index fuzz coverage: deep={deep:?} thin={thin:?}");
        for (name, cov) in [("deep", &deep), ("thin", &thin)] {
            assert!(
                cov.enters >= 20,
                "{name}: too few market entries ({})",
                cov.enters
            );
            assert!(
                cov.leaves >= 20,
                "{name}: too few market exits ({})",
                cov.leaves
            );
        }
        assert!(deep.liquidations >= 1, "no liquidation ever completed");
        assert!(deep.flips >= 1, "no position ever flipped sign");
    }

    /// **Derived-ooIM Step 0 deliverable, on the FROZEN basis.** The maintained per-side aggregates
    /// `total_buy_notional` / `total_sell_notional` ARE Binance's `Bid` / `Ask` — Σ over the user's
    /// currently resting orders of (remaining quantity × that order's ASSUMING PRICE, frozen when it
    /// was placed) — and the two qty aggregates are the matching Σ quantity, after EVERY step of a
    /// randomised sequence.
    ///
    /// The per-step assertion lives inside [`run_fuzz`] (see
    /// [`assert_side_totals_are_bid_and_ask`]); this test drives it over its own seeds so the
    /// property has a named owner, and asserts the pass actually reached the transitions that
    /// could break it. Two passes, deliberately different money, exactly as the index property
    /// test: the thin-wallet pass is the only one that reaches the taker wallet-cover LIFO
    /// cancel cascade and the mid-match order-list rewrite, which are the two paths that resync
    /// the aggregates by RECOMPUTE rather than incrementally.
    ///
    /// `marked_up_sells` is the coverage floor the freeze added: the sweep's sells straddle the
    /// Assuming-Price floor implied by the last print, so entries with `assuming_price > price`
    /// really do occur. Without one, folding at the LIMIT price would satisfy the assertion too and
    /// the whole property would be blind to the change.
    ///
    /// Mutation-tested on the frozen basis. **Four maintenance sites, each killed:**
    ///
    /// | site | mutation | result |
    /// |---|---|---|
    /// | placement increment, BUY arm (`trading/mod.rs`) | drop the `checked_add` | FAILS |
    /// | placement increment, SELL arm (`trading/mod.rs`) | drop the `checked_add` | FAILS |
    /// | the shared decrement (`remove_entry_from_side_aggregates`) — cancel, partial fill, LIFO cover | no-op the `checked_sub` | FAILS |
    /// | partial-fill delta feeding it (`settle_maker_fill_core`) | pass 0 instead of the delta | FAILS |
    /// | liquidation cancel-all zeroing (`risk/mod.rs`, `clear_side_aggregates`) | drop the call | FAILS |
    ///
    /// …and mutation-tested against the FREEZE specifically: maintaining any one site at
    /// `entry.price` instead of `entry.assuming_price` also FAILS, because the sites then disagree
    /// with each other on a marked-up sell (tried on the cancel decrement and on
    /// `math::sum_side_totals`).
    ///
    /// ⚠️ **One thing this test does NOT prove, and an earlier version of this comment wrongly
    /// claimed it did:** the match flush's `w.pos.total_* = t*` RESYNC (`settlement.rs::flush`)
    /// SURVIVES deletion. It is redundant by construction — the walk already maintains the
    /// aggregates incrementally, and the `debug_assert_eq!` two lines above the assignment is
    /// exactly the statement that the recompute equals what is already there. So the assignment can
    /// only matter if the incremental maintenance is wrong, and in that case the debug assert fires
    /// first: zeroing the partial-fill delta (row 4) panics AT that assert, not at this test's own
    /// fold. The resync is a release-build backstop, not an independently observable site. This
    /// survival is pre-existing (that block is untouched by the freeze), not a regression.
    #[test]
    fn side_aggregates_are_exactly_bid_and_ask_after_every_operation() {
        let deep = run_fuzz(0x00_1f_bd_a5_c0_de_00_11, RICH, 800);
        let thin = run_fuzz(0x7a_51_de_ad_be_ef_00_29, WALLET, 800);
        println!("ooIM Bid/Ask fuzz coverage: deep={deep:?} thin={thin:?}");
        // A pass that never rests, never partially fills and never liquidates would assert
        // "0 == 0" 800 times. Reuse the index pass's own coverage floors.
        for (name, cov) in [("deep", &deep), ("thin", &thin)] {
            assert!(cov.enters >= 20, "{name}: too few market entries");
            assert!(cov.leaves >= 20, "{name}: too few market exits");
            assert!(
                cov.marked_up_sells >= 1,
                "{name}: no resting sell ever carried a marked-up assuming price, so the fold \
                 cannot tell the frozen basis from the limit-price one"
            );
        }
        assert!(
            deep.liquidations >= 1,
            "no liquidation cancel-all exercised"
        );
        assert!(deep.flips >= 1, "no position ever flipped sign");
    }

    /// **The stored `Σ pos.margin` aggregate equals a recomputed `Σ pos.margin` after EVERY
    /// operation.** `UserAccount::total_position_margin` is what
    /// `AccountBalanceChanged.totalWalletBalance` is now read from (`wb = cw + this`), replacing a
    /// walk of the user's market index and a position load per member market. Stored, it can DRIFT —
    /// this is the property that says it does not.
    ///
    /// The per-step assertion lives inside [`run_fuzz`] (see
    /// [`assert_stored_total_position_margin`]); this test drives it over its own seeds so the
    /// property has a named owner, and asserts the pass actually reached states where the aggregate is
    /// non-zero (otherwise it would compare `0 == 0` 800 times). Two passes with deliberately
    /// different money, exactly as its two sibling properties: the thin-wallet pass is the one that
    /// reaches the underfunded maker fill, whose silo is short of its own IM, and the taker
    /// wallet-cover cascade.
    ///
    /// Ground truth is computed INDEPENDENTLY — `Σ` over the position blobs, inline, not
    /// `margin_view`'s own fold — for the same reason the Bid/Ask property does it: reusing the
    /// engine's fold would only prove the engine agrees with itself. It also sums over ALL the
    /// sweep's markets rather than the per-user index, which pins the identity the two published
    /// surfaces depend on (`getAccount` sums over the index, the event reads this all-markets field).
    ///
    /// Mutation-tested on the single maintenance point in `storage::save_position`. **Each broken
    /// variant killed:**
    ///
    /// | mutation | caught by |
    /// |---|---|
    /// | drop the increment entirely | THIS test (`step 1`), and the `debug_assertions` cross-check in `margin_view::index_account_scalars` via ~15 other tests |
    /// | wrong sign (`old_margin − pos.margin`) | THIS test, and the same cross-check |
    /// | `pos.margin` instead of the delta | THIS test, and the same cross-check |
    ///
    /// The cross-check is what makes a missed maintenance point loud everywhere rather than only
    /// here; this test is what pins the arithmetic over a transition mix no single scenario reaches.
    #[test]
    fn stored_position_margin_is_exactly_the_sum_after_every_operation() {
        let deep = run_fuzz(0x50_5f_a2_91_00_00_00_11, RICH, 800);
        let thin = run_fuzz(0x9e_ed_5c_a4_1e_d0_00_37, WALLET, 800);
        println!("Σ pos.margin fuzz coverage: deep={deep:?} thin={thin:?}");
        for (name, cov) in [("deep", &deep), ("thin", &thin)] {
            assert!(cov.enters >= 20, "{name}: too few market entries");
            assert!(cov.leaves >= 20, "{name}: too few market exits");
            assert!(
                cov.margin_bearing_users >= 2,
                "{name}: never reached a state where two users held margin at once, so the \
                 aggregate was compared against a trivial sum"
            );
        }
        assert!(
            deep.liquidations >= 1,
            "no liquidation ever zeroed a silo through the close path"
        );
        assert!(deep.flips >= 1, "no position ever flipped sign");
    }

    // ── 2. Enter / leave round trip ─────────────────────────────────────────

    /// Entering a market inserts ONCE (a second order in the same market is not a second entry);
    /// leaving happens on the LAST order cancel, not the first; and a position with no orders at
    /// all still keeps the user in.
    #[test]
    fn enter_once_leave_on_the_last_order_and_a_position_alone_keeps_membership() {
        let users = [ALICE, BOB];
        let mut ctx = make_ctx();
        setup_markets(&mut ctx, &MARKETS, &users, RICH);
        assert!(index(&mut ctx, ALICE).is_empty(), "starts in no market");

        // First resting bid in market 1 → enter.
        let a1 = place_in(&mut ctx, ALICE, 1, 0, 95 * TICK);
        assert_eq!(index(&mut ctx, ALICE), vec![1]);

        // Second order in the SAME market → still one entry, not two.
        let a2 = place_in(&mut ctx, ALICE, 1, 0, 94 * TICK);
        assert_eq!(
            index(&mut ctx, ALICE),
            vec![1],
            "second order must not re-enter"
        );
        // ...and the other side of the same market is not a third entry either.
        let a3 = place_in(&mut ctx, ALICE, 1, 1, 105 * TICK);
        assert_eq!(index(&mut ctx, ALICE), vec![1]);

        // A different market appends (ascending).
        let b1 = place_in(&mut ctx, ALICE, 2, 0, 95 * TICK);
        assert_eq!(index(&mut ctx, ALICE), vec![1, 2]);

        // Cancelling all but one order in market 1 keeps her in it.
        cancel_in(&mut ctx, ALICE, 1, a1).unwrap();
        cancel_in(&mut ctx, ALICE, 1, a3).unwrap();
        assert_eq!(
            index(&mut ctx, ALICE),
            vec![1, 2],
            "one order left → still a member"
        );
        // The LAST cancel leaves.
        cancel_in(&mut ctx, ALICE, 1, a2).unwrap();
        assert_eq!(
            index(&mut ctx, ALICE),
            vec![2],
            "last order cancelled → left market 1"
        );

        // A POSITION with no orders keeps her in: BOB lifts her market-2 bid, so her only
        // remaining trace there is the long it opened.
        place_in(&mut ctx, BOB, 2, 1, 95 * TICK);
        assert_terminal(&mut ctx, b1);
        assert!(
            storage::load_buy_orders(&mut ctx, ALICE, 2)
                .unwrap()
                .is_empty()
                && storage::load_position(&mut ctx, ALICE, 2).unwrap().amount != 0,
            "market 2 must now be position-only for ALICE"
        );
        assert_eq!(
            index(&mut ctx, ALICE),
            vec![2],
            "a position alone keeps membership"
        );
        assert_eq!(
            index(&mut ctx, BOB),
            vec![2],
            "and gives BOB the opposite side"
        );

        // Closing the position is the last thing she had there: BOB quotes a bid (he is short,
        // so this closes both sides) and ALICE market-sells into it.
        place_in(&mut ctx, BOB, 2, 0, 95 * TICK);
        try_place_in(&mut ctx, ALICE, 2, 1, 0, QTY, 1, 1).unwrap();
        assert_eq!(
            storage::load_position(&mut ctx, ALICE, 2).unwrap().amount,
            0
        );
        assert_eq!(storage::load_position(&mut ctx, BOB, 2).unwrap().amount, 0);
        assert!(
            index(&mut ctx, ALICE).is_empty(),
            "flat everywhere → index key deleted"
        );
        assert!(
            index(&mut ctx, BOB).is_empty(),
            "the maker leaves on the same fill"
        );
    }

    // ── 3. The cap ──────────────────────────────────────────────────────────

    /// Entering one market past [`MAX_USER_MARKETS`] rejects with its own error string, and does
    /// so BEFORE any write: the refused market has no order, no book entry and no index slot, and
    /// the user's wallet and order nonce are exactly where they were.
    #[test]
    fn cap_rejects_the_market_past_the_limit_before_any_write() {
        let over = MAX_USER_MARKETS as u64 + 1;
        let all: Vec<u64> = (1..=over).collect();
        let mut ctx = make_ctx();
        setup_markets(&mut ctx, &all, &[ALICE], RICH);

        for m in 1..=MAX_USER_MARKETS as u64 {
            place_in(&mut ctx, ALICE, m, 0, 95 * TICK);
        }
        let full: Vec<u64> = (1..=MAX_USER_MARKETS as u64).collect();
        assert_eq!(index(&mut ctx, ALICE), full, "exactly at the cap");

        let wallet_before = wallet(&mut ctx, ALICE);
        let nonce_before = storage::load_user_nonce(&mut ctx, ALICE).unwrap();
        let err = try_place_in(&mut ctx, ALICE, over, 0, 95 * TICK, QTY, 0, 0).unwrap_err();
        assert!(
            err.to_string().contains("user market limit reached"),
            "wrong error: {err}"
        );

        // Pre-write: nothing about the refused market exists, and nothing of ALICE's moved.
        assert_eq!(
            index(&mut ctx, ALICE),
            full,
            "index unchanged by the reject"
        );
        assert!(storage::load_buy_orders(&mut ctx, ALICE, over)
            .unwrap()
            .is_empty());
        assert_eq!(
            storage::load_position(&mut ctx, ALICE, over)
                .unwrap()
                .amount,
            0
        );
        assert!(storage::load_bid_prices(&mut ctx, over).unwrap().is_empty());
        assert_eq!(wallet(&mut ctx, ALICE), wallet_before, "no margin reserved");
        assert_eq!(
            storage::load_user_nonce(&mut ctx, ALICE).unwrap(),
            nonce_before,
            "a rejected placement must not consume an order id"
        );

        // A market she is ALREADY in is still admitted at the cap — the gate bounds the SET, not
        // the order count.
        place_in(&mut ctx, ALICE, 1, 0, 94 * TICK);
        assert_eq!(index(&mut ctx, ALICE), full);

        // Leaving one frees exactly one slot.
        let orders = storage::load_buy_orders(&mut ctx, ALICE, 2).unwrap();
        for e in orders.iter() {
            cancel_in(&mut ctx, ALICE, 2, e.order_id).unwrap();
        }
        assert!(!index(&mut ctx, ALICE).contains(&2));
        place_in(&mut ctx, ALICE, over, 0, 95 * TICK);
        assert!(
            index(&mut ctx, ALICE).contains(&over),
            "freed slot is reusable"
        );
    }

    // ── 4. Canonical ordering ───────────────────────────────────────────────

    /// The stored blob depends only on the SET, never on the order the markets were entered in.
    /// Compared on the real block-delta bytes (what the commitment folds), not on the decoded Vec.
    #[test]
    fn same_set_reached_by_different_orders_has_byte_identical_blobs() {
        fn blob_after(entry_order: &[u64]) -> Vec<u8> {
            let mut ctx = make_ctx();
            setup_markets(&mut ctx, &MARKETS, &[ALICE], RICH);
            for (i, &m) in entry_order.iter().enumerate() {
                // Vary the side and price too, so nothing but the set can coincide.
                let side = (i % 2) as u8;
                let price = if side == 0 { 95 * TICK } else { 105 * TICK };
                place_in(&mut ctx, ALICE, m, side, price);
            }
            let delta = JournalTr::take_perp_delta(ctx.journal_mut());
            delta
                .get(&storage_keys::user_markets_key(ALICE))
                .expect("the index key is in the block delta")
                .bytes
                .clone()
        }

        let ascending = blob_after(&[1, 2, 3]);
        let descending = blob_after(&[3, 2, 1]);
        let shuffled = blob_after(&[2, 3, 1]);
        assert_eq!(
            ascending, descending,
            "entry order must not reach the bytes"
        );
        assert_eq!(ascending, shuffled, "entry order must not reach the bytes");
        assert!(!ascending.is_empty());
        // And it really is the canonical encoding of the ascending set.
        assert_eq!(ascending, storage::encode(&vec![1u64, 2, 3]).unwrap());

        // Re-entering after leaving is likewise order-free: {1,3} is {1,3} either way.
        assert_eq!(blob_after(&[1, 3]), blob_after(&[3, 1]));
    }

    // ── 6. The batch single-initiator working-set ───────────────────────────

    /// A batch routes the initiator's position and order lists through a call-scoped working-set
    /// that is only flushed into the main store at the end, while the index itself is NOT hoisted.
    /// The maintenance hooks therefore read one copy and write another — this pins that the two
    /// stay in agreement, both per item and after the flush.
    #[test]
    fn batch_place_and_cancel_maintain_the_index_through_the_working_set() {
        let mut ctx = make_ctx();
        setup_markets(&mut ctx, &MARKETS, &[ALICE], RICH);

        let items: Vec<PlaceItem> = MARKETS
            .iter()
            .flat_map(|&m| {
                [
                    item_in(m, 0, 95 * TICK, QTY, 0, 0),
                    item_in(m, 1, 105 * TICK, QTY, 0, 0),
                ]
            })
            .collect();
        let out = run_perp_dex_call(
            &batchPlaceOrdersCall {
                orders: items.clone(),
            }
            .abi_encode(),
            30_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .expect("batch place");
        assert!(!out.reverted);
        assert_eq!(
            index(&mut ctx, ALICE),
            MARKETS.to_vec(),
            "one batch entered all three markets"
        );
        assert_index_matches(&mut ctx, &[ALICE], &MARKETS, "batch place");

        // Batch-cancel everything: every leg empties, so ALICE leaves every market.
        let ids: Vec<FixedBytes<32>> = MARKETS
            .iter()
            .flat_map(|&m| {
                let buys = storage::load_buy_orders(&mut ctx, ALICE, m).unwrap();
                let sells = storage::load_sell_orders(&mut ctx, ALICE, m).unwrap();
                buys.iter()
                    .chain(sells.iter())
                    .map(|e| FixedBytes(e.order_id))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(ids.len(), items.len());
        let out = run_perp_dex_call(
            &batchCancelOrdersCall { orderIds: ids }.abi_encode(),
            30_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .expect("batch cancel");
        assert!(!out.reverted);
        assert_index_matches(&mut ctx, &[ALICE], &MARKETS, "batch cancel");
        assert!(
            index(&mut ctx, ALICE).is_empty(),
            "cancelling every order leaves every market"
        );
    }

    // ── 5. Liquidation clears the market ────────────────────────────────────

    /// A liquidation cancels every resting order AND closes the position, so it is the one
    /// operation that can clear all three legs at once — the user must leave the market.
    #[test]
    fn liquidation_removes_the_user_from_the_market_it_clears() {
        let users = [ALICE, BOB];
        let mut ctx = make_ctx();
        setup_markets(&mut ctx, &MARKETS, &users, RICH);
        for &m in &[1u64, 2] {
            run_set_leverage(
                &setLeverageCall {
                    marketId: m,
                    leverage: 3,
                }
                .abi_encode(),
                ALICE,
                &mut ctx,
            )
            .unwrap();
        }

        // ALICE goes long market 1 (BOB is the maker), and keeps resting orders on BOTH sides
        // there plus an unrelated order in market 2.
        place_in(&mut ctx, BOB, 1, 1, 100 * TICK);
        place_in(&mut ctx, ALICE, 1, 0, 100 * TICK);
        assert!(storage::load_position(&mut ctx, ALICE, 1).unwrap().amount > 0);
        place_in(&mut ctx, ALICE, 1, 0, 90 * TICK);
        place_in(&mut ctx, ALICE, 1, 1, 130 * TICK);
        place_in(&mut ctx, ALICE, 2, 0, 95 * TICK);
        assert_eq!(index(&mut ctx, ALICE), vec![1, 2]);

        // Crash the mark: the long breaches maintenance and CAROL liquidates it.
        storage::save_mark_price(&mut ctx, 1, 10 * TICK).unwrap();
        run_liquidate(
            &liquidateCall {
                user: ALICE,
                marketId: 1,
            }
            .abi_encode(),
            CAROL,
            &mut ctx,
        )
        .unwrap();

        assert_eq!(
            storage::load_position(&mut ctx, ALICE, 1).unwrap().amount,
            0
        );
        assert!(storage::load_buy_orders(&mut ctx, ALICE, 1)
            .unwrap()
            .is_empty());
        assert!(storage::load_sell_orders(&mut ctx, ALICE, 1)
            .unwrap()
            .is_empty());
        assert_eq!(
            index(&mut ctx, ALICE),
            vec![2],
            "liquidation cleared market 1; the untouched market 2 order keeps her there"
        );
    }
}

// ── The derived open-order requirement: the scenarios the escrow used to price differently ───
//
// The engine's open-order requirement is
//     ooIM = ROUND_UP(max(|N + Bid|, |N − Ask|) / L) − ROUND_UP(|N| / L)
// with `N` the position notional at MARK and `Bid`/`Ask` the resting orders at their LIMIT
// prices, summed over the per-user market index to give `available = wallet − Σ ooIM`.
//
// This module was written in Phase 1 as a CENSUS: it pinned, scenario by scenario, where that
// formula disagreed with the flip-aware escrow `max(S + B', B + S')` it has now replaced. (For
// the record, the census measured 238 / 5099 gate evaluations reaching OPPOSITE decisions, 1499
// pricing the same operation differently, and 1249 holding a different available. Those figures
// describe a comparison that no longer exists.) The escrow is gone, so the escrow half of every
// assertion is gone with it — but the SCENARIOS are the only place these book shapes are
// recorded, so each test survives as a behaviour pin on the surviving basis, with the number the
// escrow used to produce kept in the prose as the delta this migration accepted.
//
// Fixture arithmetic: base_decimals 8, price_decimals 9, so one QTY lot (0.01) at $P is worth
// `P × 10_000` quote units — $100 ⇒ 1_000_000, $90 ⇒ 900_000, $110 ⇒ 1_100_000.
mod derived_ooim_divergence {
    use super::*;
    use crate::margin_view::{position_open_order_margin, total_open_order_initial_margin};
    use crate::types::MarginTier;

    const P_LOW: u64 = 90 * TICK;
    const P_HIGH: u64 = 110 * TICK;

    fn market_with(mark: u64, tiers: MarginTiers) -> Market {
        Market {
            market_id: MARKET_ID,
            base_decimals: 8,
            price_decimals: 9,
            tick_size: TICK,
            step_size: QTY,
            min_quantity: QTY,
            max_quantity: QTY * 1_000,
            max_price: PRICE * 1_000,
            price_update_interval: 15,
            active: true,
            funding_interval: 0,
            interest_rate: 0,
            liquidation_fee_rate_bps: 0,
            price_band_bps: 0,
            mark_price: mark,
            tiers,
        }
    }

    /// A market with a live mark price. `setup()` leaves `mark_price` at 0, which makes the
    /// derived basis blind to every position (`N = trunc(|amt| × 0) = 0`) — a test artefact, not
    /// production behaviour (`addMarket` rejects a zero mark). One test below exists precisely to
    /// separate that artefact from the real numbers.
    fn setup_marked(ctx: &mut TestCtx, mark: u64) {
        storage::save_admin(ctx, ADMIN).unwrap();
        storage::save_market(ctx, &market_with(mark, MarginTiers::default())).unwrap();
        fund(ctx, ALICE, WALLET * 100);
        fund(ctx, BOB, WALLET * 100);
    }

    fn place(ctx: &mut TestCtx, who: Address, side: u8, price: u64, lots: u64) -> [u8; 32] {
        let input = placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: QTY * lots,
            orderType: 0,
            tif: 3, // PostOnly: rest without matching, so the book state is exactly as written
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode();
        let ret = run_place_order(&input, who, ctx).expect("placement");
        ret[..32].try_into().unwrap()
    }

    fn seed_position(ctx: &mut TestCtx, who: Address, lots: i64, leverage: u64) {
        let notional = lots * FILL_VALUE as i64;
        storage::save_position(
            ctx,
            who,
            MARKET_ID,
            &PerpPosition {
                amount: lots * QTY as i64,
                v_quote_balance: -notional,
                margin: notional.abs(),
                leverage,
                ..PerpPosition::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
    }

    /// ALICE's derived open-order requirement in the test market, right now.
    fn oo_im_alice(ctx: &mut TestCtx) -> u64 {
        let m = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
        let p = storage::load_position(ctx, ALICE, MARKET_ID).unwrap();
        position_open_order_margin(&m, &p).unwrap()
    }

    // ── 1. Cross-side orders are charged the MAX of the two terminal exposures ───────────────

    /// **The headline shape.** With orders resting on BOTH sides of the same market the
    /// requirement is `max(|N + Bid|, |N − Ask|)`: the larger of the two exposures the book could
    /// leave behind, not the sum of anything.
    ///
    /// Flat position, mark $100, leverage 1, resting BUY 2 lots @ $90 and SELL 1 lot @ $110:
    ///
    /// | quantity | value     | how |
    /// |----------|-----------|-----|
    /// | `Bid`    | 1_800_000 | 2 × 900_000 |
    /// | `Ask`    | 1_100_000 | 1 × 1_100_000 |
    /// | ooIM     | 1_800_000 | `max(\|0 + 1_800_000\|, \|0 − 1_100_000\|) − 0` |
    ///
    /// The retired escrow charged 2_000_000 here — `max(S + B', B + S')`, where `B' = 900_000` is
    /// the buy leg left over after one lot of it is consumed covering the short the sells would
    /// open. It modelled the two sides filling in SEQUENCE; Binance's formula does not, and the
    /// owner accepted that loosening. **The migration cut this book's requirement by 200_000
    /// (−10%).**
    #[test]
    fn a_cross_side_book_is_charged_the_larger_terminal_exposure() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx, PRICE);
        place(&mut ctx, ALICE, 0, P_LOW, 2);
        place(&mut ctx, ALICE, 1, P_HIGH, 1);

        let p = storage::load_position(&mut ctx, ALICE, MARKET_ID).unwrap();
        assert_eq!(
            (p.amount, p.total_buy_notional, p.total_sell_notional),
            (0, 1_800_000, 1_100_000)
        );

        assert_eq!(
            oo_im_alice(&mut ctx),
            1_800_000,
            "ooIM = max(|N+Bid|, |N-Ask|) - |N|/L"
        );
    }

    /// The same shape one lot smaller: BUY 1 lot @ $90, SELL 1 lot @ $110, flat ⇒
    /// `max(900_000, 1_100_000) = 1_100_000`. The escrow happened to agree here
    /// (`max(1_100_000 + 0, 900_000 + 0)`), so the boundary of the old divergence set is recorded
    /// and not just its interior.
    #[test]
    fn cross_side_one_for_one_is_the_dearer_side() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx, PRICE);
        place(&mut ctx, ALICE, 0, P_LOW, 1);
        place(&mut ctx, ALICE, 1, P_HIGH, 1);
        assert_eq!(oo_im_alice(&mut ctx), 1_100_000);
    }

    // ── 2. The requirement RE-VALUES on every mark move ──────────────────────────────────────

    /// **The property the escrow structurally could not have.** `ooIM` values the position leg at
    /// the CURRENT mark on every read, so the requirement moves when the mark moves — with no
    /// order, no fill, and no action by the user. The escrow was decided once at placement and
    /// never revisited (it was computed in QUANTITY space at the orders' LIMIT prices; the mark
    /// was not an input to it at all), so it sat frozen at 2_200_000 across this whole walk.
    ///
    /// Long 1 lot, resting SELL 3 lots @ $110, leverage 1. Nothing has traded in this market, so the
    /// Assuming-Price floor at PLACEMENT was `T = max(ROUND_UP(0 × 1.0015), $100) = $100`, below the
    /// $110 limit — so each sell froze at its own $110 and `Ask = 3_300_000` for the whole walk.
    /// **Only `N` moves:**
    ///
    /// | mark | `N`       | `Ask` (frozen) | `max(\|N+Bid\|, \|N−Ask\|)` | PIM       | ooIM      | (old escrow) |
    /// |------|-----------|----------------|---------------------------|-----------|-----------|--------------|
    /// | $100 | 1_000_000 |      3_300_000 | 2_300_000 (ask branch)    | 1_000_000 | 1_300_000 |    2_200_000 |
    /// | $150 | 1_500_000 |      3_300_000 | 1_800_000 (ask branch)    | 1_500_000 |   300_000 |    2_200_000 |
    /// | $200 | 2_000_000 |      3_300_000 | 2_000_000 (BID branch)    | 2_000_000 |         0 |    2_200_000 |
    ///
    /// ⚠️ **RE-DERIVED for R12 (was `1_300_000 → 1_500_000 → 2_000_000`).** The old numbers came
    /// from re-resolving `T` on every read, so as the mark rose past $110 the sells were repriced AT
    /// THE MARK and the ask branch grew with it forever —「a sell 3× the position never becomes
    /// free」. R12 measured that a resting order is NOT repriced (90 frames, 1939 quanta), so the
    /// growing long now does swallow the frozen sells until they are pure risk reduction and free.
    /// The walk that this file previously called "wrong" is the measured one.
    ///
    /// That staleness is the exposure the docs name and accept: 「托管会变陈旧」 — rest a sell, let
    /// the market run, and its term still reflects the old print. Note the shape's own limit: a sell
    /// resting at $110 with the mark at $200 is deep inside the book and would not survive as a
    /// resting order in a live market; this fixture reaches it only because `save_mark_price` moves
    /// the mark without running the matcher.
    ///
    /// **The property under test is unchanged and is the point of the test: ooIM moves on the mark
    /// alone.** It just moves DOWN here rather than up, and the row-to-row differences are what the
    /// assertion pins.
    #[test]
    fn a_mark_move_reprices_the_requirement_with_no_order_and_no_fill() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx, PRICE);
        seed_position(&mut ctx, ALICE, 1, 1);
        place(&mut ctx, ALICE, 1, P_HIGH, 3);

        // The frozen basis, stated once: nothing below re-derives it.
        assert_eq!(
            storage::load_position(&mut ctx, ALICE, MARKET_ID)
                .unwrap()
                .total_sell_notional,
            3_300_000,
            "3 lots frozen at their own $110 limit (T was $100 at placement)"
        );

        let mut seen = Vec::new();
        for mark in [PRICE, 150 * TICK, 200 * TICK] {
            storage::save_mark_price(&mut ctx, MARKET_ID, mark).unwrap();
            seen.push(oo_im_alice(&mut ctx));
        }
        assert_eq!(
            seen,
            vec![1_300_000, 300_000, 0],
            "ooIM 1_300_000 -> 300_000 -> 0 on the mark alone: `N` is live even though `Ask` is not"
        );
        assert!(
            seen[0] != seen[1] && seen[1] != seen[2],
            "the requirement must genuinely MOVE with the mark — a frozen `Ask` must not have \
             frozen `ooIM` (R10)"
        );
        assert_eq!(
            storage::load_position(&mut ctx, ALICE, MARKET_ID)
                .unwrap()
                .total_sell_notional,
            3_300_000,
            "and `Ask` is byte-identical after the whole walk"
        );
    }

    /// The re-valuation runs BOTH ways: a mark move AGAINST the position inflates the
    /// requirement. Same position and book, mark crashed to $10: `N = 100_000`, ask branch
    /// `|100_000 − 3_300_000| = 3_200_000`, PIM `100_000` ⇒ ooIM `3_100_000` — 900_000 MORE than
    /// the escrow's frozen 2_200_000. The migration is not a uniform loosening; it is a different
    /// function, stricter in some states and looser in others.
    #[test]
    fn an_adverse_mark_raises_the_requirement_above_what_the_escrow_held() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx, PRICE);
        seed_position(&mut ctx, ALICE, 1, 1);
        place(&mut ctx, ALICE, 1, P_HIGH, 3);
        storage::save_mark_price(&mut ctx, MARKET_ID, 10 * TICK).unwrap();
        assert_eq!(oo_im_alice(&mut ctx), 3_100_000);
    }

    // ── 3. mark == 0 is a TEST artefact, and it is worth knowing which is which ──────────────

    /// A zero mark makes `N = 0`, so the derived basis cannot see the position at all and a
    /// risk-reducing sell reads as a naked one. Same state, two marks. Long 2 lots, resting SELL
    /// 1 lot @ $110:
    /// * mark 0    ⇒ `N = 0`         ⇒ ooIM `1_100_000` — the artefact.
    /// * mark $100 ⇒ `N = 2_000_000` ⇒ `max(2_000_000, 900_000) = 2_000_000 = PIM` ⇒ ooIM `0`.
    ///
    /// Production cannot reach the first row (`addMarket` rejects a zero mark and every mark
    /// component is floored away from zero), so "the derived basis punishes risk-reducing orders"
    /// is NOT a real finding. Recording it here stops it being rediscovered as one — and is why
    /// several tests elsewhere in this file had to start setting a mark.
    #[test]
    fn the_pure_reduce_charge_is_a_zero_mark_artefact_only() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx, 0);
        seed_position(&mut ctx, ALICE, 2, 1);
        place(&mut ctx, ALICE, 1, P_HIGH, 1);
        assert_eq!(
            oo_im_alice(&mut ctx),
            1_100_000,
            "mark 0: the basis is blind to the long"
        );

        storage::save_mark_price(&mut ctx, MARKET_ID, PRICE).unwrap();
        assert_eq!(oo_im_alice(&mut ctx), 0, "mark $100: a pure reduce is free");
    }

    // ── 4. A position that flips sign under a two-sided book ────────────────────────────────

    /// `ooIM` as a function of the position, with the book held fixed. Book: BUY 2 lots @ $90
    /// (`Bid = 1_800_000`) and SELL 2 lots @ $110 (`Ask = 2_200_000`), mark $100, leverage 1,
    /// position walked from +2 lots to −2 lots:
    ///
    /// | position | `N`        | ooIM      | (old escrow) |
    /// |----------|------------|-----------|--------------|
    /// | +2 lots  |  2_000_000 | 1_800_000 |    1_800_000 |
    /// | +1 lot   |  1_000_000 | 1_800_000 |    2_000_000 |
    /// |  flat    |          0 | 2_200_000 |    2_200_000 |
    /// | −1 lot   | −1_000_000 | 2_200_000 |    2_200_000 |
    /// | −2 lots  | −2_000_000 | 2_200_000 |    2_200_000 |
    ///
    /// It is a STEP function: `Bid` while the position is long enough for the bid branch to win,
    /// `Ask` once the ask branch takes over. The escrow tracked the same two plateaus but bulged
    /// 200_000 above them at +1 lot — the cross-side residual of test 1, appearing wherever the
    /// position only PARTIALLY covers one side of a two-sided book. The flip itself was never the
    /// divergence source; that one mechanism was.
    #[test]
    fn the_requirement_is_a_step_function_of_the_position_under_a_two_sided_book() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx, PRICE);
        place(&mut ctx, ALICE, 0, P_LOW, 2);
        place(&mut ctx, ALICE, 1, P_HIGH, 2);

        let m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
        let mut walk = Vec::new();
        for lots in [2i64, 1, 0, -1, -2] {
            // Move ONLY `amount`; the book and its aggregates stay exactly as placed, so the
            // position sign is the single independent variable.
            let mut p = storage::load_position(&mut ctx, ALICE, MARKET_ID).unwrap();
            p.amount = lots * QTY as i64;
            walk.push(position_open_order_margin(&m, &p).unwrap());
        }
        assert_eq!(
            walk,
            vec![1_800_000, 1_800_000, 2_200_000, 2_200_000, 2_200_000],
            "Bid while the long dominates, then Ask — one step, no bulge"
        );
    }

    // ── 5. The tier table does NOT enter the requirement ────────────────────────────────────

    /// Phase 1 briefly enforced a TIER-CAPPED ooIM, which re-priced the position leg whenever the
    /// COMBINED notional crossed a tier boundary. It was rejected: open orders are priced at the
    /// position's own leverage, uncapped, as Binance does (tiers still govern maintenance margin,
    /// which is continuous). This test pins the consequence on the exact fixture that used to
    /// show a 10× discontinuity.
    ///
    /// Tiers `[(0, 10x), (2_000_000, 2x)]`, position long 1 lot at leverage 10, mark $100
    /// (`N = 1_000_000`):
    ///
    /// | resting BUY  | `Bid`     | combined  | ooIM    | (tier-capped variant) |
    /// |--------------|-----------|-----------|---------|-----------------------|
    /// | 1 lot @ $90  |   900_000 | 1_900_000 |  90_000 |                90_000 |
    /// | 2 lots @ $90 | 1_800_000 | 2_800_000 | 180_000 |               900_000 |
    ///
    /// The capped variant dropped `L_eff` 10 → 2 on the second row, which re-priced the
    /// PRE-EXISTING position as well as the new order and multiplied the requirement by 10 for
    /// one extra lot. The surviving definition is linear in `Bid`: 90_000 → 180_000.
    #[test]
    fn the_tier_table_does_not_enter_the_open_order_requirement() {
        let tiers = MarginTiers::from_tiers(&[
            MarginTier {
                lower_bound_notional: 0,
                max_leverage: 10,
            },
            MarginTier {
                lower_bound_notional: 2_000_000,
                max_leverage: 2,
            },
        ])
        .unwrap();
        for (lots, want) in [(1u64, 90_000u64), (2, 180_000)] {
            let mut ctx = make_ctx();
            storage::save_admin(&mut ctx, ADMIN).unwrap();
            storage::save_market(&mut ctx, &market_with(PRICE, tiers)).unwrap();
            fund(&mut ctx, ALICE, WALLET * 100);
            seed_position(&mut ctx, ALICE, 1, 10);
            place(&mut ctx, ALICE, 0, P_LOW, lots);
            assert_eq!(oo_im_alice(&mut ctx), want, "{lots} lot(s) resting");
        }
    }

    // ── 6. The account-level fold, and the available it produces ────────────────────────────

    /// The Σ walker agrees with the per-market numbers, and `available` is
    /// `perp_wallet_balance − Σ ooIM` with NO gross-up term.
    ///
    /// Phase 1 needed one (`+ Σ margin_reserved`) because the escrow had already been debited
    /// from the wallet, so subtracting Σ ooIM on top would have charged the book twice. Nothing
    /// is debited now: the funded balance IS the gross, and adding anything back would re-open
    /// that double-count from the other side. This test is the pin on that — it is the single
    /// easiest thing to get backwards in this migration.
    ///
    /// Flat, mark $100, BUY 2 lots @ $90 + SELL 1 lot @ $110 (the cross-side case of test 1):
    /// ooIM is 1_800_000, the wallet is untouched, and available is funded − 1_800_000.
    #[test]
    fn the_derived_available_subtracts_the_requirement_exactly_once() {
        let mut ctx = make_ctx();
        setup_marked(&mut ctx, PRICE);
        let funded = storage::load_account(&mut ctx, ALICE)
            .unwrap()
            .perp_wallet_balance;
        place(&mut ctx, ALICE, 0, P_LOW, 2);
        place(&mut ctx, ALICE, 1, P_HIGH, 1);

        let wallet = storage::load_account(&mut ctx, ALICE)
            .unwrap()
            .perp_wallet_balance;
        assert_eq!(wallet, funded, "the escrow is gone — nothing was debited");
        assert_eq!(
            total_open_order_initial_margin(&mut ctx, ALICE, None).unwrap(),
            1_800_000
        );

        let available = crate::margin_view::derived_available_balance(&mut ctx, ALICE).unwrap();
        assert_eq!(available, funded as i128 - 1_800_000);
        // The trap, stated numerically: grossing the (nonexistent) escrow back up, as Phase 1
        // correctly did, would now over-report by exactly the requirement.
        assert_ne!(available, funded as i128);
    }

    /// A user active in SEVERAL markets folds every one of them, and a market they have left
    /// contributes nothing (it is not in the index, and its ooIM would be 0 anyway).
    #[test]
    fn the_sum_spans_every_market_in_the_index_and_only_those() {
        let mut ctx = make_ctx();
        storage::save_admin(&mut ctx, ADMIN).unwrap();
        for id in 1..=3u64 {
            let mut m = market_with(PRICE, MarginTiers::default());
            m.market_id = id;
            storage::save_market(&mut ctx, &m).unwrap();
        }
        fund(&mut ctx, ALICE, WALLET * 100);

        let mut expect = 0u128;
        for id in 1..=3u64 {
            let input = placeOrderCall {
                marketId: id,
                side: 0,
                price: P_LOW,
                quantity: QTY * id,
                orderType: 0,
                tif: 3,
                clientOrderId: FixedBytes::default(),
            }
            .abi_encode();
            run_place_order(&input, ALICE, &mut ctx).unwrap();
            expect += 900_000u128 * id as u128;
        }
        assert_eq!(
            storage::load_user_markets(&mut ctx, ALICE).unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(
            total_open_order_initial_margin(&mut ctx, ALICE, None).unwrap(),
            expect
        );
        assert_eq!(expect, 5_400_000, "900_000 × (1 + 2 + 3)");
    }
}

// ── The Assuming Price: what a resting SELL is actually charged ──────────────────────────────
//
// `misc/binance-flip-and-admission.md` §1.6b / §3.4 / §3.7 and `misc/binance-margin-verified-model.md`
// §1.6 (its 2026-08-18 correction block, lines ~38-55).
//
// ⚠️ Do NOT cite that file's §1.5 for this. Its ⚠ still asserts the OPPOSITE — that
// `openOrderInitialMargin` is priced at the limit price, "实测逐位相等". That measurement was taken
// with the sells resting 4.3% ABOVE mark, where `limit == Assuming`, so it does not discriminate
// between the two rules; the §1.6 correction block supersedes it. The two sections of that file
// currently disagree, and a reader who follows the stale one will conclude this code is wrong.
//
// A SHORT order's margin is priced at
//
//     Assuming Price = max(Last Price × 1.0015, Mark, order price)
//
// not at its own limit price (a LONG order carries no markup at all). Measured on mainnet twice
// over: run9 refused eight admission probes exactly where the plateau `q × C / L` predicts and the
// limit-price model predicted acceptance (strongest by 17× the noise floor), and R10 read Binance's
// own reported `askNotional / q == limit × 1.0015` for a sell resting below `C`.
mod assuming_price {
    use super::*;
    use crate::interface::IPerpDex::{liquidateCall, setLeverageCall};

    /// The probe market: `base_decimals 5`, `price_decimals 2`, tick 10, step 1 — so
    /// `calc_value(price, qty) == price × qty / 10` and every number below is exact.
    fn setup_probe(ctx: &mut TestCtx) {
        storage::save_admin(ctx, ADMIN).unwrap();
        storage::save_market(
            ctx,
            &Market {
                market_id: MARKET_ID,
                base_decimals: 5,
                price_decimals: 2,
                tick_size: 10,
                step_size: 1,
                min_quantity: 1,
                max_quantity: 1_000_000,
                max_price: 100_000_000,
                price_update_interval: 15,
                active: true,
                funding_interval: 0,
                interest_rate: 0,
                liquidation_fee_rate_bps: 0,
                price_band_bps: 0,
                mark_price: 100_000,
                tiers: MarginTiers::default(),
            },
        )
        .unwrap();
    }

    /// ALICE long 6 @ $1000 at leverage 3 (margin 20_000, wallet 0), mark then dropped to 85_000
    /// with the position still comfortably above maintenance. Returns nothing — every caller
    /// continues from this state.
    fn alice_long_at_85k(ctx: &mut TestCtx) {
        setup_probe(ctx);
        fund(ctx, BOB, 1_000_000);
        fund(ctx, ALICE, 20_000);
        crate::risk::run_set_leverage(
            &setLeverageCall {
                marketId: MARKET_ID,
                leverage: 3,
            }
            .abi_encode(),
            ALICE,
            ctx,
        )
        .unwrap();

        // BOB rests the ask, ALICE lifts it. The fill PRINTS at 100_000, which is what makes
        // `lastTraded` 100_000 for the rest of the test.
        place(ctx, BOB, 1, 100_000, 6, 0, 0);
        place(ctx, ALICE, 0, 100_000, 6, 0, 0);
        let p = pos(ctx, ALICE);
        assert_eq!(
            (p.amount, p.margin, p.v_quote_balance),
            (6, 20_000, -60_000)
        );
        assert_eq!(wallet(ctx, ALICE), 0, "the whole wallet funded the margin");
        assert_eq!(
            storage::load_last_traded_price(ctx, MARKET_ID).unwrap(),
            100_000
        );

        storage::save_mark_price(ctx, MARKET_ID, 85_000).unwrap();
        // Still ABOVE maintenance: N = 51_000, equity = 51_000 − 60_000 + 20_000 = 11_000 against
        // a requirement of 51_000/6 = 8_500. The liquidation attempt below is the proof — the whole
        // scenario would be vacuous if the position were liquidatable.
        assert!(
            crate::risk::run_liquidate(
                &liquidateCall {
                    user: ALICE,
                    marketId: MARKET_ID,
                }
                .abi_encode(),
                BOB,
                ctx,
            )
            .is_err(),
            "the position must still be above maintenance"
        );
    }

    /// **THE HOLE THIS CLOSES.** Selling exactly 2× the position mirrors the exposure, so at the
    /// LIMIT price `|N − Ask| == |N|` exactly and `IM == PIM ⇒ ooIM == 0`: a wallet of ZERO could
    /// rest an order that flips the position, for free.
    ///
    /// That band exists on Binance too — R7 got flipping sells accepted at `ooIM = 0E-8` — but
    /// there it is NOT SIMULTANEOUSLY FILLABLE: a sell has to sit near the touch to fill, and near
    /// the touch `Last × 1.0015` takes over the pricing and the markup bites. 「两者不可兼得」
    /// (`binance-flip-and-admission.md` §1.6c). With limit-price aggregates and no markup, ours was
    /// free AND fillable — a state Binance closes and we had opened. This is that state.
    ///
    /// ```text
    /// N = 51_000 (6 @ mark 85_000), leverage 3, PIM = ROUND_UP(51_000/3) = 17_000
    /// T = max(ROUND_UP(100_000 × 1.0015), 85_000) = max(100_150, 85_000) = 100_150
    ///
    /// at the LIMIT price      Ask = 12 × 85_000 /10 = 102_000 = 2|N| ⇒ IM = 17_000 ⇒ ooIM = 0
    /// at the ASSUMING price   Ask = 12 × 100_150/10 = 120_180        ⇒ IM = ROUND_UP(69_180/3)
    ///                                                                    = 23_060 ⇒ ooIM = 6_060
    /// ```
    #[test]
    fn a_sell_of_twice_the_position_is_no_longer_free() {
        // ── (a) at wallet 0 the order is now REFUSED ──
        let mut ctx = make_ctx();
        alice_long_at_85k(&mut ctx);
        assert_eq!(available(&mut ctx, ALICE), 0);
        let err = place_or_err(&mut ctx, ALICE, 1, 85_000, 12).unwrap_err();
        assert!(
            err.to_string()
                .contains("insufficient perp wallet for margin"),
            "{err}"
        );
        assert_eq!(oo_im(&mut ctx, ALICE), 0, "nothing rested");

        // ── (b) the requirement is EXACTLY 6_060: refused at 6_059, accepted at 6_060 ──
        let mut ctx = make_ctx();
        alice_long_at_85k(&mut ctx);
        fund(&mut ctx, ALICE, 6_059);
        assert!(place_or_err(&mut ctx, ALICE, 1, 85_000, 12).is_err());
        fund(&mut ctx, ALICE, 1);
        place(&mut ctx, ALICE, 1, 85_000, 12, 0, 0);

        // ── (c) and once it rests, the reported numbers are the Assuming-Price ones ──
        assert_eq!(oo_im(&mut ctx, ALICE), 6_060);
        assert_eq!(available(&mut ctx, ALICE), 0, "6_060 of 6_060 committed");
        let p = pos(&mut ctx, ALICE);
        assert_eq!(
            p.total_sell_notional, 120_180,
            "the maintained aggregate IS `Ask`: the markup is frozen INTO the entry at placement, \
             so there is no second (limit-price) basis to keep"
        );
        let i = margin_info(&mut ctx, ALICE);
        assert_eq!(
            i.askNotional, p.total_sell_notional,
            "and the reported `Ask` is that aggregate read back, not a re-fold"
        );
        assert_eq!(i.askNotional, 120_180);
        assert_eq!(
            storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID).unwrap()[0].price,
            85_000,
            "while the entry's LIMIT price — what matching, sorting and the fee key on — is \
             untouched at 85_000"
        );
        assert_eq!((i.notional, i.positionInitialMargin), (51_000, 17_000));
        assert_eq!(i.initialMargin, 23_060);
        assert_eq!(i.openOrderInitialMargin, 6_060);
    }

    /// The BOUNDARY, so the interior above is not the only thing recorded: a sell resting ABOVE
    /// `T` freezes at its own limit price, the markup contributes nothing, and `Ask` equals the
    /// limit-price fold — the one case where the two bases coincide.
    ///
    /// Same position, sell 12 @ 150_000 (well above `T = 100_150`):
    /// `Ask = 180_000`, `IM = ROUND_UP(|51_000 − 180_000| / 3) = 43_000`, `ooIM = 26_000`.
    #[test]
    fn a_sell_resting_above_the_assuming_floor_pays_exactly_its_own_notional() {
        let mut ctx = make_ctx();
        alice_long_at_85k(&mut ctx);
        fund(&mut ctx, ALICE, 26_000);
        place(&mut ctx, ALICE, 1, 150_000, 12, 0, 0);

        let p = pos(&mut ctx, ALICE);
        let i = margin_info(&mut ctx, ALICE);
        assert_eq!(p.total_sell_notional, 180_000);
        assert_eq!(
            i.askNotional, p.total_sell_notional,
            "above T the frozen price IS the limit price — no markup term at all"
        );
        assert_eq!(
            storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID).unwrap()[0].assuming_price,
            150_000,
            "and the frozen field records exactly that"
        );
        assert_eq!(i.initialMargin, 43_000);
        assert_eq!(oo_im(&mut ctx, ALICE), 26_000);
        assert_eq!(available(&mut ctx, ALICE), 0);
    }

    /// A resting BUY is never marked up (`Assuming Price = Order's Price` for a long order), so the
    /// bid aggregate and `bidNotional` agree to the unit even with the buy resting BELOW `T` —
    /// which is where a sell would be repriced. This is the asymmetry, pinned.
    #[test]
    fn the_buy_side_carries_no_markup() {
        let mut ctx = make_ctx();
        alice_long_at_85k(&mut ctx);
        // 6 @ 80_000 = 48_000 of notional; at leverage 3 the bid branch |N + Bid| = 99_000 gives
        // IM = ROUND_UP(99_000/3) = 33_000, so ooIM = 16_000. Priced at `T = 100_150` the same
        // order would have cost 60_090 of `Bid` and 3_030 more of ooIM.
        fund(&mut ctx, ALICE, 16_000);
        place(&mut ctx, ALICE, 0, 80_000, 6, 0, 0);

        let p = pos(&mut ctx, ALICE);
        let i = margin_info(&mut ctx, ALICE);
        assert_eq!(p.total_buy_notional, 48_000);
        assert_eq!(i.bidNotional, 48_000, "no markup on the buy side");
        assert_eq!(i.initialMargin, 33_000);
        assert_eq!(oo_im(&mut ctx, ALICE), 16_000);
        assert_eq!(available(&mut ctx, ALICE), 0);
    }

    /// **`bidNotional == Σ qty_i × P_i` EXACTLY** — the regression test
    /// `misc/binance-flip-and-admission.md` §3.8 names for its own correction
    /// (「`bidNotional` 应精确等于 `q_B × P_b`(与卖单侧对照,是 §3.7 那条更正的回归测试)」).
    ///
    /// The single-order case above cannot distinguish "no markup" from "a markup that happens to
    /// miss", so this walks MORE THAN ONE order and straddles `T` in both directions. The easy way
    /// to break the asymmetry is to apply the sell side's `max(T, price)` to the buy fold as well;
    /// that would silently lift the two below-`T` orders and leave the two others alone, which is
    /// exactly what the per-order identity below catches.
    ///
    /// `T = max(ROUND_UP(100_000 × 1.0015), 85_000) = 100_150`, and
    /// `calc_value(price, qty) == price × qty / 10` on the probe market (`tick_size` is 10, so
    /// every price below is a tick multiple):
    ///
    /// ```text
    ///  price     qty   vs T       own notional    at max(T, price)   at min(T, price)
    ///  80_000      6   BELOW            48_000          60_090             48_000
    /// 100_140      3   BELOW (1 tick)   30_042          30_045             30_042
    /// 100_150      4   == T             40_060          40_060             40_060
    /// 110_000      5   ABOVE            55_000          55_000             50_075
    ///                                 ────────        ────────           ────────
    ///                        Bid    =  173_102         185_195            168_177
    ///                                    ↑ the only right answer
    /// ```
    ///
    /// `N = 51_000`, `L = 3` ⇒ bid branch `|51_000 + 173_102| = 224_102`,
    /// `IM = ROUND_UP(224_102/3) = 74_701`, `ooIM = 74_701 − 17_000 = 57_701`.
    #[test]
    fn the_bid_notional_is_exactly_quantity_times_each_orders_own_price() {
        let mut ctx = make_ctx();
        alice_long_at_85k(&mut ctx);
        fund(&mut ctx, ALICE, 1_000_000);

        // The three high bids are above the upper band edge at the $85_000 mark, so they can only
        // be REACHED by mark drift: a bid above the upper edge is a too-good quote and — bids
        // ranking better-first — necessarily the best bid, which placement now refuses. So place
        // them at a mark that contains them and then move the mark back.
        //
        // 100_000 is chosen, not arbitrary: it is the LOWEST mark whose upper edge (110_000)
        // contains the top bid, and it is still <= T, so T = max(ROUND_UP(lastTraded x 1.0015),
        // mark) = 100_150 is the SAME pivot the table above is written against. `save_mark_price`
        // rather than an oracle update, because an oracle update would run the band-expiry sweep.
        storage::save_mark_price(&mut ctx, MARKET_ID, 100_000).unwrap();
        let (upper_at_placement, _) = crate::math::mark_band_bounds(100_000, 0);
        assert_eq!(
            upper_at_placement, 110_000,
            "precondition: the placement band contains every bid below, top one inclusive"
        );
        assert_eq!(
            storage::load_last_traded_price(&mut ctx, MARKET_ID).unwrap(),
            100_000,
            "precondition: T's other input is unchanged, so T is still 100_150"
        );

        // Placed out of price order on purpose; the buy list is maintained price-DESCENDING.
        let orders = [(80_000u64, 6u64), (110_000, 5), (100_150, 4), (100_140, 3)];
        for (price, qty) in orders {
            place(&mut ctx, ALICE, 0, price, qty, 0, 0);
        }

        // DRIFT BACK: every number asserted below (N = 51_000 and everything derived from it) is on
        // the $85_000 basis this scenario is built around.
        storage::save_mark_price(&mut ctx, MARKET_ID, 85_000).unwrap();
        let (upper, lower) = crate::math::mark_band_bounds(85_000, 0);
        for high in [100_140u128, 100_150, 110_000] {
            assert!(
                high > upper,
                "the bid at {high} is now OUTSIDE the band [{lower}, {upper}] — this fixture's \
                 book is unreachable by placement and only the drift above gets it there"
            );
        }

        let buys = storage::load_buy_orders(&mut ctx, ALICE, MARKET_ID).unwrap();
        assert_eq!(
            buys.iter().map(|e| e.price).collect::<Vec<_>>(),
            vec![110_000, 100_150, 100_140, 80_000],
            "all four rested, the buy list is price-descending, and it straddles T = 100_150 both \
             ways"
        );
        assert_eq!(
            buys.iter().map(|e| e.assuming_price).collect::<Vec<_>>(),
            vec![110_000, 100_150, 100_140, 80_000],
            "precondition: every buy entry froze its OWN price as its Assuming Price — the two \
             below T prove the straddle is real and that T did not collapse onto the prices"
        );

        // The identity, recomputed here from the order terms alone — deliberately NOT by calling an
        // engine helper, so this is a cross-check rather than a tautology.
        let want: u64 = orders.iter().map(|&(p, q)| p * q / 10).sum();
        assert_eq!(want, 48_000 + 55_000 + 40_060 + 30_042);
        assert_eq!(want, 173_102);

        let p = pos(&mut ctx, ALICE);
        let i = margin_info(&mut ctx, ALICE);
        assert_eq!(
            i.bidNotional, want,
            "bidNotional must be Σ qty × the order's OWN price, with no uplift anywhere"
        );
        assert_eq!(
            p.total_buy_notional, i.bidNotional,
            "for the buy side the maintained aggregate IS `Bid` — there is no second basis"
        );

        // Both plausible wrong rules are named, so a regression cannot pass by coincidence. The two
        // orders BELOW `T` are what the sell side's `max(T, price)` would lift; the one ABOVE `T` is
        // what a `min(T, price)` cap would lower. Neither may touch the buy fold.
        let floored_at_t: u64 = orders.iter().map(|&(p, q)| p.max(100_150) * q / 10).sum();
        let capped_at_t: u64 = orders.iter().map(|&(p, q)| p.min(100_150) * q / 10).sum();
        assert_eq!(floored_at_t, 185_195);
        assert_eq!(capped_at_t, 168_177);
        assert_ne!(
            i.bidNotional, floored_at_t,
            "the sell side's max(T, price) must NOT be applied to buys"
        );
        assert_ne!(i.bidNotional, capped_at_t, "and neither must a cap at T");
        assert_eq!(
            floored_at_t - i.bidNotional,
            12_093,
            "the exact amount the sell-side rule would have over-charged the buy side"
        );

        // ...and the requirement that follows from it.
        assert_eq!((i.notional, i.positionInitialMargin), (51_000, 17_000));
        assert_eq!(i.initialMargin, 74_701);
        assert_eq!(oo_im(&mut ctx, ALICE), 57_701);
    }

    /// The markup is PER ORDER — each entry freezes its own `max(T, price)`, not a single adjustment
    /// to the total — and the sell list stays ASCENDING BY LIMIT PRICE, which is what the insert
    /// predicate `partition_point(|e| e.price < price)` maintains and what matching, price priority
    /// and `cancel_same_side_orders_until_wallet_covers`' `.back()` depend on. The markup no longer
    /// walks that order (it is frozen per entry at placement), so this test also pins that the sort
    /// is keyed on `price` and NOT on the marked-up `assuming_price` — under which the list below
    /// would come out `[100_150, 100_150, 100_150, 150_000]` and the first three would be
    /// interchangeable.
    ///
    /// Four sells of 4 straddling `T = 100_150`:
    ///
    /// ```text
    ///  limit    frozen assuming price
    ///  90_000 → 100_150 (lifted to T)     100_000 → 100_150 (lifted to T)
    /// 100_150 → 100_150 (exactly T)       150_000 → 150_000 (above T)
    ///
    /// Ask = 4·(100_150 + 100_150 + 100_150 + 150_000) / 10 = 180_180   ← the aggregate, frozen
    /// ooIM = ROUND_UP(|51_000 − 180_180| / 3) − 17_000 = 43_060 − 17_000 = 26_060
    ///        (at the limit price it would have been 41_687 − 17_000 = 24_687)
    /// ```
    #[test]
    fn the_markup_is_frozen_per_order_and_never_reorders_the_list() {
        let mut ctx = make_ctx();
        alice_long_at_85k(&mut ctx);
        fund(&mut ctx, ALICE, 1_000_000);
        // Placed out of order on purpose: the list must end up ascending regardless.
        for price in [150_000u64, 90_000, 100_150, 100_000] {
            place(&mut ctx, ALICE, 1, price, 4, 0, 0);
        }
        let sells = storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID).unwrap();
        assert_eq!(
            sells.iter().map(|e| e.price).collect::<Vec<_>>(),
            vec![90_000, 100_000, 100_150, 150_000],
            "ascending by LIMIT price — the sort matching and `.back()` rely on"
        );
        assert_eq!(
            sells.iter().map(|e| e.assuming_price).collect::<Vec<_>>(),
            vec![100_150, 100_150, 100_150, 150_000],
            "each entry froze its own max(T, limit) — three of them collide, which is exactly why \
             the sort must not key on this field"
        );

        let p = pos(&mut ctx, ALICE);
        assert_eq!(
            p.total_sell_notional, 180_180,
            "the aggregate IS `Ask`, at the frozen prices"
        );
        assert_eq!(margin_info(&mut ctx, ALICE).askNotional, 180_180);
        assert_eq!(
            oo_im(&mut ctx, ALICE),
            26_060,
            "1_373 more than the 24_687 the limit-price basis would charge"
        );
    }

    /// `lastTraded == 0` (nothing has ever printed) collapses `T` to the mark — the `Mark` branch
    /// of the vendor `max()`, so no separate fallback is needed. A sell above the mark is then
    /// unmarked, and one below it is charged AT THE MARK.
    #[test]
    fn before_the_first_trade_the_floor_is_the_mark() {
        let mut ctx = make_ctx();
        setup_probe(&mut ctx);
        fund(&mut ctx, ALICE, 1_000_000);
        assert_eq!(
            storage::load_last_traded_price(&mut ctx, MARKET_ID).unwrap(),
            0,
            "nothing has traded"
        );

        // Above the 100_000 mark: unmarked.
        place(&mut ctx, ALICE, 1, 110_000, 3, 0, 0);
        assert_eq!(margin_info(&mut ctx, ALICE).askNotional, 33_000);

        // A second sell BELOW the mark is lifted to the mark: 3 × 100_000/10 = 30_000 instead of
        // 3 × 90_000/10 = 27_000.
        place(&mut ctx, ALICE, 1, 90_000, 3, 0, 0);
        assert_eq!(margin_info(&mut ctx, ALICE).askNotional, 33_000 + 30_000);
        assert_eq!(
            pos(&mut ctx, ALICE).total_sell_notional,
            33_000 + 30_000,
            "the aggregate carries the frozen lift, and is `Ask` itself"
        );
        assert_eq!(
            storage::load_sell_orders(&mut ctx, ALICE, MARKET_ID)
                .unwrap()
                .iter()
                .map(|e| (e.price, e.assuming_price))
                .collect::<Vec<_>>(),
            vec![(90_000, 100_000), (110_000, 110_000)],
            "the below-mark sell froze at the mark; the above-mark one at its own limit"
        );
    }

    /// **R12, the freeze itself.** Once a sell rests, its `Ask` term never moves again — not when
    /// the mark walks, not when a later trade prints, not when the floor crosses its own limit
    /// price. `misc/binance-flip-and-admission.md` §3.13: 3 trials × 30 frames × 2 s, the reported
    /// `askNotional` never moved while `Last` walked 32.80 USD, with **9 consecutive frames below
    /// the `P_s / 1.0015` kink** where a recomputed-at-read value has to plateau at `q × P_s`.
    /// `H_live` was refused by 1939 quanta — a SHAPE-level refutation, not a slope-level one.
    ///
    /// This is the on-chain analogue of that walk, and it is built to be discriminating in the same
    /// way: a sell rests at 85_000 with `T = 100_150`, freezing at 100_150, and then the mark is
    /// walked from 85_000 all the way to 200_000 — right through and far past the frozen price. A
    /// re-resolving implementation would report `Ask = 12 × mark / 10` on the later frames
    /// (240_000 at mark 200_000); the frozen one reports 120_180 on every frame.
    ///
    /// ⚠️ And the other half, which must NOT be broken by the freeze: `ooIM` still MOVES, because
    /// `N = |qty| × mark` is live. Both are asserted here so neither can be "fixed" into the other.
    #[test]
    fn a_resting_sells_ask_term_is_frozen_while_ooim_still_moves_with_the_mark() {
        let mut ctx = make_ctx();
        alice_long_at_85k(&mut ctx);
        fund(&mut ctx, ALICE, 6_060);
        place(&mut ctx, ALICE, 1, 85_000, 12, 0, 0);
        // Frozen at T = max(ROUND_UP(100_000 × 1.0015), 85_000) = 100_150 ⇒ 12 × 100_150/10.
        assert_eq!(margin_info(&mut ctx, ALICE).askNotional, 120_180);

        // Fund the walk so a rising requirement is never the thing that reverts (nothing writes
        // here anyway — `save_mark_price` is the only mutation).
        fund(&mut ctx, ALICE, 10_000_000);

        let mut asks = Vec::new();
        let mut oo_ims = Vec::new();
        for mark in [85_000u64, 95_000, 100_150, 120_000, 200_000] {
            storage::save_mark_price(&mut ctx, MARKET_ID, mark).unwrap();
            asks.push(margin_info(&mut ctx, ALICE).askNotional);
            oo_ims.push(oo_im(&mut ctx, ALICE));
        }

        // (a) THE FREEZE: byte-identical on every frame, including the three where the mark is at or
        //     above the frozen 100_150 and a live rule would have taken over.
        assert_eq!(
            asks,
            vec![120_180; 5],
            "a resting sell's Ask term must not move with the mark (R12)"
        );
        assert_ne!(
            asks.last(),
            Some(&240_000),
            "…and specifically not to 12 × mark / 10, which is what re-resolving T would give"
        );
        // Also unmoved by a fresh PRINT that lifts the floor past the frozen price: BOB and CAROL
        // trade at 200_000, so `T` becomes 200_300 — nothing to do with ALICE's resting order.
        fund(&mut ctx, CAROL, 10_000_000);
        place(&mut ctx, BOB, 0, 200_000, 1, 0, 0);
        place(&mut ctx, CAROL, 1, 200_000, 1, 0, 0);
        assert_eq!(
            storage::load_last_traded_price(&mut ctx, MARKET_ID).unwrap(),
            200_000,
            "the print landed"
        );
        assert_eq!(
            margin_info(&mut ctx, ALICE).askNotional,
            120_180,
            "a later print does not re-freeze an already-resting order"
        );

        // (b) `N` IS STILL LIVE, so ooIM moves — the frozen aggregate did not freeze the
        //     requirement. Long 6 at leverage 3 against a frozen Ask of 120_180:
        //       mark  85_000: N = 51_000, IM = ⌈(120_180−51_000)/3⌉ = 23_060, PIM = 17_000 ⇒  6_060
        //       mark  95_000: N = 57_000, IM = ⌈(120_180−57_000)/3⌉ = 21_060, PIM = 19_000 ⇒  2_060
        //       mark 100_150: N = 60_090, IM = ⌈(120_180−60_090)/3⌉ = 20_030, PIM = 20_030 ⇒      0
        //       mark 120_000: N = 72_000, bid branch |72_000| wins ⇒ IM = PIM              ⇒      0
        //       mark 200_000: N = 120_000, same                                            ⇒      0
        assert_eq!(oo_ims, vec![6_060, 2_060, 0, 0, 0]);
        assert!(
            oo_ims[0] != oo_ims[1],
            "ooIM must still re-value on a mark move with no user action (R10) — the freeze is \
             per-order, it is NOT 'the escrow is a constant' (that holds only at N = 0)"
        );
    }

    // ── local helpers ────────────────────────────────────────────────────────────────────────

    fn place_or_err(
        ctx: &mut TestCtx,
        caller: Address,
        side: u8,
        price: u64,
        qty: u64,
    ) -> Result<Bytes, PerpError> {
        let input = placeOrderCall {
            marketId: MARKET_ID,
            side,
            price,
            quantity: qty,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode();
        run_place_order(&input, caller, ctx)
    }

    /// The reported margin row for `(user, MARKET_ID)`, in ABI form.
    ///
    /// Goes through `AccountPositionRow::to_abi` — the ONE row encoder both `getPositionRisk` and
    /// `getAccount().positions[]` use. It used to hand-fill a `getMarginInfoReturn` field by field,
    /// which made it a fourth encoder of the same numbers living in a test module; when
    /// `getMarginInfo` was deleted that literal was the only thing still naming it.
    fn margin_info(
        ctx: &mut TestCtx,
        user: Address,
    ) -> crate::interface::IPerpDex::AccountPosition {
        crate::margin_view::AccountPositionRow {
            market_id: MARKET_ID,
            info: crate::margin_view::compute_margin_info(ctx, user, MARKET_ID).unwrap(),
        }
        .to_abi()
    }
}

// ── `AccountBalanceChanged`: the trigger filter and the per-transaction coalescing ────────────
//
// Two rules, both pinned here.
//
// **The trigger (choice A — strict Binance alignment).** A user is marked for a snapshot only at a
// write that moved the WALLET or a POSITION's stored state. An aggregates-only write — resting an
// order, cancelling one — marks nobody, so a pure placement and a pure cancel publish NOTHING. Not
// because nothing observable moved (`availableBalance = cross − Σ ooIM` really does move), but
// because we match a measured venue: R14 (Binance mainnet, 2026-08-21, 18 recorded frames) saw a
// pure placement push only `ORDER_TRADE_UPDATE x=NEW` and a pure cancel only `x=CANCELED`, with no
// `ACCOUNT_UPDATE` in either case, and the official docs say it verbatim — *"Unfilled orders or
// cancelled orders will not make the event `ACCOUNT_UPDATE` pushed, since there's no change on
// positions."* The accepted cost is that `availableBalance` goes stale in the stream between fills.
//
// **The granularity.** ONE snapshot per ECONOMIC EVENT: a filled maker gets one per FILL (published
// inside the match flush, from the registry working copy, right after that fill's `Trade`), a taker
// gets one per ORDER (after `finalize_apply`, off the settled store), and every other path — the
// non-trading writes plus the incidental fee recipient — keeps `websocket-implementation.md`
// Transaction-Level Coalescing #2, "one final snapshot per affected user per transaction": writes
// mark, `call::run_perp_dex_call` drains once, in ascending ADDRESS order, on its success path only.
//
// The transaction was the wrong unit for a fill because the transaction belongs to the TAKER: a
// 64-item batch filling maker M on items 3, 17 and 40 gave M ONE row, at the end of a transaction M
// never participated in, for three separate economic events. A direct emit therefore also CLEARS that
// user's mark, or the drain would repeat it.
//
// ⚠️ This decision has now flipped three times (silent → emitting in `7019fadd` → silent). The
// citation lives on `storage::mark_account_snapshot_dirty`; these tests are what make it enforced
// rather than merely written down.
mod account_snapshot_events {
    use super::*;
    use crate::interface::IPerpDex::{batchPlaceOrdersCall, setLeverageCall, PlaceItem, Trade};

    /// Every `AccountBalanceChanged` in the journal, in emission order.
    fn take_balance_events(ctx: &mut TestCtx) -> Vec<AccountBalanceChanged> {
        JournalTr::take_logs(ctx.journal_mut())
            .into_iter()
            .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
            .map(|log| {
                AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
            })
            .collect()
    }

    // `assert_account_update_groups` and `stream_shape` live in `crate::events::stream_test_support`
    // — the module that owns the invariant — because `risk::tests` needs them too (the liquidation
    // and ADL streams are asserted there, against the fixtures that already exist there).
    use crate::events::stream_test_support::{
        account_update_reasons, assert_account_update_groups, stream_shape,
    };

    // ── ORDERED-STREAM SHAPE: the assembled `ACCOUNT_UPDATE` pushes, not just counts ──────────
    //
    // The tests above assert WHO gets a snapshot and how many. These assert the exact ORDERED
    // `(event, subject)` sequence, which is the only form that can catch the failure this design
    // exists to prevent: a `PositionChanged` landing one slot too early (before its own header) or
    // one slot too late (after a foreign header) is invisible to a count and fatal to an indexer.

    /// **The two-fill sweep.** One taker crossing two DIFFERENT makers: each fill is its own
    /// `ACCOUNT_UPDATE` for that maker (`Trade` outside the group, header then row inside), and the
    /// taker gets one group for the whole order, last.
    ///
    /// Two distinct makers rather than one maker twice, so a header/row mismatch — the row from
    /// fill 2 sitting inside fill 1's group — is a DIFFERENT ADDRESS and not merely a duplicate.
    #[test]
    fn a_two_fill_sweep_publishes_one_group_per_fill_then_the_takers() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        fund(&mut ctx, CAROL, WALLET);
        // One ask each, same level → one sweep consumes both from one queue, BOB first (FIFO).
        place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell BOB");
        place_in(&mut ctx, CAROL, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell CAROL");

        let logs = crossing_order(&mut ctx, ALICE, 0, PRICE, 2 * QTY);
        assert_account_update_groups(&logs);
        assert_eq!(
            stream_shape(&logs),
            vec![
                ("OrderPlaced", Some(ALICE)),
                // fill 1: BOB's group, with the `Trade` OUTSIDE it
                ("Trade", Some(ALICE)),
                ("AccountBalanceChanged", Some(BOB)),
                ("PositionChanged", Some(BOB)),
                // fill 2: CAROL's group
                ("Trade", Some(ALICE)),
                ("AccountBalanceChanged", Some(CAROL)),
                ("PositionChanged", Some(CAROL)),
                // the taker: ONE group for the whole order
                ("AccountBalanceChanged", Some(ALICE)),
                ("PositionChanged", Some(ALICE)),
            ],
            "each fill is one ACCOUNT_UPDATE for its maker; the taker gets one for the order"
        );
        // …and every one of them reports `ORDER`, Binance's reason for a matched order moving money.
        // Measured R14: a market fill pushes exactly one `ACCOUNT_UPDATE(m=ORDER)`.
        assert_eq!(
            account_update_reasons(&logs),
            vec![
                (BOB, AccountUpdateReason::Order),
                (CAROL, AccountUpdateReason::Order),
                (ALICE, AccountUpdateReason::Order),
            ],
        );
    }

    /// **The funding-epoch rollover, non-registry path.** A rolled-over funding index plus an
    /// `addPositionMargin` is TWO economic events for one user in one call, so it is two pushes:
    /// the funding group, then the margin group. `FundingSettled` sits before the first header and
    /// `PositionMarginAdjusted` between the two, and both are group terminators — which is exactly
    /// why the funding header cannot be hoisted to the top of `apply_funding_settlement`.
    #[test]
    fn a_funding_rollover_publishes_its_own_group_before_the_margin_add_group() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        set_mark(&mut ctx, PRICE);

        // ALICE: QTY long at $100.
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

        // The epoch rolled over: a funding index a LONG owes against.
        storage::save_funding_state(
            &mut ctx,
            MARKET_ID,
            &FundingState {
                last_funding_rate: 1_000,
                next_funding_ts: u64::MAX,
                cumulative_funding_index: PRICE as i128 * 1_000,
            },
        )
        .unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());

        start_call(&mut ctx);
        crate::risk::run_add_position_margin(
            &crate::interface::IPerpDex::addPositionMarginCall {
                marketId: MARKET_ID,
                amount: 1,
            }
            .abi_encode(),
            ALICE,
            &mut ctx,
        )
        .unwrap();
        end_call(&mut ctx);

        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_account_update_groups(&logs);
        assert_eq!(
            stream_shape(&logs),
            vec![
                ("FundingSettled", Some(ALICE)),
                ("AccountBalanceChanged", Some(ALICE)),
                ("PositionChanged", Some(ALICE)),
                ("PositionMarginAdjusted", Some(ALICE)),
                ("AccountBalanceChanged", Some(ALICE)),
                ("PositionChanged", Some(ALICE)),
            ],
            "funding and the margin add are two economic events → two ACCOUNT_UPDATE pushes; the \
             drain adds nothing (the inline emit cleared the mark)"
        );
        // Two economic events, two DIFFERENT reasons — and this is the pair that shows the field
        // earning its keep: both rows are for the same user, both carry the same three balances'
        // shape, and only `reason` distinguishes "funding took money out of my silo" from "I moved
        // money into it myself". `MarginTransfer` is Binance's value for their *Modify Isolated
        // Position Margin*, which is what `addPositionMargin` is.
        assert_eq!(
            account_update_reasons(&logs),
            vec![
                (ALICE, AccountUpdateReason::FundingFee),
                (ALICE, AccountUpdateReason::MarginTransfer),
            ],
        );
    }

    /// **The funding-epoch rollover, REGISTRY path — the subtle one.** A maker's funding is settled
    /// at `MatchRegistry::get_or_load` and replayed by the flush BEFORE the flush writes the
    /// position, so a header folded from STORAGE at that point would report the pre-match silo. The
    /// payload is therefore captured from the working copy; this pins that it lands as its own
    /// group, ahead of the fill's `Trade` and the fill's own group.
    #[test]
    fn a_funding_rollover_on_the_match_path_groups_the_makers_funding_before_its_fill() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        set_mark(&mut ctx, PRICE);
        fund(&mut ctx, CAROL, WALLET);

        // BOB ends up SHORT QTY (he sold to ALICE), so funding can bite his silo.
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        assert_eq!(pos(&mut ctx, BOB).amount, -(QTY as i64));
        // …and rests a bid, which CAROL will cross: BOB is the MAKER of the next fill.
        place_in(&mut ctx, BOB, MARKET_ID, 0, PRICE, QTY, 3).expect("maker buy BOB");

        // The epoch rolled over with a NEGATIVE index, so the SHORT is the side that pays.
        storage::save_funding_state(
            &mut ctx,
            MARKET_ID,
            &FundingState {
                last_funding_rate: -1_000,
                next_funding_ts: u64::MAX,
                cumulative_funding_index: -(PRICE as i128) * 1_000,
            },
        )
        .unwrap();

        let logs = crossing_order(&mut ctx, CAROL, 1, PRICE, QTY);
        assert_account_update_groups(&logs);
        assert_eq!(
            stream_shape(&logs),
            vec![
                ("OrderPlaced", Some(CAROL)),
                // BOB's funding group, replayed from the registry at his first touch
                ("FundingSettled", Some(BOB)),
                ("AccountBalanceChanged", Some(BOB)),
                ("PositionChanged", Some(BOB)),
                // then the fill: `Trade` outside, BOB's fill group, then CAROL's order group
                ("Trade", Some(CAROL)),
                ("AccountBalanceChanged", Some(BOB)),
                ("PositionChanged", Some(BOB)),
                ("AccountBalanceChanged", Some(CAROL)),
                ("PositionChanged", Some(CAROL)),
            ],
            "the maker's funding is its own push, ahead of the fill it was settled for"
        );
        // The maker's funding push is `FUNDING_FEE`; his fill and the taker's order are `ORDER`.
        assert_eq!(
            account_update_reasons(&logs),
            vec![
                (BOB, AccountUpdateReason::FundingFee),
                (BOB, AccountUpdateReason::Order),
                (CAROL, AccountUpdateReason::Order),
            ],
        );
    }

    /// A second market, so a batch can touch one user across two of them.
    const MARKET_2: u64 = 2;

    fn add_market_2(ctx: &mut TestCtx) {
        let mut m = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
        m.market_id = MARKET_2;
        storage::save_market(ctx, &m).unwrap();
        storage::save_mark_price(ctx, MARKET_2, MARK).unwrap();
    }

    fn place_in(
        ctx: &mut TestCtx,
        caller: Address,
        market: u64,
        side: u8,
        price: u64,
        qty: u64,
        tif: u8,
    ) -> Result<Bytes, PerpError> {
        let input = placeOrderCall {
            marketId: market,
            side,
            price,
            quantity: qty,
            orderType: 0,
            tif,
            clientOrderId: FixedBytes::default(),
        }
        .abi_encode();
        run_place_order(&input, caller, ctx)
    }

    /// A LONG whose mark has moved off its entry, so `Σ uPnL`, `Σ isolatedWallet`, `Σ PIM` and
    /// `Σ maintMargin` are all non-zero — otherwise a field-for-field equality against `getAccount`
    /// would be an equality between rows of zeros.
    fn seed_long(ctx: &mut TestCtx, who: Address, market: u64, lots: i64) {
        let notional = lots * FILL_VALUE as i64;
        storage::save_position(
            ctx,
            who,
            market,
            &PerpPosition {
                amount: lots * QTY as i64,
                v_quote_balance: -notional,
                margin: notional.abs(),
                leverage: 1,
                ..PerpPosition::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
    }

    /// Mark $110 against a $100 entry: `N` and therefore PIM / MM / uPnL are all live.
    const MARK: u64 = PRICE + 10 * TICK;

    /// ALICE with a live long in the test market; journal and marks both drained.
    fn fixture(ctx: &mut TestCtx) {
        setup(ctx);
        storage::save_mark_price(ctx, MARKET_ID, MARK).unwrap();
        seed_long(ctx, ALICE, MARKET_ID, 3);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        start_call(ctx);
    }

    // ── The trigger filter: placement and cancel are SILENT ──────────────────────────────────

    /// **The headline.** A placement that only RESTS publishes no account snapshot, even though it
    /// moves `availableBalance`.
    ///
    /// The `start_call` / `end_call` pair matters: without the drain actually running, "no event"
    /// would hold for the trivial reason that `place_in` bypasses the shell. Here the drain runs and
    /// finds an EMPTY set, which is the property under test — `rest_in_book` writes the position
    /// through `save_position_reservation_only`, and that entry point marks nobody.
    #[test]
    fn a_resting_placement_publishes_no_account_snapshot() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);

        let available_before = get_account(&mut ctx, ALICE).availableBalance;
        let _ = JournalTr::take_logs(ctx.journal_mut());

        start_call(&mut ctx);
        place_in(&mut ctx, ALICE, MARKET_ID, 0, PRICE, QTY, 0).expect("resting buy");
        end_call(&mut ctx);

        assert!(
            take_balance_events(&mut ctx).is_empty(),
            "a pure placement must publish nothing — R14 + the official trigger sentence"
        );
        // And it really did move the number the event would have carried, so the silence is a
        // DECISION and not an absence of change. This is the stale-between-fills cost, pinned.
        let after = get_account(&mut ctx, ALICE);
        assert!(
            after.totalOpenOrderInitialMargin > 0,
            "the resting order must contribute a real ooIM term"
        );
        assert_eq!(
            after.availableBalance,
            available_before - after.totalOpenOrderInitialMargin as i64,
            "availableBalance fell by exactly the new order's requirement — unannounced"
        );
    }

    /// PostOnly never calls `match_order`, so `rest_in_book` is the only write path: still silent.
    #[test]
    fn a_post_only_placement_publishes_no_account_snapshot() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);

        start_call(&mut ctx);
        place_in(&mut ctx, ALICE, MARKET_ID, 0, PRICE, QTY, 3).expect("post-only buy");
        end_call(&mut ctx);

        assert!(take_balance_events(&mut ctx).is_empty());
    }

    /// A pure CANCEL publishes nothing either. `release_open_order_margin` shrinks `Bid`/`Ask` and
    /// nothing else, so it too takes the reservation-only write path — "nothing to release", the
    /// same Summary-Matrix row and the same official sentence as the placement.
    #[test]
    fn a_pure_cancel_publishes_no_account_snapshot() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let oo_im_before = get_account(&mut ctx, ALICE).totalOpenOrderInitialMargin;
        assert!(oo_im_before > 0, "the fixture must leave a real ooIM term");
        let _ = JournalTr::take_logs(ctx.journal_mut());

        start_call(&mut ctx);
        let out = run_perp_dex_call(
            &cancelOrderCall {
                orderId: id.into(),
                marketId: MARKET_ID,
            }
            .abi_encode(),
            10_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(!out.reverted);

        assert!(
            take_balance_events(&mut ctx).is_empty(),
            "a pure cancel must publish nothing"
        );
        assert_eq!(
            get_account(&mut ctx, ALICE).totalOpenOrderInitialMargin,
            0,
            "…while the requirement it released really is gone"
        );
    }

    /// **THE PAYLOAD/TRIGGER AGREEMENT.** A placement and a cancel cannot move ANY field the event
    /// publishes — so the two tests above ("they publish nothing") are not merely Binance mimicry,
    /// they are *consistent*: there is nothing to publish.
    ///
    /// This is the property the payload was narrowed for. The old eleven-field payload did NOT have
    /// it: `totalOpenOrderInitialMargin` and `availableBalance` move on every placement and every
    /// cancel, so the event shipped fields whose freshness its own trigger did not guarantee. The
    /// assertions below pin both halves — the three published balances are byte-identical across a
    /// placement and a cancel, and the numbers that DID move are `getAccount`-only.
    ///
    /// # Why this survives the per-event granularity intact
    ///
    /// It never depended on "the snapshot is emitted last" as a mechanism, only on the DRAIN existing
    /// for a pure account write — and it still does (rule 3: non-trading paths coalesce). The
    /// measurement is a probe in its own separate call, `mutate_account_balance(credit_perp(0))`, so
    /// what it reads is the state at the end of that probe call, never a row emitted by the placement
    /// or the cancel under test. The two calls under test remain PURE placement and PURE cancel — the
    /// order rests in `MARKET_2`, which has no liquidity, so neither becomes a taker and neither can
    /// reach the new per-fill or per-order emit points. That is what keeps "they publish nothing"
    /// checkable: it is still a statement about a stream with no money rows in it.
    ///
    /// The order is placed in a market ALICE holds NO position in, which is the subtle leg: an
    /// order-list transition flips per-user-market-index membership
    /// (`storage::sync_user_market_membership`) with no snapshot mark, so the placement ADDS a market
    /// to the set `Σ pos.margin` is summed over and the cancel DROPS it. That is only harmless
    /// because a flat position carries no margin. If a flat position could hold margin,
    /// `totalWalletBalance` would move here and this test is what would catch it.
    #[test]
    fn placement_and_cancel_cannot_move_any_published_field() {
        // Force one snapshot for `user` and return its whole payload. `credit_perp(0)` is a write
        // that MOVES NOTHING, so the payload is the current state verbatim — the same isolation
        // trick `margin_view_tests::the_event_and_get_account_agree_field_for_field_on_the_same_state`
        // uses.
        fn published(ctx: &mut TestCtx, user: Address) -> (U256, i64, i64) {
            let _ = JournalTr::take_logs(ctx.journal_mut());
            start_call(ctx);
            storage::mutate_account_balance(ctx, user, AccountUpdateReason::Adjustment, |a| {
                a.credit_perp(0)
            })
                .unwrap()
                .unwrap();
            end_call(ctx);
            let events = take_balance_events(ctx);
            assert_eq!(
                events.len(),
                1,
                "the probe must publish exactly one snapshot"
            );
            let e = &events[0];
            assert_eq!(e.user, user);
            (
                e.usdcBalance,
                e.totalWalletBalance,
                e.totalCrossWalletBalance,
            )
        }

        let mut ctx = make_ctx();
        fixture(&mut ctx);
        add_market_2(&mut ctx);
        // The fixture's long is in MARKET_ID; MARKET_2 is untouched, so the order below moves ALICE's
        // index membership. Both non-trivial fields are live: she has a wallet AND a funded silo.
        let before = published(&mut ctx, ALICE);
        assert!(
            before.1 != before.2 && before.2 != 0,
            "fixture must make both published balances non-trivial, got {before:?}"
        );
        assert!(
            !storage::load_user_markets(&mut ctx, ALICE)
                .unwrap()
                .contains(&MARKET_2),
            "MARKET_2 must start OUTSIDE the index for the membership leg to be exercised"
        );
        let available_before = get_account(&mut ctx, ALICE).availableBalance;

        // ── A placement ───────────────────────────────────────────────────────────────────────
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let out = run_perp_dex_call(
            &placeOrderCall {
                marketId: MARKET_2,
                side: 0,
                price: PRICE,
                quantity: QTY,
                orderType: 0,
                tif: 0,
                clientOrderId: FixedBytes::default(),
            }
            .abi_encode(),
            10_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(!out.reverted, "placement reverted: {:?}", out.bytes);
        assert!(
            take_balance_events(&mut ctx).is_empty(),
            "placement is silent"
        );
        let order_id: [u8; 32] =
            crate::interface::IPerpDex::placeOrderCall::abi_decode_returns(&out.bytes)
                .unwrap()
                .0;
        assert!(
            storage::load_user_markets(&mut ctx, ALICE)
                .unwrap()
                .contains(&MARKET_2),
            "the resting order really did add MARKET_2 to the index — the membership leg is live"
        );
        assert_eq!(
            published(&mut ctx, ALICE),
            before,
            "a placement cannot move any published field — not the two balances, and not through \
             the index-membership change it causes"
        );
        // …while the numbers the OLD payload carried really did move. Without this the equality
        // above would hold for the trivial reason that nothing happened.
        let after_place = get_account(&mut ctx, ALICE);
        assert!(
            after_place.totalOpenOrderInitialMargin > 0
                && after_place.availableBalance < available_before,
            "the resting order must move `Σ ooIM` and `availableBalance` — the two fields the \
             narrowing removed, which is why they are REST-only: {:?}",
            (
                after_place.totalOpenOrderInitialMargin,
                after_place.availableBalance,
                available_before
            )
        );

        // ── …and a cancel ─────────────────────────────────────────────────────────────────────
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let out = run_perp_dex_call(
            &cancelOrderCall {
                orderId: order_id.into(),
                marketId: MARKET_2,
            }
            .abi_encode(),
            10_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(!out.reverted, "cancel reverted: {:?}", out.bytes);
        assert!(take_balance_events(&mut ctx).is_empty(), "cancel is silent");
        assert!(
            !storage::load_user_markets(&mut ctx, ALICE)
                .unwrap()
                .contains(&MARKET_2),
            "the cancel dropped MARKET_2 back out of the index — the other direction of the same leg"
        );
        assert_eq!(
            published(&mut ctx, ALICE),
            before,
            "a cancel cannot move any published field either"
        );
        assert_eq!(
            get_account(&mut ctx, ALICE).availableBalance,
            available_before,
            "…while `availableBalance` came back, again unannounced"
        );
    }

    /// **The emit filter is not a revert filter.** A placement refused by the margin gate emits
    /// NOTHING at all — not the (already absent) snapshot, and not `OrderPlaced`/`OrderRested`
    /// either. Validate-then-apply: under commit-only there is no undo, so a log emitted before a
    /// reject would survive in a batch (the batch selectors catch a per-item error and return Ok).
    #[test]
    fn a_rejected_placement_emits_nothing() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        // Leave ALICE with no headroom at all, then ask for an order whose ooIM is strictly
        // positive: `available(after) < 0` and `Δ ooIM > 0`, so the gate refuses.
        set_available(&mut ctx, ALICE, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        start_call(&mut ctx);
        let err = place_in(&mut ctx, ALICE, MARKET_ID, 0, PRICE, QTY, 0).unwrap_err();
        assert!(
            err.to_string()
                .contains("insufficient perp wallet for margin"),
            "{err}"
        );
        assert!(
            JournalTr::take_logs(ctx.journal_mut()).is_empty(),
            "a rejected placement must emit no logs at all"
        );
    }

    /// A call that REVERTS through the shell publishes nothing: the drain is on the success arm.
    #[test]
    fn a_reverted_call_publishes_no_account_snapshot() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        set_available(&mut ctx, ALICE, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let out = run_perp_dex_call(
            &placeOrderCall {
                marketId: MARKET_ID,
                side: 0,
                price: PRICE,
                quantity: QTY,
                orderType: 0,
                tif: 0,
                clientOrderId: FixedBytes::default(),
            }
            .abi_encode(),
            10_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(out.reverted, "the margin gate must refuse this");
        assert!(take_balance_events(&mut ctx).is_empty());

        // …and the marks a reverted call left behind do not leak into the NEXT call. This is the
        // scope property: the set lives in `TypedPerpStore`, which the journal keeps for the whole
        // BLOCK, so it is `begin_perp_call` at the top of the shell — not any tx boundary — that
        // makes it call-scoped. Without that reset the deposit below would publish for ALICE too.
        let out = run_perp_dex_call(
            &crate::interface::IPerpDex::getAdminCall {}.abi_encode(),
            1_000_000,
            BOB,
            U256::ZERO,
            true,
            &mut ctx,
        )
        .unwrap();
        assert!(!out.reverted);
        assert!(
            take_balance_events(&mut ctx).is_empty(),
            "a later call must not inherit a reverted call's marks"
        );
    }

    // ── The coalescing: one settled snapshot per user, last, in address order ─────────────────

    /// A crossing fill publishes one snapshot per affected user — the maker's at his fill, the
    /// taker's after her order settles — and each equals `getAccount`.
    ///
    /// The counts happen to be unchanged here (one fill, one order, so one each), which is exactly
    /// why this is the right place to pin the STREAM SHAPE rather than the counts: what moved is
    /// WHERE the maker's row sits. It used to be in the drain at the very end; it is now inside the
    /// match flush, immediately after the `Trade` row it closes, with the taker's row after her own
    /// `PositionChanged` and before the `OrderRested` of the remainder she rests. So the snapshots
    /// are no longer a contiguous suffix, and
    /// [`assert_account_update_groups`] is the property that replaces that one.
    #[test]
    fn a_crossing_fill_publishes_a_snapshot_closing_each_party_s_rows() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        // One lot of liquidity: ALICE's 2-lot GTC buy fills one and rests one, so the same call both
        // moves money (the fill) and rests an order (which on its own would publish nothing).
        place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell");
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let out = run_perp_dex_call(
            &placeOrderCall {
                marketId: MARKET_ID,
                side: 0,
                price: PRICE,
                quantity: 2 * QTY,
                orderType: 0,
                tif: 0,
                clientOrderId: FixedBytes::default(),
            }
            .abi_encode(),
            10_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(!out.reverted);

        let logs = JournalTr::take_logs(ctx.journal_mut());
        // The snapshots are NOT a suffix any more: BOB's sits mid-stream, inside the flush.
        let names = logs
            .iter()
            .map(|log| log.data.topics().first().copied())
            .collect::<Vec<_>>();
        let snapshot_at = |i: usize| names[i] == Some(AccountBalanceChanged::SIGNATURE_HASH);
        let first_snapshot = (0..names.len())
            .find(|i| snapshot_at(*i))
            .expect("snapshots");
        assert!(
            !(first_snapshot..names.len()).all(snapshot_at),
            "the suffix shape is GONE by design — if it came back, the maker's snapshot has drifted \
             out of the flush and back into the drain"
        );
        assert_account_update_groups(&logs);

        let events = logs
            .into_iter()
            .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
            .map(|log| {
                AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
            })
            .collect::<Vec<_>>();
        // BOB first (his fill, inside the flush), then ALICE (her order, after `finalize_apply`).
        // ALICE is written twice inside the call and also rests a remainder, and still gets exactly
        // one: her mark is cleared at her emit, and the resting leg does not re-mark her. The fee
        // rates are 0 here so ADMIN is never credited and never appears — only AFFECTED users do.
        assert_eq!(
            events.iter().map(|e| e.user).collect::<Vec<_>>(),
            vec![BOB, ALICE],
        );
        for e in &events {
            assert_event_matches_get_account(&mut ctx, e);
        }
    }

    /// A 2-item batch, each item a crossing order into the same maker: **the initiator gets TWO and
    /// the maker gets TWO** — one per order and one per fill respectively.
    ///
    /// This is the test whose expectation the granularity change is FOR, so it is worth being
    /// explicit about what it used to say and why that was wrong. It used to assert ONE snapshot for
    /// the initiator, citing "one final snapshot per affected user per transaction": a K-item batch
    /// (K ≤ `MAX_BATCH_PLACE` = 64) published a single event for the initiator. The initiator's side
    /// of that was defensible — it is her transaction. The MAKER's was not: BOB also got one event,
    /// at the end of a transaction he did not participate in, for two fills that were two separate
    /// economic events for him, and the cadence of his notifications was set by how ALICE chose to
    /// batch. Both sides are now per-event, and BOB's two rows sit at his two fills.
    ///
    /// The two items are in DIFFERENT markets, which is the leg that makes the maker count
    /// non-trivial: each market runs its own `MatchRegistry`, so BOB's two snapshots come from two
    /// different working copies, and each has to reconstruct `Σ pos.margin` over the market the other
    /// one moved (`UserWork::other_market_position_margin`). A single-market version of this test
    /// would leave that term at zero and prove nothing about it.
    #[test]
    fn a_two_item_batch_publishes_per_order_for_the_initiator_and_per_fill_for_the_maker() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        add_market_2(&mut ctx);
        // Liquidity on both markets, from the same maker.
        place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell m1");
        place_in(&mut ctx, BOB, MARKET_2, 1, PRICE, QTY, 3).expect("maker sell m2");
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let item = |market: u64| PlaceItem {
            marketId: market,
            side: 0,
            price: PRICE,
            quantity: QTY,
            orderType: 0,
            tif: 0,
            clientOrderId: FixedBytes::default(),
        };
        let out = run_perp_dex_call(
            &batchPlaceOrdersCall {
                orders: vec![item(MARKET_ID), item(MARKET_2)],
            }
            .abi_encode(),
            30_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(
            !out.reverted,
            "batch reverted: {:?}",
            String::from_utf8_lossy(&out.bytes)
        );

        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_account_update_groups(&logs);
        let events = logs
            .into_iter()
            .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
            .map(|log| {
                AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            events.iter().filter(|e| e.user == ALICE).count(),
            2,
            "the initiator gets one snapshot per ORDER — two items, two orders, got {:?}",
            events.iter().map(|e| e.user).collect::<Vec<_>>()
        );
        assert_eq!(
            events.iter().filter(|e| e.user == BOB).count(),
            2,
            "the maker gets one snapshot per FILL — his two orders are hit once each, and his \
             cadence no longer depends on how the taker batched, got {:?}",
            events.iter().map(|e| e.user).collect::<Vec<_>>()
        );
        assert_eq!(
            events.iter().map(|e| e.user).collect::<Vec<_>>(),
            vec![BOB, ALICE, BOB, ALICE],
            "item by item: maker at the fill, then initiator at the end of that order (fee rates \
             are 0, so no ADMIN credit)"
        );
        // The LAST event per user is the settled state an indexer keeps under last-per-user-wins.
        // Earlier rows for the same user are earlier truths, not wrong ones, so only the last is
        // compared against `getAccount` here.
        for user in [ALICE, BOB] {
            let last = events.iter().rfind(|e| e.user == user).unwrap();
            assert_event_matches_get_account(&mut ctx, last);
        }
    }

    /// The drain order is ASCENDING ADDRESS order, not write order and not hash-map order — pinned
    /// with two users whose writes happen in the opposite order.
    ///
    /// A hash-map iteration order here would make the receipts differ between nodes running the same
    /// code, so this is a consensus property rather than a cosmetic one.
    #[test]
    fn the_drain_order_is_ascending_address_order() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        start_call(&mut ctx);
        // BOB (0x22..) written FIRST, ALICE (0x11..) second.
        storage::mutate_account_balance(&mut ctx, BOB, AccountUpdateReason::Adjustment, |a| {
            a.credit_perp(1)
        })
            .unwrap()
            .unwrap();
        storage::mutate_account_balance(&mut ctx, ALICE, AccountUpdateReason::Adjustment, |a| {
            a.credit_perp(1)
        })
            .unwrap()
            .unwrap();
        end_call(&mut ctx);

        assert_eq!(
            take_balance_events(&mut ctx)
                .iter()
                .map(|e| e.user)
                .collect::<Vec<_>>(),
            vec![ALICE, BOB],
            "drained in address order, which here is the REVERSE of the write order"
        );
    }

    /// **`setLeverage` publishes NOTHING — and this is the third position that decision has held.**
    ///
    /// It used to publish nothing (the trigger was the ACCOUNT write, and `setLeverage` writes only
    /// the position), then one snapshot (the trigger became the position write too), and now nothing
    /// again — but for a reason neither of the first two had: **a leverage change cannot move any
    /// field this event carries.** The assertions below pin both halves, so "publishes nothing" is
    /// not mimicry, it is *consistent* — exactly the shape of
    /// [`placement_and_cancel_cannot_move_any_published_field`]:
    ///
    /// * no account blob is written, so `usdcBalance` / `totalCrossWalletBalance` cannot move;
    /// * `risk::rebalance_order_margin_for_leverage` is a pure GATE (it takes `&PerpPosition`), so
    ///   changing leverage on a LIVE position does not reallocate `pos.margin` and the
    ///   `Σ pos.margin` half of `totalWalletBalance` is unchanged too;
    /// * `leverage` is not a field of `AccountBalanceChanged` (nor of Binance's `a.P[]`); it has its
    ///   own `LeverageChanged`, which IS emitted.
    ///
    /// What forced the third flip is `reason`: a row with no changed field has no truthful reason,
    /// and every available label (`ADJUSTMENT`, `ORDER`, `Multiple`) is one a consumer would act on.
    /// So the write routes through `storage::save_position_leverage_only`, which marks nobody.
    ///
    /// The GAS is deliberately left at 30_000. The surcharge was originally justified by the
    /// snapshot fold, but `rebalance_order_margin_for_leverage` walks the per-user market index on
    /// its own (`derived_available_balance_with`), which is the same "walks a per-user list" work the
    /// surcharge prices — and lowering a DoS brake is not a side effect this change should have.
    #[test]
    fn set_leverage_publishes_no_snapshot() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        // The seeded long is leverage 1; raising it is allowed (only REDUCING with an open position
        // is refused).
        let margin_before = pos(&mut ctx, ALICE).margin;
        let account_before = storage::load_account(&mut ctx, ALICE).unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let out = run_perp_dex_call(
            &setLeverageCall {
                marketId: MARKET_ID,
                leverage: 2,
            }
            .abi_encode(),
            10_000_000,
            ALICE,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(
            !out.reverted,
            "setLeverage reverted: {:?}",
            String::from_utf8_lossy(&out.bytes)
        );
        assert_eq!(out.gas_used, 30_000);

        assert!(
            take_balance_events(&mut ctx).is_empty(),
            "a leverage change moves no published field, so it must publish no snapshot"
        );
        // …and the call really did something: the leverage moved, and `LeverageChanged` announced it.
        let after = pos(&mut ctx, ALICE);
        assert_eq!(after.leverage, 2, "the leverage really did change");
        // The three legs of "no published field moved", read off state rather than off the event.
        assert_eq!(
            after.margin, margin_before,
            "changing leverage on a LIVE position must not reallocate `pos.margin` — if it ever \
             does, `totalWalletBalance` moves and this selector owes a snapshot again"
        );
        let account_after = storage::load_account(&mut ctx, ALICE).unwrap();
        assert_eq!(
            (
                account_after.usdc_balance.clone(),
                account_after.perp_wallet_balance,
                account_after.total_position_margin
            ),
            (
                account_before.usdc_balance.clone(),
                account_before.perp_wallet_balance,
                account_before.total_position_margin
            ),
            "no account field behind the published payload may move"
        );
    }

    // ── Per-fill for makers, per-order for takers ────────────────────────────────────────────

    /// Drive one crossing `placeOrder` through the shell and return its whole log stream.
    fn crossing_order(
        ctx: &mut TestCtx,
        caller: Address,
        side: u8,
        price: u64,
        qty: u64,
    ) -> Vec<primitives::Log> {
        let _ = JournalTr::take_logs(ctx.journal_mut());
        let out = run_perp_dex_call(
            &placeOrderCall {
                marketId: MARKET_ID,
                side,
                price,
                quantity: qty,
                orderType: 0,
                tif: 0,
                clientOrderId: FixedBytes::default(),
            }
            .abi_encode(),
            10_000_000,
            caller,
            U256::ZERO,
            false,
            ctx,
        )
        .unwrap();
        assert!(
            !out.reverted,
            "crossing order reverted: {}",
            String::from_utf8_lossy(&out.bytes)
        );
        JournalTr::take_logs(ctx.journal_mut())
    }

    fn snapshots_of(logs: &[primitives::Log]) -> Vec<AccountBalanceChanged> {
        logs.iter()
            .filter(|log| log.data.topics().first() == Some(&AccountBalanceChanged::SIGNATURE_HASH))
            .map(|log| {
                AccountBalanceChanged::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
            })
            .collect()
    }

    /// **The headline of the granularity change.** BOB rests TWO sells in the same level; ALICE
    /// sweeps both in ONE order. BOB gets **two** snapshots — one per fill — and ALICE gets **one**,
    /// for her one order.
    ///
    /// This is precisely the asymmetry the coalescing hid. Under "one per user per transaction" BOB
    /// got a single event, at the end of ALICE's transaction, for two distinct economic events of his
    /// own; batch the taker side harder and BOB's two fills, or forty, still collapsed into one
    /// notification whose timing he had no part in choosing. A maker order is consumed at most once
    /// per taker sweep, so per fill is also exactly per maker ORDER, which is the unit a maker
    /// actually reasons about.
    ///
    /// The two rows are DIFFERENT — each is BOB's state at its own fill — which is what makes the
    /// count meaningful rather than a duplicated log. The second is his settled state and equals
    /// `getAccount`; the first is his state after one lot, and the difference between them is exactly
    /// the second lot's effect on `wb`.
    #[test]
    fn a_maker_filled_twice_in_one_sweep_gets_two_snapshots_and_the_taker_one() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        // Two separate maker orders in the SAME level, so one sweep consumes both from one queue.
        place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell 1");
        place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("maker sell 2");

        let logs = crossing_order(&mut ctx, ALICE, 0, PRICE, 2 * QTY);
        assert_account_update_groups(&logs);
        let events = snapshots_of(&logs);

        assert_eq!(
            events.iter().map(|e| e.user).collect::<Vec<_>>(),
            vec![BOB, BOB, ALICE],
            "two fills for BOB → two snapshots, interleaved with his fills; ONE order for ALICE → \
             one snapshot, after it settles"
        );
        // Two fills against the same maker really did happen — the counts above are not one fill
        // reported twice.
        assert_eq!(
            logs.iter()
                .filter(|l| l.data.topics().first() == Some(&Trade::SIGNATURE_HASH))
                .count(),
            2,
            "fixture: the sweep must consume BOTH maker orders"
        );
        // BOB's two rows are two different states, and the second is the settled one.
        assert_ne!(
            (
                events[0].totalWalletBalance,
                events[0].totalCrossWalletBalance
            ),
            (
                events[1].totalWalletBalance,
                events[1].totalCrossWalletBalance
            ),
            "each maker row is that maker's state at ITS OWN fill, not the same row twice"
        );
        assert_event_matches_get_account(&mut ctx, &events[1]);
        assert_event_matches_get_account(&mut ctx, &events[2]);
    }

    /// A SELF-TRADE publishes **both legs**: the maker-leg snapshot at the fill and the taker-leg
    /// snapshot at the end of the order, both for the same address.
    ///
    /// The owner chose both over suppressing one. The alternative — recognise `maker == taker` and
    /// emit once — would make the maker leg conditional on who the taker happens to be, i.e. a
    /// maker's own notification would depend on a property of the counterparty. Emitting both keeps
    /// each leg's rule unconditional and pushes the reconciliation to where it is trivial:
    /// **consumers take the last row per user**, which is already how every after-image event on this
    /// ABI has to be read.
    ///
    /// The maker-leg row is the genuinely interesting one: it is derived from the registry working
    /// copy that the TAKER leg then goes on to reuse and evolve (`match_order`'s "self-match reuses
    /// the evolved copies"), so it is a real intermediate — and the taker row that follows is the
    /// settled state.
    #[test]
    fn a_self_trade_publishes_both_the_maker_leg_and_the_taker_leg() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        place_in(&mut ctx, ALICE, MARKET_ID, 1, PRICE, QTY, 3).expect("ALICE rests a maker sell");

        let logs = crossing_order(&mut ctx, ALICE, 0, PRICE, QTY);
        assert_account_update_groups(&logs);
        let events = snapshots_of(&logs);

        assert_eq!(
            events.iter().map(|e| e.user).collect::<Vec<_>>(),
            vec![ALICE, ALICE],
            "both legs: the maker-leg snapshot at the fill, then the taker-leg one for the order"
        );
        // Last-per-user-wins: the second row is the settled account, the first is not required to be.
        assert_event_matches_get_account(&mut ctx, &events[1]);
    }

    /// **The one branch where a bug LOSES an event: the fee recipient is also a maker.**
    ///
    /// ADMIN rests a sell at `PRICE`; BOB rests one a tick worse. ALICE sweeps both, so ADMIN's own
    /// fill comes FIRST and BOB's second — and BOB's maker fee is credited to ADMIN through
    /// `MatchRegistry::credit_admin`'s read-through, landing on ADMIN's working copy **after** ADMIN's
    /// per-fill snapshot was already derived from it. ADMIN's state therefore moves once more, with no
    /// further event of ADMIN's own to hang a snapshot on.
    ///
    /// So ADMIN gets **two** rows: the per-fill one, and a drain one carrying the later fee. That
    /// second row exists only because `MatchRegistry::flush` gates the mark-clear on the settled
    /// payload equalling what was published, rather than on "I emitted something for this user".
    /// An unconditional clear passes every other test in this module and silently drops this update —
    /// which is why this test exists, and why it asserts the DELTA rather than just the count.
    ///
    /// ⚠️ ALICE's taker fee is deliberately left at 0. A non-zero one would make `finalize_apply`'s
    /// `credit_fee_recipient` mark ADMIN *after* the flush as well, so the drain row would be
    /// explained by either cause and the branch under test would no longer be isolated.
    #[test]
    fn a_fee_recipient_who_is_also_a_maker_gets_a_drain_row_for_the_later_fee() {
        let mut ctx = make_ctx();
        fixture(&mut ctx);
        fund(&mut ctx, ADMIN, WALLET);
        // BOB pays a maker fee; ADMIN's own fill pays none, so the only fee in this call is the one
        // credited to ADMIN AFTER ADMIN's snapshot.
        storage::save_user_fee_rates(
            &mut ctx,
            BOB,
            UserFeeRates {
                maker_fee_bps: 10,
                taker_fee_bps: 0,
            },
        )
        .unwrap();
        assert_eq!(
            storage::load_user_fee_rates(&mut ctx, ALICE)
                .unwrap()
                .taker_fee_bps,
            0,
            "fixture: the taker must pay no fee, or the drain row is not attributable"
        );

        // ADMIN at the better price is hit FIRST; BOB's fill (and therefore BOB's fee) follows.
        place_in(&mut ctx, ADMIN, MARKET_ID, 1, PRICE, QTY, 3).expect("ADMIN maker sell");
        place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE + TICK, QTY, 3).expect("BOB maker sell");

        let logs = crossing_order(&mut ctx, ALICE, 0, PRICE + TICK, 2 * QTY);
        assert_account_update_groups(&logs);
        let events = snapshots_of(&logs);

        assert_eq!(
            events.iter().map(|e| e.user).collect::<Vec<_>>(),
            vec![ADMIN, BOB, ALICE, ADMIN],
            "ADMIN twice: once at ADMIN's own fill, once from the drain for BOB's fee — which \
             arrived after that fill's snapshot had already been taken"
        );
        // ── The KNOWN live drain conflict, and why it is NOT a `Multiple` ────────────────────────
        //
        // ADMIN is marked twice here by two different economic events — their own maker fill (the
        // flush's `save_position` / `save_account`) and BOB's fee credit (`credit_admin`'s
        // read-through, materialised at the flush) — and the drain publishes ONE row for both. Both
        // marks carry `Order`, because a trading fee IS money moved by a matched order, so
        // `AccountUpdateReason::merge` is a no-op and the row keeps a truthful label rather than
        // degrading to `Multiple`. That is the case the merge rule was checked against first.
        assert_eq!(
            account_update_reasons(&logs),
            vec![
                (ADMIN, AccountUpdateReason::Order),
                (BOB, AccountUpdateReason::Order),
                (ALICE, AccountUpdateReason::Order),
                (ADMIN, AccountUpdateReason::Order),
            ],
            "the fee recipient's drain row reports ORDER — a fee is order-driven money"
        );

        // The two ADMIN rows differ by exactly BOB's maker fee, which is the whole point: the second
        // row carries information the first could not have.
        //
        //   calc_value(PRICE + TICK, QTY, 8, 9) = 101 * 1e9 * 1e6 * 1e6 / (1e9 * 1e8) = 1_010_000
        //   fee = 1_010_000 * 10 bps / 10_000                                        =     1_010
        const BOB_MAKER_FEE: i64 = 1_010;
        let (first, second) = (&events[0], &events[3]);
        assert_eq!(
            second.totalCrossWalletBalance - first.totalCrossWalletBalance,
            BOB_MAKER_FEE,
            "the drain row must carry the later fee credit, not repeat the fill row"
        );
        assert_eq!(
            second.totalWalletBalance - first.totalWalletBalance,
            BOB_MAKER_FEE,
            "…and it lands on the wallet, so `wb` moves with `cw`"
        );
        // The LAST row is the settled account. (The first is not, and must not be asserted to be.)
        assert_event_matches_get_account(&mut ctx, second);
    }

    /// The whole snapshot stream — order, users and payloads — is **byte-for-byte reproducible**.
    ///
    /// Two identical scenarios built in two independent contexts must produce the same sequence.
    /// Emission is no longer a single `BTreeSet` drain, so determinism now rests on three ordered
    /// things instead of one: the match walk's fill order, `MatchRegistry`'s event replay (a `Vec`,
    /// in push order), and the drain's `BTreeSet`. A `HashMap` anywhere in that chain would make
    /// receipts differ between nodes running the same code, which is a consensus bug rather than a
    /// cosmetic one — so this is pinned across a scenario that exercises all three: two makers, a
    /// multi-fill sweep, a resting remainder and a fee-earning ADMIN.
    #[test]
    fn the_snapshot_stream_is_deterministic() {
        fn run() -> Vec<(Address, i64, i64)> {
            let mut ctx = make_ctx();
            fixture(&mut ctx);
            // Non-zero maker fee → ADMIN is credited and joins the stream through the drain.
            storage::save_user_fee_rates(
                &mut ctx,
                BOB,
                UserFeeRates {
                    maker_fee_bps: 1,
                    taker_fee_bps: 0,
                },
            )
            .unwrap();
            fund(&mut ctx, CAROL, WALLET);
            place_in(&mut ctx, BOB, MARKET_ID, 1, PRICE, QTY, 3).expect("BOB sell");
            place_in(&mut ctx, CAROL, MARKET_ID, 1, PRICE, QTY, 3).expect("CAROL sell");
            // 3 lots: two fills, one lot rests.
            let logs = crossing_order(&mut ctx, ALICE, 0, PRICE, 3 * QTY);
            assert_account_update_groups(&logs);
            snapshots_of(&logs)
                .into_iter()
                .map(|e| (e.user, e.totalWalletBalance, e.totalCrossWalletBalance))
                .collect()
        }

        let first = run();
        assert_eq!(first, run(), "the snapshot stream must be reproducible");
        // …and it is the stream this change is about, not an empty one: both makers report at their
        // own fills (FIFO: BOB's order rested first), the taker once, ADMIN last from the drain.
        assert_eq!(
            first.iter().map(|(u, ..)| *u).collect::<Vec<_>>(),
            vec![BOB, CAROL, ALICE, ADMIN]
        );
    }
}

// ── `PositionChanged`'s derived fields = `ACCOUNT_UPDATE.a.P[]` ───────────────────────────────
//
// `entryPrice` (`ep`) and `unrealizedProfit` (`up`) are DERIVED at emit time by the single
// `crate::events` helper every one of the seven emit sites calls; `cumulativeRealizedPnl` (`cr`)
// and `breakevenPrice` (`bep`) are placeholders. These pin all four.
mod position_changed_derived_fields {
    use super::*;

    /// $150 — the size-weighted average of a $100 fill and a $200 fill of equal size.
    const AVG_PRICE: u64 = 150 * TICK;

    /// The LAST `PositionChanged` this call emitted for `user`. Every field on the event is an
    /// after-image, so for a multi-fill call this is the settled row an indexer would put in
    /// `P[]` (see the ABI note: last-one-wins per `(user, marketId)`).
    fn last_change(
        ctx: &mut TestCtx,
        user: Address,
    ) -> crate::interface::IPerpDex::PositionChanged {
        take_position_changes(ctx)
            .into_iter()
            .rfind(|c| c.user == user)
            .expect("expected at least one PositionChanged")
    }

    /// Drive the shared derivation directly on a hand-built position at a hand-set mark. This is
    /// the same `crate::events::emit_position_changed` all seven production sites go through, so
    /// what it reports here is what they report — without a fixture that has to keep a matching
    /// engine happy at every mark under test.
    fn emit_at(
        ctx: &mut TestCtx,
        amount: i64,
        v_quote: i64,
        margin: i64,
        mark: u64,
    ) -> crate::interface::IPerpDex::PositionChanged {
        storage::save_position(
            ctx,
            ALICE,
            MARKET_ID,
            &PerpPosition {
                amount,
                v_quote_balance: v_quote,
                margin,
                leverage: 1,
                ..PerpPosition::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
        set_mark(ctx, mark);
        let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
        let p = pos(ctx, ALICE);
        // The account header the production sites emit before their position row. Without it this
        // helper would publish an ORPHAN `PositionChanged`, which the group guard in `crate::events`
        // rejects — correctly: the derivation under test is only ever reached from inside a group.
        storage::publish_account_snapshot_now(ctx, ALICE, AccountUpdateReason::Adjustment).unwrap();
        crate::events::emit_position_changed(ctx, ALICE, &market, &p, 0, 0).unwrap();
        last_change(ctx, ALICE)
    }

    // ── `ep` ─────────────────────────────────────────────────────────────────────────────────

    /// `P[].ep` is specified as `"0"` when flat, and a full close is how a position gets there on
    /// the REAL path — so this drives an actual open-then-close through the book rather than
    /// hand-setting `amount = 0`.
    #[test]
    fn a_full_close_reports_a_flat_position_with_a_zero_entry_price() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        fund(&mut ctx, CAROL, WALLET);

        // ALICE opens a long, then sells the whole thing back into CAROL's bid.
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        place(&mut ctx, CAROL, 0, PRICE, QTY, 0, 0);
        place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);

        let flat = last_change(&mut ctx, ALICE);
        assert_eq!(
            pos(&mut ctx, ALICE).amount,
            0,
            "fixture: ALICE must be flat"
        );
        assert_eq!(flat.amount, 0);
        assert_eq!(
            flat.entryPrice, 0,
            "a flat position has no entry price — `P[].ep` must be 0"
        );
        assert_eq!(
            flat.unrealizedProfit, 0,
            "nothing open, nothing to mark to market"
        );
    }

    /// **The average, not the last fill.** ALICE opens `QTY` at $100 and `QTY` at $200, then
    /// closes HALF at $500. `entryPrice` must read $150 throughout — the size-weighted average of
    /// the two opens — and specifically must not be $200 (the last opening fill) or $500 (the
    /// close). A partial close scales `v_quote_balance` proportionally
    /// (`settlement::apply_position_fill`), which is exactly why the average survives it.
    #[test]
    fn entry_price_after_a_partial_close_is_the_average_not_the_last_fill() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        fund(&mut ctx, CAROL, WALLET);
        let dave = user_addr(9);
        fund(&mut ctx, dave, WALLET);

        // Open leg 1: QTY @ $100.
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        assert_eq!(last_change(&mut ctx, ALICE).entryPrice, PRICE);

        // Open leg 2: QTY @ $200 → 2×QTY long, average entry $150.
        place(&mut ctx, CAROL, 1, 200 * TICK, QTY, 0, 0);
        place(&mut ctx, ALICE, 0, 200 * TICK, QTY, 0, 0);
        let after_second_open = last_change(&mut ctx, ALICE);
        assert_eq!(after_second_open.amount, 2 * QTY as i64);
        assert_eq!(
            after_second_open.entryPrice, AVG_PRICE,
            "two equal-size fills at $100 and $200 average to $150"
        );

        // Close HALF at $500 — far from both opens, so a "last fill price" bug is unmissable.
        place(&mut ctx, dave, 0, 500 * TICK, QTY, 0, 0);
        place(&mut ctx, ALICE, 1, 500 * TICK, QTY, 0, 0);

        let after_close = last_change(&mut ctx, ALICE);
        assert_eq!(after_close.amount, QTY as i64, "fixture: half still open");
        assert_eq!(after_close.closedQuantity, QTY);
        assert_eq!(
            after_close.entryPrice, AVG_PRICE,
            "a partial close must not move the entry price"
        );
        assert_ne!(
            after_close.entryPrice,
            200 * TICK,
            "entryPrice is the average, not the last OPENING fill price"
        );
        assert_ne!(
            after_close.entryPrice,
            500 * TICK,
            "entryPrice is the average, not the CLOSING fill price"
        );
    }

    // ── `up` ─────────────────────────────────────────────────────────────────────────────────

    /// A LONG is in profit above its entry and in loss below it. Position: `QTY` long at $100
    /// (`v_quote = -FILL_VALUE`); at $150 the notional is `1.5 × FILL_VALUE`, so
    /// `up = +0.5 × FILL_VALUE`.
    #[test]
    fn unrealised_pnl_sign_is_positive_for_a_long_above_entry_and_negative_below() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let (amount, v_quote) = (QTY as i64, -(FILL_VALUE as i64));

        let above = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, AVG_PRICE);
        assert_eq!(above.entryPrice, PRICE);
        assert_eq!(above.unrealizedProfit, FILL_VALUE as i64 / 2);
        assert!(above.unrealizedProfit > 0, "long above entry is in PROFIT");

        let below = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, 50 * TICK);
        assert_eq!(below.unrealizedProfit, -(FILL_VALUE as i64 / 2));
        assert!(below.unrealizedProfit < 0, "long below entry is in LOSS");

        let at_entry = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, PRICE);
        assert_eq!(at_entry.unrealizedProfit, 0, "mark == entry ⟹ up == 0");
    }

    /// A SHORT is the mirror: in profit BELOW its entry. Position: `QTY` short at $100
    /// (`v_quote = +FILL_VALUE`, `amount` negative), so the signed notional is negative and shrinks
    /// in magnitude as the mark falls.
    #[test]
    fn unrealised_pnl_sign_is_positive_for_a_short_below_entry_and_negative_above() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let (amount, v_quote) = (-(QTY as i64), FILL_VALUE as i64);

        let below = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, 50 * TICK);
        assert_eq!(
            below.entryPrice, PRICE,
            "a short's entry price is the same positive price as a long's"
        );
        assert_eq!(below.unrealizedProfit, FILL_VALUE as i64 / 2);
        assert!(below.unrealizedProfit > 0, "short below entry is in PROFIT");

        let above = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, AVG_PRICE);
        assert_eq!(above.unrealizedProfit, -(FILL_VALUE as i64 / 2));
        assert!(above.unrealizedProfit < 0, "short above entry is in LOSS");
    }

    /// `up` is mark-to-market: the SAME stored position reports a different `up` at every mark,
    /// moving one-for-one with `amount × Δmark`. This is the whole reason the field is on the
    /// event — with the `@position` stream gone, an indexer has no other way to fill it.
    #[test]
    fn unrealised_pnl_tracks_a_mark_move() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        let (amount, v_quote) = (QTY as i64, -(FILL_VALUE as i64));

        let mut seen = Vec::new();
        for mark in [
            PRICE,
            PRICE + 10 * TICK,
            PRICE + 20 * TICK,
            PRICE - 40 * TICK,
        ] {
            let e = emit_at(&mut ctx, amount, v_quote, INIT_MARGIN as i64, mark);
            // Stored state never moved, so `pa`/`ep` are constant and only `up` responds.
            assert_eq!((e.amount, e.entryPrice), (amount, PRICE));
            seen.push(e.unrealizedProfit);
        }
        // calc_value(10 * TICK, QTY, 8, 9) = 100_000 quote units per $10 of mark move.
        let per_ten_dollars = FILL_VALUE as i64 / 10;
        assert_eq!(
            seen,
            vec![
                0,
                per_ten_dollars,
                2 * per_ten_dollars,
                -4 * per_ten_dollars
            ],
            "up must move one-for-one with the mark on an unchanged position"
        );
    }

    // ── the two placeholders ─────────────────────────────────────────────────────────────────

    /// ⚠️ **THIS TEST MUST BE UPDATED — not deleted — WHEN EITHER FIELD IS POPULATED.**
    ///
    /// `cumulativeRealizedPnl` (`P[].cr`) and `breakevenPrice` (`P[].bep`) are carried so the
    /// payload shape is stable (the precedent the public docs set for `ACCOUNT_UPDATE.a.m`) and
    /// are hardcoded 0 because the state they need does not exist: a per-position cumulative
    /// realised-PnL accumulator, and cumulative fees paid. See the ABI comment on
    /// `PositionChanged`.
    ///
    /// The scenario below deliberately GENERATES both quantities — a realised profit and paid
    /// commissions on both sides — so the assertion is "zero even though there is something real
    /// to report", not "zero because nothing happened". When the fields are wired, replace these
    /// two `assert_eq!(.., 0)` with the true values; a green run here after that change means the
    /// wiring never reached the event.
    #[test]
    fn the_placeholder_position_fields_are_zero() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        fund(&mut ctx, CAROL, WALLET);
        // Non-zero fees on both sides, so `bep` would differ from `ep` if it were populated.
        for user in [ALICE, BOB, CAROL] {
            storage::save_user_fee_rates(
                &mut ctx,
                user,
                UserFeeRates {
                    maker_fee_bps: 2,
                    taker_fee_bps: 5,
                },
            )
            .unwrap();
        }

        // Open QTY long at $100, then close HALF at $200 → a realised profit plus four fee legs.
        place(&mut ctx, BOB, 1, PRICE, 2 * QTY, 0, 0);
        place(&mut ctx, ALICE, 0, PRICE, 2 * QTY, 0, 0);
        place(&mut ctx, CAROL, 0, 200 * TICK, QTY, 0, 0);
        place(&mut ctx, ALICE, 1, 200 * TICK, QTY, 0, 0);

        let e = last_change(&mut ctx, ALICE);
        assert!(
            e.realizedPnl > 0 && e.closedQuantity == QTY,
            "fixture must actually realise PnL, else the zeros below prove nothing:                  realizedPnl={} closedQuantity={}",
            e.realizedPnl,
            e.closedQuantity
        );
        assert!(market_fee_total(&mut ctx) > 0, "fixture must pay fees");

        assert_eq!(
            e.cumulativeRealizedPnl, 0,
            "PLACEHOLDER: needs a per-position cumulative realised-PnL field on PerpPosition"
        );
        assert_eq!(
            e.breakevenPrice, 0,
            "PLACEHOLDER: needs cumulative fees paid per position"
        );
    }

    // ── the funding path emits a position row ────────────────────────────────────────────────

    /// Funding moves `pos.margin` = `P[].iw`. It used to do so with NO `PositionChanged` on some
    /// paths, publishing an account update (`save_position` marks the snapshot dirty) whose `P[]`
    /// was missing the very position whose margin moved. `apply_funding_settlement` now emits one,
    /// paired with `FundingSettled` and valued at the same mark.
    #[test]
    fn a_funding_settlement_emits_a_position_row_carrying_the_new_margin() {
        let mut ctx = make_ctx();
        setup(&mut ctx);
        set_mark(&mut ctx, PRICE);

        // ALICE: QTY long at $100, margin INIT_MARGIN.
        place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
        place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
        let _ = take_position_changes(&mut ctx);

        // A funding index a long OWES against: index_delta > 0 ⟹ the long pays.
        storage::save_funding_state(
            &mut ctx,
            MARKET_ID,
            &FundingState {
                last_funding_rate: 1_000,
                next_funding_ts: u64::MAX,
                // The index accumulates `mark_price × funding_rate` per epoch, and
                // `calc_funding_payment` divides by `1e price_decimals · 1e base_decimals ·
                // FUNDING_RATE_ONE`. At mark $100 (100e9) and rate 1_000 (0.1%) this is one
                // epoch, so a QTY long owes `0.001 × FILL_VALUE = 1_000` quote units.
                cumulative_funding_index: PRICE as i128 * 1_000,
            },
        )
        .unwrap();

        // `addPositionMargin` is the cheapest way to touch the position and trigger the lazy
        // settle. It emits its OWN PositionChanged too, so take the FUNDING one: the first.
        let before = pos(&mut ctx, ALICE).margin;
        crate::risk::run_add_position_margin(
            &crate::interface::IPerpDex::addPositionMarginCall {
                marketId: MARKET_ID,
                amount: 1,
            }
            .abi_encode(),
            ALICE,
            &mut ctx,
        )
        .unwrap();

        let changes: Vec<_> = take_position_changes(&mut ctx)
            .into_iter()
            .filter(|c| c.user == ALICE)
            .collect();
        assert_eq!(
            changes.len(),
            2,
            "expected the funding row then the margin-add row, got {} rows",
            changes.len()
        );
        let funding_row = &changes[0];
        let after = pos(&mut ctx, ALICE).margin;
        assert!(
            funding_row.margin < before,
            "the long must have PAID funding out of its margin: {before} -> {}",
            funding_row.margin
        );
        assert_eq!(
            funding_row.margin + 1,
            after,
            "the funding row's margin is the post-funding, pre-margin-add level"
        );
        // Same derivation as everywhere else: funding touches neither leg of `up`.
        assert_eq!(funding_row.entryPrice, PRICE);
        assert_eq!(funding_row.unrealizedProfit, 0, "mark == entry");
    }

    /// The path that had NO other position row at all: a maker whose fill is rejected as
    /// open-into-insolvency (`MakerFillOutcome::RejectedInsolvent`). Its funding is computed at
    /// `MatchRegistry::get_or_load` and flushed to storage with everyone else's, but the walk
    /// pushes no `PositionChanged` for it — so before the funding-path emit this transaction moved
    /// the maker's `iw` and published an account update with no `P[]` entry for it.
    #[test]
    fn an_insolvency_rejected_maker_still_reports_its_funding_adjusted_margin() {
        let mut ctx = make_ctx();
        setup_banded(&mut ctx, 1_000_000); // band effectively disabled
        set_mark(&mut ctx, PRICE);
        fund(&mut ctx, CAROL, WALLET);

        // CAROL takes BOB's bid and holds a real SHORT, so she has margin for funding to bite.
        place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0);
        place(&mut ctx, CAROL, 1, PRICE, QTY, 0, 0);
        assert_eq!(pos(&mut ctx, CAROL).amount, -(QTY as i64));

        // …and rests an ask at $1 while mark is $100. It must be an OPENING leg for the guard to
        // reach it — a pure close is always admitted — hence an ask (she is already short), and it
        // must be far enough below mark that the fill's own margin cannot carry the existing
        // position's buffer: filling would leave equity ~20_000 against a ~333_333 maintenance
        // requirement on 2×QTY, so the K9 guard cancels the order instead (same mechanism as
        // `maker_open_below_maintenance_is_cancelled_not_filled`, which uses a FLAT maker).
        let carol_ask = try_place_limit(&mut ctx, CAROL, 1, TICK).unwrap();
        let carol_ask: [u8; 32] = carol_ask[..32].try_into().unwrap();
        let _ = take_position_changes(&mut ctx);

        // NEGATIVE index so the SHORT is the side that pays (a positive rate credits a short).
        storage::save_funding_state(
            &mut ctx,
            MARKET_ID,
            &FundingState {
                last_funding_rate: -1_000,
                next_funding_ts: u64::MAX,
                cumulative_funding_index: -(PRICE as i128) * 1_000,
            },
        )
        .unwrap();
        let before = pos(&mut ctx, CAROL).margin;

        // ALICE buys into CAROL's stranded ask: the fill is rejected, the order cancelled, and
        // CAROL's ONLY state change is the funding settle.
        let _ = try_place_limit(&mut ctx, ALICE, 0, TICK).unwrap();
        assert_terminal(&mut ctx, carol_ask);
        assert_eq!(
            pos(&mut ctx, CAROL).amount,
            -(QTY as i64),
            "fixture: the insolvent fill must NOT have been applied"
        );

        let after = pos(&mut ctx, CAROL).margin;
        assert_eq!(
            after,
            before - 1_000,
            "fixture: funding must have moved CAROL's margin"
        );
        let carol_rows: Vec<_> = take_position_changes(&mut ctx)
            .into_iter()
            .filter(|c| c.user == CAROL)
            .collect();
        assert_eq!(
            carol_rows.len(),
            1,
            "the rejected maker must get exactly the funding row, got {} rows",
            carol_rows.len()
        );
        assert_eq!(
            carol_rows[0].margin, after,
            "the row must carry the margin that was actually persisted — this is the `iw` an \
             indexer puts in P[]"
        );
        assert_eq!(
            carol_rows[0].amount,
            -(QTY as i64),
            "her short is untouched"
        );
        assert_eq!(carol_rows[0].entryPrice, PRICE);
    }
}
