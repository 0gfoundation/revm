use super::*;
use alloy_sol_types::SolCall;
use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, Address, FixedBytes, U256};

use crate::perp_dex::{
    interface::IPerpDex::{cancelOrderCall, getMarketFeeTotalCall, getOrderCall, placeOrderCall},
    storage,
    types::{FundingState, Market, OrderStatus, PerpPosition, UserFeeRates},
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
    storage::save_account(ctx, user, acc).unwrap();
}

fn wallet(ctx: &mut TestCtx, user: Address) -> u64 {
    storage::load_account(ctx, user)
        .unwrap()
        .visible_perp_wallet_balance()
}

fn pos(ctx: &mut TestCtx, user: Address) -> PerpPosition {
    storage::load_position(ctx, user, MARKET_ID).unwrap()
}

/// Deterministic distinct test user address from a small index (avoids ALICE/BOB/CAROL/ADMIN).
fn user_addr(i: u64) -> Address {
    let mut b = [0u8; 20];
    b[12..20].copy_from_slice(&i.to_be_bytes());
    Address::from(b)
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

fn get_order(ctx: &mut TestCtx, id: [u8; 32]) -> crate::perp_dex::types::Order {
    storage::load_order(ctx, &id).unwrap().unwrap()
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

// ── Regression: unbounded tick-walk over wide gaps (#21, was the TICK_WALK_CAP sparse-book bug) ──

/// SERIAL repro of the server `perpdex-parallel-fail-f9e3c6b37` regression: a taker that must sweep
/// across a wide price gap. The old TICK_WALK_CAP=4096 bound treated a level > CAP ticks away as
/// empty, so the taker stopped early, the far maker was orphaned, and the taker's remainder rested —
/// crossing the book (realisticMix scatters levels tens of thousands of ticks apart; the other tests
/// cluster levels 1 tick apart so it never fired locally). Fixed by making discovery UNBOUNDED over
/// the occupied range. This was a SERIAL semantic bug (the tick-walk is shared by both paths), so the
/// repro runs on the plain serial place/match path. The gap here (WIDE_GAP_TICKS) is deliberately far
/// past the old cap.
#[test]
fn taker_sweeps_across_wide_gap() {
    const WIDE_GAP_TICKS: u64 = 8_192; // 2x the old TICK_WALK_CAP — would orphan the far level pre-fix
    let mut ctx = make_ctx();
    setup(&mut ctx);
    // Generous funding: the far level sits at a high price → large notional/margin.
    fund(&mut ctx, ALICE, 1_000_000_000);
    fund(&mut ctx, BOB, 1_000_000_000);

    let near = PRICE;
    let far = PRICE + WIDE_GAP_TICKS * TICK;

    // BOB rests two asks: one at best (near), one far above.
    place(&mut ctx, BOB, 1, near, QTY, 0, 0); // Sell GTC @ near
    place(&mut ctx, BOB, 1, far, QTY, 0, 0); // Sell GTC @ far
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), near);

    // ALICE buys 2*QTY at a limit that crosses BOTH asks → serial FIFO sweeps both.
    place(&mut ctx, ALICE, 0, far, QTY * 2, 0, 0);

    // The far ask must have been consumed; ALICE must not have rested a leftover bid (crossed book).
    let far_ask = storage::load_ask_level(&mut ctx, MARKET_ID, far).unwrap();
    assert!(
        far_ask.is_empty(),
        "far ask ({WIDE_GAP_TICKS} ticks above best) must be swept, but {} order(s) remain",
        far_ask.len()
    );
    let far_bid = storage::load_bid_level(&mut ctx, MARKET_ID, far).unwrap();
    assert!(
        far_bid.is_empty(),
        "taker should fully fill, not rest a leftover bid at the far price (book would be crossed)"
    );
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
    assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), PRICE);
}

#[test]
fn limit_sell_rests_in_book_when_no_bid() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let id = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // GTC limit sell
    assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Open);
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), PRICE);
}

