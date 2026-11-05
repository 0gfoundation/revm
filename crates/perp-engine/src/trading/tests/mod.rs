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

/// The chain id `make_ctx`'s `CfgEnv::default()` reports. Every signed message is bound to it,
/// so a test that signs with a different value verifies against nothing — which is the POINT of
/// the binding, but a confusing way to learn it. `the_signed_domain_is_bound_to_the_chain_id`
/// asserts this constant still matches the context.
const TEST_CHAIN_ID: u64 = 1;

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
    // Isolated margin, explicitly: every position this suite opens is ISOLATED unless a test
    // switches it (`margin_type::test_gate::isolated_by_default`), under the real gate.
    crate::margin::margin_type::test_gate::isolated_by_default();
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
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
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
    crate::margin::view::position_open_order_margin(&market, &p).unwrap()
}

/// `perp_wallet_balance − Σ ooIM` — the account's spendable headroom, i.e. the quantity every
/// admission gate now compares against. Signed: it can legitimately go negative.
fn available(ctx: &mut TestCtx, user: Address) -> i128 {
    crate::margin::view::derived_available_balance(ctx, user).unwrap()
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

/// How many self-trade-prevention cancels (`OrderCancelled` with reason 9) `logs` holds. The
/// randomised suites take it as a COVERAGE floor: a fuzz pass that never reaches STP says nothing
/// about it, and every one of them mixes the same users onto both sides of one book.
fn stp_cancel_count(logs: &[primitives::Log]) -> u32 {
    use crate::interface::IPerpDex::OrderCancelled;
    logs.iter()
        .filter(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
        .filter(|l| {
            OrderCancelled::decode_raw_log(l.data.topics(), &l.data.data).unwrap().reason
                == crate::types::CancelReason::SelfTradePrevention as u8
        })
        .count() as u32
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
/// * real check (`rest_in_book`, trading/resting.rs sell arm): the same formula over
///   `calc_value($100.15, QTY)`, because `save_last_traded_price` ran in between.
///
/// **That window no longer exists**: `save_last_traded_price` is deferred past the rest decision, so
/// `T` is resolved exactly once — and at the PRE-fill `last_traded`, which is the documented-correct
/// input ("not the caller's own fill", `margin::view::assuming_price_floor`). `2 x FILL_VALUE` is
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
                    flags: 0,
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
            flags: 0,
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
mod pre_walk_early_out;

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
            maker_fee_ppm: 0,
            taker_fee_ppm: 10_000,
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
            flags: 0,
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
        ..crate::margin::margin_type::never_set_position()
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

/// The registry blob is the members' 20-byte addresses back to back, in insertion order, edited in
/// place: an open appends one record, a close cuts its record out of the middle and moves the rest
/// up, and the last close leaves no key at all (an empty blob).
#[test]
fn position_registry_blob_is_edited_in_place() {
    let mut ctx = make_ctx();
    let m = MARKET_ID;
    let users = [user_addr(1), user_addr(2), user_addr(3), user_addr(4)];
    let open = |amt: i64| PerpPosition {
        amount: amt,
        ..crate::margin::margin_type::never_set_position()
    };
    let blob = |ctx: &mut TestCtx| {
        storage::load_blob(ctx, perp_core::keys::position_registry_key(MARKET_ID)).unwrap()
    };
    let packed = |members: &[Address]| members.iter().flat_map(|a| a.0 .0).collect::<Vec<u8>>();
    for (i, &u) in users.iter().enumerate() {
        let amt = if i % 2 == 0 { 5 } else { -5 };
        storage::save_position(&mut ctx, u, m, &open(amt), AccountUpdateReason::Adjustment)
            .unwrap();
    }
    assert_eq!(blob(&mut ctx), packed(&users));
    // A close in the middle, then at the end, then at the front.
    for (gone, left) in [
        (users[1], vec![users[0], users[2], users[3]]),
        (users[3], vec![users[0], users[2]]),
        (users[0], vec![users[2]]),
    ] {
        storage::save_position(&mut ctx, gone, m, &open(0), AccountUpdateReason::Adjustment)
            .unwrap();
        assert_eq!(blob(&mut ctx), packed(&left), "{gone} closed");
        assert_eq!(storage::load_position_registry(&mut ctx, m).unwrap(), left);
    }
    // Reopening appends at the end, once.
    storage::save_position(&mut ctx, users[1], m, &open(2), AccountUpdateReason::Adjustment)
        .unwrap();
    storage::save_position(&mut ctx, users[1], m, &open(3), AccountUpdateReason::Adjustment)
        .unwrap();
    assert_eq!(blob(&mut ctx), packed(&[users[2], users[1]]));
    for u in [users[2], users[1]] {
        storage::save_position(&mut ctx, u, m, &open(0), AccountUpdateReason::Adjustment).unwrap();
    }
    assert!(blob(&mut ctx).is_empty(), "the last close deletes the key");
    // An open by a user the blob already lists — a planted state: the position zeroed behind the
    // hook's back — keeps one record.
    storage::save_position(&mut ctx, users[0], m, &open(1), AccountUpdateReason::Adjustment)
        .unwrap();
    {
        use crate::host::PerpHost as _;
        ctx.live().set_position(users[0], m, crate::margin::margin_type::never_set_position());
    }
    storage::save_position(&mut ctx, users[0], m, &open(1), AccountUpdateReason::Adjustment)
        .unwrap();
    assert_eq!(blob(&mut ctx), packed(&[users[0]]), "never listed twice");
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
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
            maker_fee_ppm: 100,
            taker_fee_ppm: 0,
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
                    flags: 0,
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
    // construction; `margin::view::tests` covers them at a live mark.)
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
            maker_fee_ppm: 20_000,
            taker_fee_ppm: 0,
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
    let info = crate::margin::view::compute_margin_info(&mut ctx, BOB, MARKET_ID).unwrap();
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
    assert!(!crate::margin::view::derived_can_afford(
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
    let i = crate::margin::view::compute_margin_info(&mut ctx, BOB, MARKET_ID).unwrap();
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
/// cleared the same way since cross P3 (owner decision Q8, `risk::negative_balance`): BOB's close
/// is his LAST position, so the end of the call that made him flat has the insurance fund cover
/// the deficit and publishes an `INSURANCE_CLEAR` row (with a position still open the deficit
/// would stand as a receivable, `perp_wallet_balance: i64`).
///
/// **This state is the reason `AccountBalanceChanged` had to become signed.** The event's wallet
/// field used to be `uint64 perpWalletBalance` and floored at 0, so the very deficit this test
/// produces was invisible to anyone watching only the event stream — the field reported `0` while
/// the ledger held `−12_000`. The assertions below pin the fixed behaviour: the call's row for the
/// close reports `totalCrossWalletBalance == −12_000` unclamped (a hypothetical clamped reading
/// would have said `0`), and the clearance row that follows it reports the cleared 0.
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
            maker_fee_ppm: 20_000,
            taker_fee_ppm: 0,
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
            ..crate::margin::margin_type::never_set_position()
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
        2,
        "the close's row, drained at the end, then the clearance's own row"
    );
    assert_eq!(
        bob_events[0].totalCrossWalletBalance, -maker_fee,
        "the wallet went negative by EXACTLY the commission and no more — no margin shortfall \
         reaches it any more (that is the M1 cap), only a fee with nothing left to absorb it. The \
         event reports the DEFICIT: under the retired `uint64 perpWalletBalance` this read 0 and \
         an operator watching only the event stream could not see it."
    );
    assert_eq!(
        crate::types::UserAccount {
            perp_wallet_balance: bob_events[0].totalCrossWalletBalance,
            ..crate::types::UserAccount::default()
        }
        .visible_perp_wallet_balance(),
        0,
        "what a CLAMPED reading would have said — kept as the contrast, used by no published surface"
    );
    // BOB is flat with no orders, so he has left the market index entirely: the event's totals are
    // an EMPTY fold plus the raw wallet.
    assert_eq!(
        bob_events[0].totalWalletBalance, -maker_fee,
        "no silos left"
    );
    // ── Q8: flat with a negative wallet ⇒ the insurance fund clears it, in its own row ──────────
    assert_eq!(
        bob_events[1].reason,
        AccountUpdateReason::InsuranceClear.code(),
        "Binance's INSURANCE_CLEAR"
    );
    assert_eq!(
        (bob_events[1].totalCrossWalletBalance, bob_events[1].totalWalletBalance),
        (0, 0)
    );
    assert_eq!(
        storage::load_account(&mut ctx, BOB)
            .unwrap()
            .perp_wallet_balance,
        0,
        "cleared"
    );
    assert_eq!(
        get_account(&mut ctx, BOB).availableBalance,
        0,
        "no ooIM left, nothing owed any more"
    );
    assert_event_matches_get_account(&mut ctx, &bob_events[1]);
    // Not a mint: the fee recipient really was paid, the insolvent close's beyond-margin slice
    // really did reach the fund, and the fund then paid the commission gap between the two.
    assert_eq!(wallet(&mut ctx, ADMIN), maker_fee as u64);
    assert_eq!(
        if_before - storage::load_insurance_fund(&mut ctx).unwrap(),
        66_667 + maker_fee as u64,
        "loss 400_000 − released margin 333_333, plus the cleared commission"
    );
    // Money-out stays shut on the cleared (zero) wallet.
    assert!(!crate::margin::view::derived_can_afford(
        available(&mut ctx, BOB),
        1
    ));
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
            ..crate::margin::margin_type::never_set_position()
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
            maker_fee_ppm: 0,
            taker_fee_ppm: 10_000,
        },
    )
    .unwrap();
    storage::save_user_fee_rates(
        &mut ctx,
        BOB,
        UserFeeRates {
            maker_fee_ppm: 20_000,
            taker_fee_ppm: 0,
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
            maker_fee_ppm: 0,
            taker_fee_ppm: 10_000,
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
/// * `removePositionMargin` sees the shortfall and nothing else: after `addPositionMargin(X)` exactly
///   `X − fee` comes back out and the full `X` is refused (2b — Binance's withdrawal cap is equity
///   against PIM, so the fee the silo is short by stays in);
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
            maker_fee_ppm: 0,
            taker_fee_ppm: 10_000,
        },
    )
    .unwrap();
    let fee = FILL_VALUE * 100 / 10_000;

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // real taker open, leverage 1

    // ── the §3.9 shape, produced not written ──
    let i = crate::margin::view::compute_margin_info(&mut ctx, ALICE, MARKET_ID).unwrap();
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

    // ── 2b: an add/remove round trip returns the deposit LESS the fee shortfall ──
    // The withdrawal cap is `iw + uPnL − PIM` here (the `iw − MM` term is far looser at 1×), and
    // the silo starts one fee below PIM, so `addPositionMargin(X)` makes exactly `X − fee`
    // withdrawable. The full `X` is refused whole.
    let margin_before = pos(&mut ctx, ALICE).margin;
    let wallet_before = wallet(&mut ctx, ALICE);
    let remove = |ctx: &mut TestCtx, amount: u64| {
        crate::risk::run_remove_position_margin(
            &removePositionMarginCall {
                marketId: MARKET_ID,
                amount,
            }
            .abi_encode(),
            ALICE,
            ctx,
        )
    };
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
    let err = remove(&mut ctx, 100_000).unwrap_err();
    assert!(
        err.to_string()
            .contains(crate::risk::REMOVE_MARGIN_G3_REJECT),
        "{err}"
    );
    assert!(remove(&mut ctx, 100_000 - fee + 1).is_err());
    remove(&mut ctx, 100_000 - fee).unwrap();
    assert_eq!(pos(&mut ctx, ALICE).margin, INIT_MARGIN as i64, "the silo lands exactly on PIM");
    assert_eq!(pos(&mut ctx, ALICE).margin, margin_before + fee as i64);
    assert_eq!(wallet(&mut ctx, ALICE), wallet_before - fee);

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
            maker_fee_ppm: 20_000,
            taker_fee_ppm: 0,
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
            maker_fee_ppm: 0,
            taker_fee_ppm: 10_000,
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
            maker_fee_ppm: 0,
            taker_fee_ppm: 100_000, // 10% — the tightened ceiling
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
            maker_fee_ppm: 20_000,
            taker_fee_ppm: 0,
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
            flags: 0,
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
            ..crate::margin::margin_type::never_set_position()
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
            // The flip ENDED the short's life: a new generation (see `PerpPosition::generation`).
            generation: 1,
            ..crate::margin::margin_type::never_set_position()
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
            ..crate::margin::margin_type::never_set_position()
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
            // The FLIP's accumulator: exactly the closing leg's PnL asserted above, and nothing
            // for the opening leg — which is the apportionment R15 measured (`rp` covers only the
            // close). This position is a fresh LONG whose `v_quote_balance` carries no trace of the
            // short it replaced, so a `cr` of 0 here would be a lost round.
            cumulative_realized_pnl: 100_000,
            // The flip ENDED the short's life: a new generation (see `PerpPosition::generation`).
            generation: 1,
            ..crate::margin::margin_type::never_set_position()
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
            ..crate::margin::margin_type::never_set_position()
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

// ── `Trade.takerRealizedPnl` / `Trade.makerRealizedPnl` — the `@order` stream's `o.rp` ──────────
//
// See the ABI note on `Trade` in `crate::interface` for what these fields are and why they are on
// this event. What the tests below pin is the arithmetic:
//
//   * the TAKER's aggregate is split N ways by telescoping differences and the split has ZERO
//     residual — on a fixture where the naive independent-per-fill split would be off by +1;
//   * the acceptance identity `Δwb == rp − n`, at the granularity each side actually publishes:
//     PER FILL for the maker (one account group per fill), PER ORDER for the taker (one group per
//     order), measured on the GROSS wallet `wb = cross + Σ isolatedWallet` so margin moving
//     cross→silo is invisible to it and only money entering/leaving the account moves it;
//   * the flip case, where the opening legs must report 0;
//   * the insolvent case, where the identity acquires its ONE correction term.

/// Every `Trade` in `logs`, in emission order.
///
/// Takes a SNAPSHOT rather than draining the journal, because these tests need the `Trade` rows and
/// the `PositionChanged` rows of the SAME call — `take_logs` empties the buffer, so calling two
/// draining helpers in a row silently hands the second an empty stream.
fn decode_trades(logs: &[primitives::Log]) -> Vec<crate::interface::IPerpDex::Trade> {
    logs.iter()
        .filter(|log| {
            log.data.topics().first() == Some(&crate::interface::IPerpDex::Trade::SIGNATURE_HASH)
        })
        .map(|log| {
            crate::interface::IPerpDex::Trade::decode_raw_log(log.data.topics(), &log.data.data)
                .unwrap()
        })
        .collect()
}

/// `PositionChanged` counterpart of [`decode_trades`], over the same snapshot.
fn decode_position_changes(
    logs: &[primitives::Log],
) -> Vec<crate::interface::IPerpDex::PositionChanged> {
    logs.iter()
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

/// `AccountBalanceChanged.totalWalletBalance` for `user`, re-derived from RAW STORED STATE rather
/// than read off an event: Binance's `wb`, the GROSS perp wallet, `cross + Σ isolatedWallet`.
///
/// Single-market tests only — `Σ isolatedWallet` is this market's `pos.margin`. The callers below
/// cross-check it against the emitted event, so a divergence between this and the real producer
/// (`margin::view::index_account_wallet_balances`) cannot hide.
fn wb(ctx: &mut TestCtx, user: Address) -> i64 {
    storage::load_account(ctx, user)
        .unwrap()
        .perp_wallet_balance
        + storage::load_position(ctx, user, MARKET_ID).unwrap().margin
}

/// THE APPORTIONMENT, on a fixture built so that the division actually truncates.
///
/// ALICE is long 3 lots at a NON-UNIFORM basis — `v_quote_balance = −3_010_000` over
/// `pos_abs = 3_000_000`, i.e. `−1_003_333.33…` per lot — and closes all three in one market sell
/// that sweeps three separate resting bids at $102 / $101 / $100.
///
/// That basis is the whole point: `vq / pos_abs` is not an integer, so `vq × rem_q / pos_abs`
/// genuinely floors at every step, and the naive split (each fill computing its own
/// `vq × q_k / pos_abs`) floors THREE TIMES to a sum that is +1 away from the aggregate. This
/// fixture therefore DISCRIMINATES between the two implementations rather than merely agreeing
/// with both — the test asserts that explicitly below, so a later edit that makes the fixture
/// uniform (and the residual vacuously zero) fails here.
#[test]
fn trade_apportions_taker_realized_pnl_by_telescoping_with_zero_residual() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);

    // 10 bps taker / 5 bps maker, so the `n` in `Δwb == rp − n` is a real number and not 0.
    // Maker rates are snapshotted onto the order entry AT PLACEMENT, so they must be set before
    // the resting orders go in.
    storage::save_user_fee_rates(
        &mut ctx,
        ALICE,
        UserFeeRates {
            maker_fee_ppm: 0,
            taker_fee_ppm: 1_000,
        },
    )
    .unwrap();
    for maker in [BOB, CAROL] {
        storage::save_user_fee_rates(
            &mut ctx,
            maker,
            UserFeeRates {
                maker_fee_ppm: 500,
                taker_fee_ppm: 0,
            },
        )
        .unwrap();
    }

    // ALICE: long 3 lots at an average basis of $100.333…, which is what makes the division bite.
    let alice_vq: i64 = -3_010_000;
    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: 3 * QTY as i64,
            v_quote_balance: alice_vq,
            margin: -alice_vq,
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    // BOB short 2 lots, CAROL short 1, both entered at exactly $100 — so their closes are the
    // simple leg and ALICE's is the one carrying the awkward basis.
    storage::save_position(
        &mut ctx,
        BOB,
        MARKET_ID,
        &PerpPosition {
            amount: -(2 * QTY as i64),
            v_quote_balance: 2 * FILL_VALUE as i64,
            margin: 2 * FILL_VALUE as i64,
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    storage::save_position(
        &mut ctx,
        CAROL,
        MARKET_ID,
        &PerpPosition {
            amount: -(QTY as i64),
            v_quote_balance: FILL_VALUE as i64,
            margin: FILL_VALUE as i64,
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();

    // Three resting bids. A sell taker takes the HIGHEST bid first, so the fill order is
    // $102 (BOB) → $101 (CAROL) → $100 (BOB). BOB deliberately takes TWO of the three fills: his
    // two account groups must each move by their OWN fill, which is what "per fill, not
    // accumulated onto the last one" means on the maker side.
    place(&mut ctx, BOB, 0, PRICE + 2 * TICK, QTY, 0, 0);
    place(&mut ctx, CAROL, 0, PRICE + TICK, QTY, 0, 0);
    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let wb_before: Vec<(Address, i64)> = [ALICE, BOB, CAROL]
        .into_iter()
        .map(|u| (u, wb(&mut ctx, u)))
        .collect();

    place(&mut ctx, ALICE, 1, 0, 3 * QTY, 1, 1); // market sell 3 lots
    end_call(&mut ctx);

    let logs = JournalTr::take_logs(ctx.journal_mut());
    let trades = decode_trades(&logs);
    assert_eq!(trades.len(), 3, "the fixture must produce exactly 3 fills");
    assert_eq!(
        trades.iter().map(|t| t.price).collect::<Vec<_>>(),
        vec![PRICE + 2 * TICK, PRICE + TICK, PRICE],
        "sell taker sweeps the book from the top"
    );

    // ── The taker split, value by value ───────────────────────────────────────────────────────
    //
    //   pos_abs = 3_000_000   vq = −3_010_000
    //   fill 1: rem_q = 2_000_000  rem_vq = −2_006_666  vqf = −1_003_334  +1_020_000 →  16_666
    //   fill 2: rem_q = 1_000_000  rem_vq = −1_003_333  vqf = −1_003_333  +1_010_000 →   6_667
    //   fill 3: rem_q =         0  rem_vq =         0   vqf = −1_003_333  +1_000_000 →  −3_333
    //                                                                          Σ  =    20_000
    //
    // Note fill 1's `vqf` is −1_003_334 while fills 2 and 3 are −1_003_333: the telescoping puts
    // the odd unit on ONE fill instead of losing it. That asymmetry IS the fix.
    assert_eq!(
        trades
            .iter()
            .map(|t| t.takerRealizedPnl)
            .collect::<Vec<_>>(),
        vec![16_666, 6_667, -3_333],
    );
    let taker_rp_sum: i64 = trades.iter().map(|t| t.takerRealizedPnl).sum();

    // ZERO RESIDUAL against the number that actually mutated state. `PositionChanged.realizedPnl`
    // is emitted from the aggregate `apply_position_fill` return, so this is the split measured
    // against the mutation, not against a second copy of the split.
    let position_changes = decode_position_changes(&logs);
    let taker_change = position_changes
        .iter()
        .find(|c| c.user == ALICE)
        .expect("taker PositionChanged");
    assert_eq!(taker_change.realizedPnl, 20_000);
    assert_eq!(
        taker_rp_sum, taker_change.realizedPnl,
        "Σ per-fill takerRealizedPnl must equal the aggregate that moved the money, EXACTLY"
    );

    // ── The fixture really does discriminate ──────────────────────────────────────────────────
    //
    // What an independent per-fill computation would have produced: each fill floors its own
    // `vq × q_k / pos_abs` = −1_003_333.33… → −1_003_333, so the three floors sum to −3_009_999
    // instead of −3_010_000 and the reported PnL totals 20_001 — a +1 phantom MINT. If this ever
    // stops differing from the aggregate, the fixture has gone uniform and the test above has
    // become vacuous.
    let naive_sum: i64 = trades
        .iter()
        .map(|t| {
            let vqf = alice_vq as i128 * QTY as i128 / (3 * QTY) as i128;
            let closing_value = t.price as i128 / 100_000; // calc_value(price, QTY, 8, 9)
            (vqf + closing_value) as i64
        })
        .sum();
    assert_eq!(naive_sum, 20_001, "the naive split's value, for the record");
    assert_ne!(
        naive_sum, taker_change.realizedPnl,
        "fixture no longer discriminates telescoping from independent flooring"
    );

    // ── `Δwb == Σrp − Σn` for the TAKER, at ORDER granularity ─────────────────────────────────
    //
    // The taker publishes ONE account group per order, so the order-level form is the only one its
    // stream can state. (The per-fill split is covered by the equality above and by the
    // `debug_assert` inside `finalize_core`, which is live in this very test run.)
    let alice_before = wb_before.iter().find(|(u, _)| *u == ALICE).unwrap().1;
    let taker_fee_sum: i64 = trades.iter().map(|t| t.takerFee as i64).sum();
    assert_eq!(taker_fee_sum, 3_030, "10 bps on 3_030_000 of notional");
    assert_eq!(
        wb(&mut ctx, ALICE) - alice_before,
        taker_rp_sum - taker_fee_sum,
        "taker: Δwb == Σrp − Σn"
    );

    // ── `Δwb == rp − n` for the MAKERS, at FILL granularity ───────────────────────────────────
    //
    // Each maker settles once per fill, so this is the per-fill statement the brief calls for.
    // CAROL has one fill, so her call-level delta IS her fill-level delta. BOB has two, and they
    // are checked as a pair against the sum of his two fills — his intermediate after-image is
    // pinned separately, below, so "his two fills" is not being allowed to hide a mis-split.
    for maker in [BOB, CAROL] {
        let before = wb_before.iter().find(|(u, _)| *u == maker).unwrap().1;
        let rp: i64 = trades
            .iter()
            .filter(|t| t.maker == maker)
            .map(|t| t.makerRealizedPnl)
            .sum();
        let n: i64 = trades
            .iter()
            .filter(|t| t.maker == maker)
            .map(|t| t.makerFee as i64)
            .sum();
        assert_eq!(
            wb(&mut ctx, maker) - before,
            rp - n,
            "maker {maker:?}: Δwb == Σ(rp − n) over its fills"
        );
    }

    // The individual per-fill maker values, so the sum above cannot pass on a compensating error.
    // BOB closes 1 of 2 lots at $102 (basis $100) → −20_000, then his last lot at $100 → exactly 0.
    // CAROL closes her only lot at $101 → −10_000.
    assert_eq!(
        trades
            .iter()
            .map(|t| (t.maker, t.makerRealizedPnl, t.makerFee))
            .collect::<Vec<_>>(),
        vec![(BOB, -20_000, 510), (CAROL, -10_000, 505), (BOB, 0, 500)],
    );

    // Each fill's `makerRealizedPnl` is the SAME number as that fill's maker `PositionChanged`
    // (the maker settles per fill, so no apportionment is involved on that side — this is the
    // plumbing check).
    let maker_rps: Vec<i64> = position_changes
        .iter()
        .filter(|c| c.user != ALICE)
        .map(|c| c.realizedPnl)
        .collect();
    assert_eq!(maker_rps, vec![-20_000, -10_000, 0]);
}

/// A FLIP: the closing legs earn `rp`, the opening legs report exactly 0 — matching R15's
/// "`rp` covers only the closing leg". This is the case that would break if the apportionment
/// walked a running position instead of `remaining_closing_qty` against the original `pos_abs`.
#[test]
fn trade_taker_realized_pnl_is_zero_on_the_opening_legs_of_a_flip() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    fund(&mut ctx, CAROL, WALLET);

    // ALICE is long 1 lot and sells 3 — one lot closes, two open a short.
    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -(FILL_VALUE as i64),
            margin: FILL_VALUE as i64,
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();

    // Three bids, one lot each, so the closing leg is fill 1 and fills 2-3 are pure opens.
    place(&mut ctx, BOB, 0, PRICE + 2 * TICK, QTY, 0, 0);
    place(&mut ctx, CAROL, 0, PRICE + TICK, QTY, 0, 0);
    place(&mut ctx, BOB, 0, PRICE, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    place(&mut ctx, ALICE, 1, 0, 3 * QTY, 1, 1);
    end_call(&mut ctx);

    let logs = JournalTr::take_logs(ctx.journal_mut());
    let trades = decode_trades(&logs);
    assert_eq!(trades.len(), 3);
    // Fill 1 closes the long at $102 against a $100 basis → +20_000. Fills 2 and 3 open the new
    // short and realise NOTHING, even though they trade at prices that would show a "profit"
    // against the old basis if the apportionment had let the entry basis follow the position.
    assert_eq!(
        trades
            .iter()
            .map(|t| t.takerRealizedPnl)
            .collect::<Vec<_>>(),
        vec![20_000, 0, 0],
    );
    let position_changes = decode_position_changes(&logs);
    let taker_change = position_changes.iter().find(|c| c.user == ALICE).unwrap();
    assert_eq!(taker_change.realizedPnl, 20_000);
    assert_eq!(taker_change.closedQuantity, QTY);
    assert_eq!(
        trades.iter().map(|t| t.takerRealizedPnl).sum::<i64>(),
        taker_change.realizedPnl,
    );
    // The makers were all flat and are pure opens: no `rp` on their side either.
    assert!(trades.iter().all(|t| t.makerRealizedPnl == 0));
}

/// AN INSOLVENT CLOSE — the one case where `Δwb == rp − n` does NOT hold, and the correct
/// statement.
///
/// The identity was written here with NO exemption first, and this is what the engine said: it
/// fails, and it fails by EXACTLY the bad debt. The reason is structural rather than a rounding
/// artefact. `rp` is the GROSS close PnL; `wb` is money actually in the account. Under isolated
/// margin a realized loss can only be paid out of the closed slice's own collateral — never the
/// wallet, never another position (`apply_position_fill`'s insolvent branch) — so the part of the
/// loss that exceeds that collateral never leaves `wb` at all. It leaves the INSURANCE FUND. So:
///
/// ```text
///     Δwb == rp − n + bad_debt
/// ```
///
/// with `bad_debt == 0` on every solvent fill, which is why R15 measured the plain form 10/10 on
/// mainnet: its sample contained no insolvent close. The plain identity is therefore correct as an
/// acceptance test for solvent flow and must NOT be applied blind — a backend reconciling `wb`
/// against `Σ rp` has to source the correction.
///
/// ⚠️ AND IT CANNOT SOURCE IT FROM `Trade`. Bad debt is not on this event, and the insurance-fund
/// events that carry it (`InsuranceFundChanged`, `InsuranceFundDepleted`) name NO user — so a
/// per-account `wb` reconciliation cannot attribute the correction from the log stream today. That
/// is a REAL remaining gap in the `@order`/`ACCOUNT_UPDATE` reconstruction, but it is a different
/// field on a different event and is deliberately NOT fixed here; it is recorded rather than
/// papered over. (Binance has the same shape: the shortfall shows up as a separate
/// `INSURANCE_CLEAR` income row, not as an adjustment to `rp`.)
#[test]
fn insolvent_close_breaks_the_wallet_identity_by_exactly_the_bad_debt() {
    let mut ctx = make_ctx();
    setup(&mut ctx);
    // Fund the IF so the bad debt is ABSORBED (a visible debit) rather than merely reported as
    // `InsuranceFundDepleted` against an empty fund — this test measures the correction term, so
    // the term has to actually move somewhere.
    storage::save_insurance_fund(&mut ctx, 10_000_000).unwrap();

    // ALICE: long 1 lot at $100 on 10x — only 100_000 of collateral behind a 1_000_000 notional.
    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -(FILL_VALUE as i64),
            margin: (FILL_VALUE / 10) as i64,
            leverage: 10,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();

    // She closes at $50: a 500_000 loss against 100_000 of margin → 400_000 of bad debt.
    let crash = PRICE / 2;
    place(&mut ctx, BOB, 0, crash, QTY, 0, 0);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let alice_before = wb(&mut ctx, ALICE);
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    place(&mut ctx, ALICE, 1, 0, QTY, 1, 1);
    end_call(&mut ctx);

    let logs = JournalTr::take_logs(ctx.journal_mut());
    let trades = decode_trades(&logs);
    assert_eq!(trades.len(), 1);
    let t = &trades[0];
    assert_eq!(
        t.takerRealizedPnl, -500_000,
        "bad debt changes where the money comes from, NOT the realized PnL"
    );

    // The bad debt is real and went to the insurance fund.
    let bad_debt = if_before as i64 - storage::load_insurance_fund(&mut ctx).unwrap() as i64;
    assert_eq!(bad_debt, 400_000);

    let delta_wb = wb(&mut ctx, ALICE) - alice_before;
    let rp_minus_n = t.takerRealizedPnl - t.takerFee as i64;

    // The unexempted identity FAILS here — asserted as a fact, so that if the engine ever starts
    // routing an insolvent close through the wallet this test notices.
    assert_ne!(
        delta_wb, rp_minus_n,
        "an insolvent close is exactly where the plain identity must fail"
    );
    // …and the corrected one holds, exactly.
    assert_eq!(delta_wb, rp_minus_n + bad_debt, "Δwb == rp − n + bad_debt");
    // Concretely: only the 100_000 that was actually in the silo left the account.
    assert_eq!(delta_wb, -100_000 - t.takerFee as i64);
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
            ..crate::margin::margin_type::never_set_position()
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

/// A FOK taker is the one kind self-trade prevention does not apply to (Binance: "STP won't take
/// effect"), so it still SELF-FILLS — and the taker leg is settled from the maker leg's evolved
/// working copy, which is what this pins: the round trip nets to exactly flat.
#[test]
fn self_trade_finalizes_taker_from_latest_maker_state() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let sell_id = place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);
    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 2);

    assert_terminal(&mut ctx, sell_id);
    assert_terminal(&mut ctx, buy_id);
    assert_eq!(wallet(&mut ctx, ALICE), WALLET);
    assert_eq!(
        pos(&mut ctx, ALICE),
        PerpPosition {
            leverage: 1,
            ..crate::margin::margin_type::never_set_position()
        }
    );
}

/// The same self-cross as a GTC: under the default `EXPIRE_MAKER` it no longer trades. The resting
/// sell is cancelled (reason 9), and the buy rests for its whole quantity — no position, no money.
#[test]
fn a_gtc_self_cross_cancels_the_resting_order_and_rests_instead() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let sell_id = place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);
    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    assert_terminal(&mut ctx, sell_id);
    let buy = get_order(&mut ctx, buy_id);
    assert_eq!((buy.status, buy.filled), (OrderStatus::Open, 0));
    assert_eq!(storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap(), vec![PRICE]);
    assert!(storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap().is_empty());
    assert_eq!(wallet(&mut ctx, ALICE), WALLET);
    assert_eq!(pos(&mut ctx, ALICE).amount, 0);
    assert_eq!(pos(&mut ctx, ALICE).total_sell_qty, 0, "the cancelled sell left the Ask");
}

/// FOK, so the taker self-fills (STP does not apply to FOK) — the subject is the margin cover
/// around a self-match, not self-trade prevention.
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

    let taker_buy = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 2);

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
            ..crate::margin::margin_type::never_set_position()
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
        assert_eq!(pos(&mut ctx, ALICE), crate::margin::margin_type::never_set_position());
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
        assert_eq!(pos(&mut ctx, ALICE), crate::margin::margin_type::never_set_position());
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
        FundingSettled, InsuranceFundChanged, InsuranceFundDepleted, OrderCancelled, OrderExpired,
        OrderPlaced, OrderRested, PositionChanged, Trade,
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
                ("OrderExpired", OrderExpired::SIGNATURE_HASH),
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
        vec!["OrderPlaced", "OrderExpired"],
        "an accepted IOC that expired unfilled emits its OrderPlaced and then its terminal \
         OrderExpired — and in THAT order, which is why the expiry is emitted at \
         place_order_core's tail rather than at either cancel_unfilled_remainder call site (both \
         run BEFORE the fallback OrderPlaced flush, so emitting there would put the terminal \
         event first)"
    );

    // Same for a market order with no book.
    place(&mut ctx, ALICE, 0, 0, QTY, 1, 1); // Market/IOC
    let logs = take_event_names(&mut ctx);
    assert_eq!(
        logs,
        vec!["OrderPlaced", "OrderExpired"],
        "an accepted market order that expired unfilled reports the same pair"
    );
}

