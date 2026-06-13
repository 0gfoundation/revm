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
    assert!(storage::load_ask_prices(&mut ctx, MARKET_ID)
        .unwrap()
        .is_empty());
}

#[test]
fn sell_taker_fully_matches_resting_bid() {
    let mut ctx = make_ctx();
    setup(&mut ctx);

    let buy_id = place(&mut ctx, ALICE, 0, PRICE, QTY, 0, 0); // resting bid
    let sell_id = place(&mut ctx, BOB, 1, PRICE, QTY, 0, 0); // taker sell

    assert_eq!(get_order(&mut ctx, buy_id).status, OrderStatus::Filled);
    assert_eq!(get_order(&mut ctx, sell_id).status, OrderStatus::Filled);
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
    assert_eq!(
        storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![PRICE]
    );
}

#[test]
fn maker_auto_expire_current_level_keeps_expired_status_and_clears_queue() {
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
    assert_eq!(maker.status, OrderStatus::Expired);
    assert_eq!(maker.filled, QTY);
    assert_eq!(get_order(&mut ctx, buy_id).status, OrderStatus::Filled);
    assert!(!storage::load_ask_prices(&mut ctx, MARKET_ID)
        .unwrap()
        .contains(&PRICE));
    assert!(storage::load_ask_level(&mut ctx, MARKET_ID, PRICE)
        .unwrap()
        .is_empty());
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
    assert_eq!(
        storage::load_ask_prices(&mut ctx, MARKET_ID).unwrap(),
        vec![ask_price]
    );
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
    assert!(storage::load_bid_prices(&mut ctx, MARKET_ID)
        .unwrap()
        .is_empty());
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
    // `place` drives `run_place_order` directly (no dispatch), so the per-call commitment fold is
    // still in the accumulator — flush it to the on-trie slot, as `run_perp_dex_call` would.
    storage::flush_commitment(&mut ctx).unwrap();

    // Perp writes are captured in the off-trie delta...
    let delta = ctx.journal_mut().take_perp_delta();
    assert!(!delta.is_empty(), "perp writes must land in the off-trie delta");

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
        report(
            "place+cancel pair avg",
            t_place + t_cancel,
            iters * 2,
        );
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
    /// Last re-pin 2026-06-14 (P4/#20 positional): blob encoding switched to
    /// positional msgpack (struct field names dropped) — CHAIN change, value
    /// re-pinned. The business SNAPSHOT below is unchanged (blobs decode to the
    /// same structs), the guard that survives across re-pins.
    /// (Prior re-pins: 2026-06-14 P4/#18 coalesce 0x58835a…; P4/#24 BLAKE3
    /// 0x995638…; P4/16b framed keccak 0x69e699…; 2026-06-12 ext 0x2d5fa5…; P0.)
    const GOLDEN_COMMITMENT: B256 =
        b256!("0xcdeac19a48fa920a118fed3a64136db053cf090f57edb2ace298dfd20758d26b");

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
            (CAROL, 10_000_000), // $10 — only used for the post-liquidation step
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
        dex_call(&mut ctx, ADMIN, &initAdminCall { admin: ADMIN }.abi_encode());
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
        dex_view(&mut ctx, &getMarkPriceCall { marketId: MARKET_ID }.abi_encode());
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
        dex_view(&mut ctx, &getIndexPriceCall { marketId: MARKET_ID }.abi_encode());
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

        // View stretch #2 — final reads must not fold either.
        let c_final_views = read_commitment(&mut ctx);
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
        let commitment = read_commitment(&mut ctx);
        assert_eq!(
            commitment, c_final_views,
            "snapshot views must not fold the commitment"
        );

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
        let insurance_fund =
            getInsuranceFundCall::abi_decode_returns(&dex_view(
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
        assert_eq!(first.1, second.1, "business end-state must be deterministic");
    }
}