#[test]
fn resting_buy_reserves_margin_from_perp_wallet() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    // buy_side_margin_reserved = calc_value(PRICE, QTY, 8, 9) / leverage(1) = FILL_VALUE
    assert_eq!(pos(&mut ctx, ALICE).buy_side_margin_reserved, INIT_MARGIN);
    assert_eq!(pos(&mut ctx, ALICE).fee_reserved, MAKER_FEE);
    assert_eq!(wallet(&mut ctx, ALICE), WALLET - INIT_MARGIN - MAKER_FEE);
}

#[test]
fn resting_sell_reserves_margin_from_perp_wallet() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);

    // sell_side_margin_reserved = calc_value(PRICE, QTY, 8, 9) / leverage(1) = FILL_VALUE
    assert_eq!(pos(&mut ctx, BOB).sell_side_margin_reserved, INIT_MARGIN);
    assert_eq!(pos(&mut ctx, BOB).fee_reserved, MAKER_FEE);
    assert_eq!(wallet(&mut ctx, BOB), WALLET - INIT_MARGIN - MAKER_FEE);
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
        },
    )
    .unwrap();
    fund(&mut ctx, ALICE, 200_000_000);

    let price = 12_345; // $123.45 with price_decimals = 2.
    let qty = 100_000_000; // 1 base unit with base_decimals = 8.
    let expected_margin = 123_450_000; // $123.45 in 6-decimal quote units.

    place(&mut ctx, ALICE, 0, price, qty, 0, 0);

    assert_eq!(
        pos(&mut ctx, ALICE).buy_side_margin_reserved,
        expected_margin
    );
    let expected_maker_fee = 0;
    assert_eq!(pos(&mut ctx, ALICE).fee_reserved, expected_maker_fee);
    assert_eq!(
        wallet(&mut ctx, ALICE),
        200_000_000 - expected_margin - expected_maker_fee
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

    assert_eq!(get_order(&mut ctx, buy_id).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, sell_id).status, OrderStatus::Filled);
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), 0);
}

#[test]
fn sell_taker_fully_matches_resting_bid() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // resting bid
    let sell_id = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // taker sell

    assert_eq!(get_order(&mut ctx, buy_id).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, sell_id).status, OrderStatus::Filled);
    assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), 0);
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

#[test]
fn maker_fill_consumes_reserved_fee_instead_of_position_margin() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // resting ask reserves margin + maker fee
    let mut bob = storage::load_account(&mut ctx, BOB).unwrap();
    bob.perp_wallet_balance = 0;
    storage::save_account(&mut ctx, BOB, bob).unwrap();

    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // taker buy

    let bob_pos = pos(&mut ctx, BOB);
    assert_eq!(bob_pos.margin, INIT_MARGIN as i64);
    assert_eq!(bob_pos.fee_reserved, 0);
    assert_eq!(wallet(&mut ctx, BOB), 0);
    assert_eq!(wallet(&mut ctx, ADMIN), TAKER_FEE + MAKER_FEE);
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