// ── `OrderExpired`: the terminal event of a TIF expiry ───────────────────────────────────────────
//
// THE GAP IT CLOSES: an IOC or market order that PARTIALLY filled used to end with no terminal
// event at all. `cancel_unfilled_remainder` set `OrderStatus::Expired` in memory, delete-on-terminal
// removed the record so `getOrder` answered "not found", and the stream read
// `OrderPlaced → Trade × N → (nothing)`.
//
// It is DERIVABLE — a consumer knows `tif`/`orderType` from `OrderPlaced` and can reason "an IOC
// never rests, so any remainder is dead" — so these tests are not about recovering lost information.
// They pin the FIRING SET, which is the thing a consumer would otherwise have to reimplement as
// engine policy: exactly the two paths that discard a remainder, and none of the four that look like
// they might.
mod order_expired;

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
//   cargo test -p perp-engine --release perf_ -- --ignored --nocapture --test-threads=1
//
// Methodology: each scenario does an explicit warmup, then accumulates
// per-section `Instant` deltas over enough iterations for >=100ms of timed
// work. Steady state: scenarios that would otherwise grow the order book
// pair every "add" with a corresponding "drain" (cancel or full fill) so the
// book returns to empty every iteration. Order blobs and per-user nonces do
// accumulate in the block-scoped perp overlay (a realistic in-block effect);
// positions/wallets drift monotonically (wallets are pre-funded huge so no
// margin rejection ever triggers).
mod perf;

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
            ..crate::margin::margin_type::never_set_position()
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
//   * a funding charge driving a position's margin NEGATIVE (a debt), and the
//     signed close / ADL that nets it (the scenario's funding charge is fully
//     covered by ALICE's position margin; the debt paths are covered by
//     risk::tests)
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
pub(crate) mod golden;