#[test]
fn maker_fill_recomputes_remaining_order_margin_after_position_close() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert_eq!(pos(&mut ctx, BOB).amount, -(QTY as i64));

    let buy_id = place(&mut ctx, BOB, 0, PRICE, QTY * 2, 0, 0);
    let bob = pos(&mut ctx, BOB);
    assert_eq!(bob.buy_side_margin_reserved, INIT_MARGIN);
    assert_eq!(wallet(&mut ctx, BOB), WALLET - (INIT_MARGIN * 2));

    place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);

    let bob = pos(&mut ctx, BOB);
    assert_eq!(bob.amount, 0);
    assert_eq!(bob.margin, 0);
    assert_eq!(bob.buy_side_margin_reserved, INIT_MARGIN);
    assert_eq!(bob.margin_reserved, INIT_MARGIN);
    assert_eq!(wallet(&mut ctx, BOB), WALLET - INIT_MARGIN);
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
    storage::save_account(&mut ctx, ALICE, alice).unwrap();

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
    )
    .unwrap();
    let mut alice = storage::load_account(&mut ctx, ALICE).unwrap();
    alice.perp_wallet_balance = 0;
    storage::save_account(&mut ctx, ALICE, alice).unwrap();

    place(&mut ctx, BOB, 1, PRICE, QTY * 2, 0, 0); // resting ask
    let market_buy = place(&mut ctx, ALICE, 0, 0, QTY * 2, 1, 1);

    assert_eq!(get_order(&mut ctx, market_buy).status, OrderStatus::Filled);
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
    )
    .unwrap();
    let mut alice = storage::load_account(&mut ctx, ALICE).unwrap();
    alice.perp_wallet_balance = 0;
    storage::save_account(&mut ctx, ALICE, alice).unwrap();

    let close_price = PRICE - 10 * TICK; // $90
    let open_price = PRICE + 10 * TICK; // $110
    let open_value = 1_100_000;
    let taker_fee = 0;

    place(&mut ctx, BOB, 1, close_price, QTY, 0, 0);
    place(&mut ctx, CAROL, 1, open_price, QTY, 0, 0);
    place(&mut ctx, ALICE, 0, 0, QTY * 2, 1, 1);

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
fn taker_fill_cancels_worst_same_side_order_to_cover_opening_margin() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let high_buy_price = PRICE - TICK;
    let low_buy_price = PRICE - 2 * TICK;
    let high_buy = place(&mut ctx, ALICE, 0, high_buy_price, QTY, 0, 0);
    let low_buy = place(&mut ctx, ALICE, 0, low_buy_price, QTY, 0, 0);
    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // resting ask

    let low_buy_margin = 980_000;
    let mut alice = storage::load_account(&mut ctx, ALICE).unwrap();
    alice.perp_wallet_balance = (INIT_MARGIN + TAKER_FEE - low_buy_margin) as i64;
    storage::save_account(&mut ctx, ALICE, alice).unwrap();

    let market_buy = place(&mut ctx, ALICE, 0, 0, QTY, 1, 1);

    assert_eq!(get_order(&mut ctx, market_buy).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, low_buy).status, OrderStatus::Expired);
    assert_eq!(get_order(&mut ctx, high_buy).status, OrderStatus::Open);
    assert_eq!(wallet(&mut ctx, ALICE), 0);
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
    assert_eq!(get_order(&mut ctx, buy_id).status, OrderStatus::Filled);

    // Remaining sell still in ask book.
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), PRICE);
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
    storage::save_position(
        &mut ctx,
        BOB,
        MARKET_ID,
        &PerpPosition {
            amount: QTY as i64,
            v_quote_balance: -(FILL_VALUE as i64),
            leverage: 1,
            ..PerpPosition::default()
        },
    )
    .unwrap();
    let mut bob = storage::load_account(&mut ctx, BOB).unwrap();
    bob.perp_wallet_balance = 0;
    storage::save_account(&mut ctx, BOB, bob).unwrap();

    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    let maker = get_order(&mut ctx, sell_id);
    assert_eq!(maker.status, OrderStatus::PartiallyFilled);
    assert_eq!(maker.filled, QTY);
    assert_eq!(get_order(&mut ctx, buy_id).status, OrderStatus::Filled);
    // The remaining QTY of the maker's sell is NOT auto-expired — it stays resting.
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), PRICE);
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
    )
    .unwrap();

    // BOB rests a sell of QTY at PRICE (pure close → reserves nothing); ALICE buys it,
    // closing BOB's long at PRICE — a loss of 1e6 against a 500k margin.
    let _sell = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0);
    let buy = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert_eq!(get_order(&mut ctx, buy).status, OrderStatus::Filled);

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

#[test]
fn cross_side_flip_no_longer_triggers_maker_reserve_deficit_under_flip_aware_reservation() {
    // Regression for the flip-aware reservation (formula C = max(S + B', B + S')).
    //
    // This is the brute-force MINIMUM deficit scenario under the OLD max-of-side
    // reservation (lev=1, pos +1 -> -1, buys=[(2,1),(2,2)], sells=[(3,1),(3,2)],
    // fill the qty-2 sell at price 3). Under max-of-side the maker reserved only
    // 6e6 up front and the cross-side flip fill fired the reserve-deficit branch
    // (debiting 1e6 mid-fill). Under C the maker reserves the flip-aware worst
    // case (8e6) UP FRONT, so the same flip fill creates NO deficit — the branch
    // does not fire and no resting order is auto-cancelled.
    //   Plo = $200 (buy level, below market)
    //   Pm  = $250 (Alice opens long here against Bob)
    //   Phi = $300 (sell level, above market)  -> own book uncrossed (200<300)
    let mut ctx = make_ctx();
    setup(&mut ctx);
    // Bob (taker) funded generously. Alice now needs MORE up-front margin than
    // under max-of-side (C reserves the full flip exposure), so fund her beyond
    // the old single WALLET: total = 2 * WALLET = 20e6.
    fund(&mut ctx, BOB, WALLET * 100);
    fund(&mut ctx, ALICE, WALLET);

    let plo = 200 * TICK;
    let pm = 250 * TICK;
    let phi = 300 * TICK;

    // (1) Alice opens a long of 1*QTY at Pm by lifting Bob's resting ask.
    let _bob_open = place(&mut ctx, BOB, 1, pm, QTY, 0, 0); // Bob sells (maker)
    let alice_open = place(&mut ctx, ALICE, 0, pm, QTY, 0, 0); // Alice buys (taker)
    assert_eq!(get_order(&mut ctx, alice_open).status, OrderStatus::Filled);
    assert_eq!(pos(&mut ctx, ALICE).amount, QTY as i64, "Alice long +1");

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
    // Per-side opening notionals net to 6e6 each, so OLD max-of-side would
    // reserve only 6e6. The FLIP-AWARE reservation is strictly higher: if all 3
    // sell-lots fill, the position goes to -2 and the resting buys re-open more
    // notional, so C = max(S + B', B + S')
    //   = max(6e6 + B'(p=-2)=2e6 , 6e6 + S'(p=+4)=0) = 8e6.
    assert_eq!(pos_before.buy_side_reserved_notional, 6_000_000);
    assert_eq!(pos_before.sell_side_reserved_notional, 6_000_000);
    assert_eq!(
        pos_before.margin_reserved, 8_000_000,
        "flip-aware reservation (C) collected up front; max-of-side would be 6e6"
    );
    // 20e6 funded - 2.5e6 opening margin - 8e6 flip-aware reservation = 9.5e6.
    assert_eq!(wallet(&mut ctx, ALICE), 9_500_000);

    // (4) Bob (a DIFFERENT taker) buys 2*QTY at Phi, lifting Alice's qty-2 ask.
    //     This closes 1*QTY of Alice's long and opens 1*QTY short => FLIP to -1.
    let bob_take = place(&mut ctx, BOB, 0, phi, QTY * 2, 0, 0);
    assert_eq!(get_order(&mut ctx, bob_take).status, OrderStatus::Filled);

    // The qty-2 ask that Bob lifted is filled.
    assert_eq!(get_order(&mut ctx, alice_sell2).status, OrderStatus::Filled);

    // Position flipped sign: +1 long -> -1 short.
    let pos_after = pos(&mut ctx, ALICE);
    assert_eq!(pos_after.amount, -(QTY as i64), "sign flip +1 -> -1");
    assert_eq!(pos_after.buy_side_reserved_notional, 4_000_000);
    assert_eq!(pos_after.sell_side_reserved_notional, 3_000_000);
    // Post-fill flip-aware reservation: C = max(S + B', B + S')
    //   = max(3e6 + B'(p=-2)=2e6 , 4e6 + S'(p=+2)=0) = 5e6.
    assert_eq!(pos_after.margin_reserved, 5_000_000);

    // NO DEFICIT (the whole point of formula C):
    //   old_reserved(C)=8e6, opening_margin=3e6 => max_sustainable=8e6-3e6=5e6;
    //   new_reserved(C)=5e6 is NOT > 5e6 (it sits exactly on the tight boundary),
    //   so the ELSE branch runs: net_release = sat(8e6 - 5e6 - 3e6) = 0, with no
    //   deficit debit. The wallet receives only the +3e6 closing cashflow
    //   (2.5e6 margin returned + 0.5e6 realised PnL on the long opened @ $250
    //   and closed @ $300):  9.5e6 + 3e6 = 12.5e6.
    // (Under the OLD max-of-side path the deficit branch debited 1e6, leaving a
    //  balance 1e6 lower; C eliminates that debit.)
    assert_eq!(
        wallet(&mut ctx, ALICE),
        12_500_000,
        "no deficit debit under flip-aware reservation (max-of-side would be 1e6 lower)"
    );
    // The deficit branch never fired, so every other resting order is untouched.
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

    assert_eq!(get_order(&mut ctx, bob_id).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, carol_id).status, OrderStatus::Open);
}