// ── commit-only #23 conservation repro (realisticMix-shaped, EVM-framed) ─────────
#[cfg(test)]
mod commit_only_conservation;

// ── Batch cancel (Phase 1) ─────────────────────────────────────────────────
//
// Covers the batch SHELL that `batchPlaceOrders` (Phase 2) reuses: pre-decode length validation,
// dynamic gas, the numeric reason codes, the index-aligned 34-byte status blob, the abort-forward
// driver's runtime genuine-vs-abort classification, and the one-signature-per-batch replay guard.
mod batch_cancel;

// ── Batch place (Phase 2) ──────────────────────────────────────────────────
//
// Reuses the Phase 1 shell verbatim (pre-decode length validation, dynamic gas, the 34-byte
// index-aligned status blob, abort-forward). What is NEW and therefore what these tests hammer:
// the orderId/nonce rule (`save_order` has no collision guard, so a wrong rule is silent state
// corruption), the Accepted-vs-Filled tag split, the place-path reason codes, the 224-byte element
// stride, and the Phase 0 synergy that makes a batch safe at all — a rejected item emits no
// `OrderPlaced`.
mod batch_place;

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
mod risk_reducing_admission;

// ── Per-user market index (derived-ooIM Phase 0) ─────────────────────────────
//
// The index (`umkt`) answers "which markets is this user active in?", where ACTIVE means a
// non-zero position OR at least one resting order. Nothing reads it yet — these tests are the
// whole proof that it is maintained correctly, so the centrepiece is the ground-truth property
// test: after EVERY step of a randomised operation sequence, the STORED index of each user must
// equal the set recomputed independently from the position + order lists.
mod user_market_index;

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
mod derived_ooim_divergence;

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
mod assuming_price;

// ── `OrderRested.assumingPrice` ⇒ `getPositionRisk`'s `bidNotional` / `askNotional` ──────────────
//
// The `@order` websocket stream carries `b` / `a` — "this order's contribution to your resting bid /
// ask notional" — and that contribution is `restingQuantity × the order's FROZEN Assuming Price`.
// The quantity was already on `OrderRested`; the Assuming Price was emitted NOWHERE, so an indexer
// had to re-derive `max(⌈lastTraded × 1.0015⌉, mark, limit)` for a sell and snapshot it at the
// instant of placement — i.e. reimplement a chain rule, off chain, including its TIMING. The
// evaluation timing of exactly that rule is what leaked state on val0 (there used to be a second
// resolution of `T` with `save_last_traded_price` between the two; see THE FREEZE in
// `super::rest_in_book`), which makes it the last rule anyone should be reimplementing downstream.
//
// So the field is emitted, and this module is the reason it is worth emitting rather than deriving:
// it builds the indexer, feeding it NOTHING BUT THE LOG STREAM, and asserts its Σ per side equals
// what `getPositionRisk` reports on the same state. If the field were wrong, absent or re-resolved,
// the two would part.
mod resting_notional_stream;

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
mod account_snapshot_events;

// ── `PositionChanged`'s derived fields = `ACCOUNT_UPDATE.a.P[]` ───────────────────────────────
//
// `entryPrice` (`ep`) and `unrealizedProfit` (`up`) are DERIVED at emit time by the single
// `crate::events` helper every one of the seven emit sites calls; `cumulativeRealizedPnl` (`cr`)
// and `breakevenPrice` (`bep`) are placeholders. These pin all four.
mod position_changed_derived_fields;