#[test]
fn self_trade_finalizes_taker_from_latest_maker_state() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let sell_id = place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);
    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    assert_eq!(get_order(&mut ctx, sell_id).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, buy_id).status, OrderStatus::Filled);
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

    let old_buy_margin = 980_000;
    let mut alice = storage::load_account(&mut ctx, ALICE).unwrap();
    alice.perp_wallet_balance = (INIT_MARGIN - old_buy_margin) as i64;
    storage::save_account(&mut ctx, ALICE, alice).unwrap();

    let taker_buy = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 0);

    assert_eq!(get_order(&mut ctx, self_sell).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, bob_sell).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, taker_buy).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, old_buy).status, OrderStatus::Expired);
    assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), 0);
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

    let old_buy_margin = 980_000;
    let mut alice = storage::load_account(&mut ctx, ALICE).unwrap();
    alice.perp_wallet_balance = (INIT_MARGIN - old_buy_margin) as i64;
    storage::save_account(&mut ctx, ALICE, alice).unwrap();

    ctx.block.timestamp = U256::from(10);
    let taker_buy = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);

    assert_eq!(get_order(&mut ctx, taker_buy).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, bob_sell).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, carol_sell).status, OrderStatus::Open);
    assert_eq!(get_order(&mut ctx, old_buy).status, OrderStatus::Expired);
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

    assert_eq!(get_order(&mut ctx, low_sell).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, mkt_buy).status, OrderStatus::Filled);
    // High-price level must still be present (best_ask refreshed up to it after low filled).
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), high_price);
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
    let o = get_order(&mut ctx, id);
    assert_eq!(o.status, OrderStatus::Expired);
    assert_eq!(o.filled, 0);
}

#[test]
fn ioc_partial_fill_cancels_remainder() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // only QTY available

    // IOC buy for 2×QTY: fills QTY, remainder expired.
    let id = place(&mut ctx, ALICE, 0, PRICE, QTY * 2, 0, 1);
    let o = get_order(&mut ctx, id);
    assert_eq!(o.status, OrderStatus::Expired);
    assert_eq!(o.filled, QTY);
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

    assert_eq!(get_order(&mut ctx, buy_id).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, sell_id).status, OrderStatus::Filled);
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
    assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), ask_price);
}

// ── Cancel ────────────────────────────────────────────────────────────────

#[test]
fn cancel_resting_order_releases_margin_and_clears_book() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert!(
        wallet(&mut ctx, ALICE) < WALLET,
        "margin should be reserved"
    );

    let input = cancelOrderCall {
        orderId: id.into(),
        marketId: MARKET_ID,
    }
    .abi_encode();
    run_cancel_order(&input, ALICE, &mut ctx).unwrap();

    assert_eq!(wallet(&mut ctx, ALICE), WALLET, "margin should be returned");
    assert_eq!(get_order(&mut ctx, id).status, OrderStatus::Cancelled);
    assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), 0);
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
    assert!(storage::load_bid_level(&mut ctx, MARKET_ID, p_lo)
        .unwrap()
        .is_empty());
    assert_eq!(get_order(&mut ctx, lo).status, OrderStatus::Cancelled);
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
    assert!(storage::load_bid_level(&mut ctx, MARKET_ID, p_hi)
        .unwrap()
        .is_empty());
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
    assert!(storage::load_ask_level(&mut ctx, MARKET_ID, p_hi)
        .unwrap()
        .is_empty());
    assert_eq!(get_order(&mut ctx, hi).status, OrderStatus::Cancelled);
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
        crate::perp_dex::types::Side::Buy,
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
    assert!(err.to_string().contains("not cancellable"), "{err}");
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
fn reverted_subcall_leaves_no_perp_residue() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    // Snapshot before the (to-be-reverted) sub-call.
    let cp = ctx.journal_mut().checkpoint();
    let order_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0);
    assert!(
        storage::load_order(&mut ctx, &order_id).unwrap().is_some(),
        "order should exist after placing"
    );

    // Revert the sub-call: the placed order must vanish — the perp overlay reverts in
    // lock-step with the EVM journal.
    ctx.journal_mut().checkpoint_revert(cp);
    assert!(
        storage::load_order(&mut ctx, &order_id).unwrap().is_none(),
        "reverted sub-call must leave no perp residue"
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
    use crate::perp_dex::{
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
        st: &crate::perp_dex::storage::bench_counter::Stats,
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
        use crate::perp_dex::storage::bench_counter as bc;
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
        use crate::perp_dex::storage::bench_counter as bc;
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
        use crate::perp_dex::storage::bench_counter as bc;
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
//   * funding-charge waterfall into position margin / insurance fund (the
//     scenario's funding charge is covered by the wallet)
//   * margin-shortfall auto-cancel cascades: taker-side
//     cancel_same_side_orders_until_wallet_covers and maker-side
//     cancel_maker_orders_until_wallet_nonnegative (maker auto-expire), incl.
//     the matcher early-exit sub-variant where the surviving queue tail was
//     entirely expired-during-level (trading/mod.rs:827/:984) — all require a
//     maker-deficit cascade that would dominate the scenario
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
    use crate::perp_dex::{
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
    const GOLDEN_COMMITMENT: B256 =
        b256!("0xa4f990daed21ad051cde3d8858c66708ab8f608ff30c5961b0cce42b6ef97c01");

    /// Business end-state read back through view calls after the scenario.
    /// Pins semantics independently of the commitment hash construction.
    #[derive(Debug, PartialEq, Eq)]
    struct BusinessSnapshot {
        /// (amount, vQuoteBalance, margin)
        alice_position: (i64, i64, i64),
        bob_position: (i64, i64, i64),
        carol_position: (i64, i64, i64),
        /// (spot usdc balance, perp wallet balance)
        alice_account: (U256, u64),
        bob_account: (U256, u64),
        carol_account: (U256, u64),
        bob_erc20: U256,
        /// Trading-fee sink (taker+maker fees credit the admin's perp wallet).
        admin_perp_wallet: u64,
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
    }

    fn expected_snapshot() -> BusinessSnapshot {
        BusinessSnapshot {
            // ALICE fully closed by the liquidation round-trip; BOB flat after
            // CAROL's taker sell closes his last short QTY.
            alice_position: (0, 0, 0),
            bob_position: (0, 0, 0),
            // CAROL short QTY @ $80 at default leverage 1 (full-notional margin).
            carol_position: (-1_000_000, 800_000, 800_000),
            // ALICE perp = 1e9 − 806_000 fill margin − 2_015 taker fees
            //   − 500_000 addMargin + 250_000 removeMargin − 400 funding
            //   + 169_500 book-leg close (margin release 792_000 + PnL −622_500)
            //   + 56_500 residual mark-price settle (margin 264_000 + PnL
            //   −207_500) − 1_200 close taker fee − 5_280 clearance fee.
            alice_account: (U256::from(500_000_000u64), 999_161_105),
            // BOB perp = 1e9 + 830_000 short PnL (622_500 on the 3-QTY
            //   liquidation leg + 207_500 on the QTY closed via CAROL) + 400
            //   funding credit − 1_446 maker fees − 400_160 still reserved for
            //   the resting tail bid (400_000 MR + 160 fee)
            //   − 500_000_000 transferFromPerp.
            bob_account: (U256::from(500_000_000u64), 500_428_794),
            // CAROL perp = 5_000_000 funded − 800_000 short opening margin.
            carol_account: (U256::from(5_000_000u64), 4_200_000),
            // 2e9 seed − 1.5e9 deposit + 0.5e9 withdraw.
            bob_erc20: U256::from(1_000_000_000u64),
            // 100M funding − 50M IF deposit + 1M IF withdraw + 4_661 fees.
            admin_perp_wallet: 51_004_661,
            // 50M deposit − 1M withdraw + 5_280 clearance fee
            //   (50 bps of ALICE's 1_056_000 pre-liquidation margin).
            insurance_fund: 49_005_280,
            // ALICE takers 2_015 + close taker 1_200 + BOB maker 806 + 480
            //   + 160 (CAROL's taker fee is 0 bps: default fee rates).
            market_fee_total: 4_661,
            mark_price: 80_000_000_000, // $80 post-crash
            funding: (100, 7_215),      // rate = interest-rate clamp; next epoch ts
            signed_buy_status: OrderStatus::Cancelled as u8,
            gtc_cancelled_status: OrderStatus::Cancelled as u8,
            ioc_status: OrderStatus::Expired as u8,
            mkt_expired_status: OrderStatus::Expired as u8,
            po_ask_cancelled_status: OrderStatus::Cancelled as u8,
            liq_cancelled_bid_status: OrderStatus::Cancelled as u8,
            liq_cancelled_ask_status: OrderStatus::Cancelled as u8,
            bob_bid_status: OrderStatus::Filled as u8,
            bob_tail_bid_status: OrderStatus::Open as u8,
            carol_close_status: OrderStatus::Filled as u8,
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
        keccak256(b"cmit")
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

    /// Step 4b replay mode at the entry point: with the journal in replay mode a trading selector
    /// returns the pre-computed result VERBATIM without decoding / verifying / matching — proven by a
    /// bare 4-byte selector (no args), which the normal path rejects as undecodable but replay still
    /// answers from the queue. An exhausted cursor fail-stops (Fatal) rather than mis-replaying.
    #[cfg(feature = "perp-parallel")]
    #[test]
    fn replay_mode_returns_precomputed_result_and_failstops_on_exhaustion() {
        use context::journaled_state::PerpReplayResult;
        use primitives::{Log, LogData};
        let mut ctx = golden_ctx();
        let input = placeOrderSignedCall::SELECTOR.to_vec();

        // Non-replay: a bare selector cannot decode → clean revert (the verify/match path ran).
        let out = run_perp_dex_call(&input, 10_000_000, ALICE, U256::ZERO, false, &mut ctx).unwrap();
        assert!(out.reverted, "non-replay bare selector should revert (undecodable args)");
        let _ = ctx.journal_mut().take_logs(); // clear any pre-replay logs

        // Replay mode: hand back the pre-computed bytes verbatim (no decode/verify/match) AND re-emit
        // the pre-computed logs into this call's journal so they land in the tx receipt.
        let replayed_log = Log {
            address: PERP_DEX_ADDRESS,
            data: LogData::new_unchecked(
                std::vec![B256::repeat_byte(0x11)],
                Bytes::from(std::vec![1u8, 2, 3]),
            ),
        };
        ctx.journal_mut().set_perp_replay(std::vec![PerpReplayResult {
            reverted: false,
            output: std::vec![0xAB; 32],
            logs: std::vec![replayed_log.clone()],
        }]);
        let out = run_perp_dex_call(&input, 10_000_000, ALICE, U256::ZERO, false, &mut ctx).unwrap();
        assert!(!out.reverted);
        assert_eq!(out.bytes.as_ref(), [0xAB; 32].as_slice());
        assert_eq!(
            ctx.journal_mut().take_logs(),
            std::vec![replayed_log],
            "replay must re-emit the pre-computed logs into the receipt"
        );

        // Cursor exhausted → fail-stop (an unclassified trading call), not a silent mis-replay.
        let err =
            run_perp_dex_call(&input, 10_000_000, ALICE, U256::ZERO, false, &mut ctx).unwrap_err();
        assert!(matches!(err, crate::PrecompileError::Fatal(_)));
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

    fn order_status(ctx: &mut TestCtx, id: [u8; 32]) -> u8 {
        let ret = dex_view(
            ctx,
            &getOrderCall {
                orderId: id.into(),
                marketId: MARKET_ID,
            }
            .abi_encode(),
        );
        getOrderCall::abi_decode_returns(&ret).unwrap().status
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
                leverage: 5,
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
        // debit branch (5→4 grows the resting-order reserve; relayed by CAROL
        // through the signed path) and its credit branch (4→5 shrinks it back).
        dex_call(&mut ctx, CAROL, &signed_leverage_input(&sk, 4));
        dex_call(
            &mut ctx,
            ALICE,
            &setLeverageCall {
                marketId: MARKET_ID,
                leverage: 5,
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
            OrderStatus::Filled as u8,
            "L1 head maker must be filled"
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
            "not cancellable",
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

        // ALICE leaves a resting bid AND a resting ask so liquidate()'s
        // cancel-all clears both book sides. The ask is fully offset by her
        // 4×QTY long, so it reserves no margin — only the maker fee.
        let alice_resting_bid = g_place(&mut ctx, ALICE, 0, PRICE - 30 * TICK, QTY, 0, 0);
        let alice_resting_ask = g_place(&mut ctx, ALICE, 1, PRICE, QTY, 0, 0);

        // Crash: index $100 → $80; ALICE's 5x long drops under maintenance.
        dex_call(
            &mut ctx,
            ORACLE,
            &updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: PRICE - 20 * TICK,
                timestamp: 3_630,
            }
            .abi_encode(),
        );

        // BOB quotes only 3×QTY of closing liquidity, so the liquidation
        // closes 3×QTY through the book and settles the residual QTY at mark
        // price; CAROL (anyone) liquidates.
        let bob_bid = g_place(&mut ctx, BOB, 0, PRICE - 20 * TICK, 3 * QTY, 0, 0);
        dex_call(
            &mut ctx,
            CAROL,
            &liquidateCall {
                user: ALICE,
                marketId: MARKET_ID,
            }
            .abi_encode(),
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
            OrderStatus::Filled as u8,
            "head bid must be filled by CAROL's taker sell"
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
            alice_account: (alice_acct.usdcBalance, alice_acct.perpWalletBalance),
            bob_account: (bob_acct.usdcBalance, bob_acct.perpWalletBalance),
            carol_account: (carol_acct.usdcBalance, carol_acct.perpWalletBalance),
            bob_erc20,
            admin_perp_wallet: admin_acct.perpWalletBalance,
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