// ── Signed-call replay guards ────────────────────────────────────────────────────
//
// Pins the rule that `placeOrderSigned` and `setLeverageSigned` spend a signature by SUBMISSION,
// not by success. Before this, `placeOrderSigned` burned only on success and `setLeverageSigned`
// never burned at all — both left a signature usable for the rest of its recv window (≤65s).
#[cfg(test)]
mod signed_replay;

// ── reduce-only, end to end ──────────────────────────────────────────────────────
//
// The unit tests in `crate::trading::reduce_only` pin the predicate as arithmetic. These drive it
// through
// the real selector: admission, silent truncation, and the eviction trigger that has nothing to do
// with the position (a normal order resting AHEAD).
#[cfg(test)]
mod reduce_only_e2e;


// ── The ABI doc's byte widths, enforced ──────────────────────────────────────
//
// `interface.rs` spells every signed message out as `prefix || field(N) || ...`. External signers
// build their digests from exactly those numbers and have no code to check them against, so the
// widths there are load-bearing documentation.
//
// They have gone stale three times — once per field added — and each time it was caught by a
// human reading two descriptions of the same thing side by side. This parses the doc and checks it
// against the real message lengths, so the fourth time is caught by `cargo test`.
#[cfg(test)]
mod abi_message_layout;

// ── Per-market open-order cap ───────────────────────────────────────────────────────────────
//
// `algo::MAX_OPEN_ORDERS_PER_MARKET` (200): book buys + book sells + untriggered conditionals per
// (user, market), checked pre-match for an order that can REST and for `placeAlgoOrder`. Every
// refusal below goes through the full call shell, whose commit-only tripwire `debug_assert!`s on a
// revert that left a perp write behind — so reaching the post-reject assertions at all already
// says the reject was write-clean; the explicit write-count / nonce / book checks say it again.
#[cfg(test)]
mod open_order_cap;

// ── Self-trade-prevention ENCODING (flags bits 3:2) ──────────────────────────────────────────
//
// The mode is decoded, validated, stored on `Order` / `OrderEntry` and echoed. `00` IS
// EXPIRE_MAKER, so an echo site that dropped the stored mode would not fail — it would silently
// report EXPIRE_MAKER. Hence every test here uses the two NON-default modes as well.
#[cfg(test)]
mod stp_encoding;

// ── Self-trade prevention in the match walk (plan §2, T1–T23b) ────────────────────────────────
//
// Every case that walks the book runs on BOTH sides. A buy taker walking asks upward and a sell
// taker walking bids downward share no code below `match_order`'s `match side`, so a mutation that
// touches only one walk is caught by the other side's run. Every taker goes through the full call
// shell, so the `OrderCancelled ⇒ prior OrderRested`, `OrderExpired ⇒ born here, never rested` and
// STP-attribution guards (`events.rs`), the account-update grouping guard, the write-then-revert
// tripwire and the end-of-call `AccountBalanceChanged` drain all watch it.
#[cfg(test)]
mod stp_matching;

/// Self-trade prevention and the per-market open-order cap, written independently from the plan
/// rather than from this implementation.
#[cfg(test)]
mod stp_spec;

/// Trading-fee accounting (taker fee attribution) through the full call shell.
#[cfg(test)]
mod fee_spec;

/// Maker rebates and ppm fee rates, written independently from the spec rather than from this
/// implementation (or its `trading::tests::fee_spec`).
#[cfg(test)]
mod rebate_spec;

/// Cross margin P2b: money movement on a CROSSED position — fills, funding, liquidation and ADL
/// next to cross positions, J6 and the negative-wallet audit — under the custody identity.
#[cfg(test)]
mod cross_settlement;
