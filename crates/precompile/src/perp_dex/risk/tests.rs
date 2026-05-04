use super::*;
use alloy_sol_types::SolCall;
use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId};

use crate::perp_dex::{
    interface::IPerpDex::{liquidateCall, placeOrderCall},
    trading::run_place_order,
    types::{PerpPosition, UserAccount},
    USDC_ADDRESS,
};

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const KEEPER: Address = address!("2222222222222222222222222222222222222222");
const MAKER: Address = address!("3333333333333333333333333333333333333333");
const MARKET_ID: u64 = 1;
const PRICE_DECIMALS: u32 = 2;
const ENTRY_PRICE: u64 = 10_000; // $100.00
const LONG_LIQ_PRICE: u64 = 9_000; // $90.00
const SHORT_LIQ_PRICE: u64 = 11_000; // $110.00
const QTY: i64 = 10;
const ENTRY_VALUE: i64 = 1_000_000_000; // $1,000.00 in quote units.
const MARGIN: i64 = 200_000_000; // $200.00 in quote units.
const USER_WALLET: u64 = 50_000_000; // $50.00 in quote units.
const MAKER_WALLET: u64 = 2_000_000_000; // $2,000.00 in quote units.

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, KEEPER, MAKER] {
        JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
    }
    ctx
}

fn setup_market(ctx: &mut TestCtx) {
    storage::save_market(
        ctx,
        &Market {
            market_id: MARKET_ID,
            base_decimals: 0,
            price_decimals: PRICE_DECIMALS,
            tick_size: 1,
            step_size: 1,
            min_quantity: 1,
            max_quantity: 1_000_000,
            max_price: 1_000_000,
            active: true,
        },
    )
    .unwrap();
    storage::save_mark_price(ctx, MARKET_ID, ENTRY_PRICE).unwrap();
    storage::save_account(
        ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: USER_WALLET,
            ..UserAccount::default()
        },
    )
    .unwrap();
    storage::save_account(
        ctx,
        MAKER,
        UserAccount {
            perp_wallet_balance: MAKER_WALLET,
            ..UserAccount::default()
        },
    )
    .unwrap();
}

fn liquidate(ctx: &mut TestCtx, user: Address) -> Result<Bytes, PrecompileError> {
    let input = liquidateCall {
        user,
        marketId: MARKET_ID,
    }
    .abi_encode();
    run_liquidate(&input, KEEPER, ctx)
}

fn place_maker_order(ctx: &mut TestCtx, side: u8, price: u64, qty: u64) {
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
    run_place_order(&input, MAKER, ctx).unwrap();
}

fn wallet(ctx: &mut TestCtx, user: Address) -> u64 {
    storage::load_account(ctx, user)
        .unwrap()
        .perp_wallet_balance
}

fn position(ctx: &mut TestCtx, user: Address) -> PerpPosition {
    storage::load_position(ctx, user, MARKET_ID).unwrap()
}

fn save_position(ctx: &mut TestCtx, amount: i64, v_quote_balance: i64) {
    storage::save_position(
        ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance,
            margin: MARGIN,
            leverage: 5,
            ..PerpPosition::default()
        },
    )
    .unwrap();
}

#[test]
fn liquidate_rejects_healthy_position() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let err = liquidate(&mut ctx, ALICE).unwrap_err();
    assert!(err.to_string().contains("above maintenance margin"));
}

#[test]
fn liquidate_long_sells_full_position_into_bids() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64);

    liquidate(&mut ctx, ALICE).unwrap();

    let alice = position(&mut ctx, ALICE);
    assert_eq!(alice.amount, 0);
    assert_eq!(alice.v_quote_balance, 0);
    assert_eq!(alice.margin, 0);
    assert_eq!(alice.leverage, 5);

    let maker = position(&mut ctx, MAKER);
    assert_eq!(maker.amount, QTY);
    assert_eq!(maker.v_quote_balance, -900_000_000);
    assert_eq!(maker.margin, 900_000_000);

    // Alice closes at $90: 200 margin - 100 unrealized loss = 100 USDC.
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET + 100_000_000);
    assert_eq!(wallet(&mut ctx, KEEPER), 0);
}

#[test]
fn liquidate_short_buys_full_position_from_asks() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, -QTY, ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, SHORT_LIQ_PRICE).unwrap();
    place_maker_order(&mut ctx, Side::Sell as u8, SHORT_LIQ_PRICE, QTY as u64);

    liquidate(&mut ctx, ALICE).unwrap();

    let alice = position(&mut ctx, ALICE);
    assert_eq!(alice.amount, 0);
    assert_eq!(alice.v_quote_balance, 0);
    assert_eq!(alice.margin, 0);

    let maker = position(&mut ctx, MAKER);
    assert_eq!(maker.amount, -QTY);
    assert_eq!(maker.v_quote_balance, 1_100_000_000);
    assert_eq!(maker.margin, 1_100_000_000);

    // Alice closes at $110: 200 margin - 100 unrealized loss = 100 USDC.
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET + 100_000_000);
    assert_eq!(wallet(&mut ctx, KEEPER), 0);
}

#[test]
fn liquidate_rejects_when_orderbook_cannot_fully_close() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, (QTY as u64) - 1);

    let err = liquidate(&mut ctx, ALICE).unwrap_err();
    assert!(err.to_string().contains("cannot fully close position"));

    let alice = position(&mut ctx, ALICE);
    assert_eq!(alice.amount, QTY);
    assert_eq!(alice.v_quote_balance, -ENTRY_VALUE);
}

#[test]
fn liquidate_refunds_reserved_margin_before_market_close() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    let mut pos = position(&mut ctx, ALICE);
    pos.margin_reserved = 33_000_000;
    pos.buy_side_margin_reserved = 33_000_000;
    storage::save_position(&mut ctx, ALICE, MARKET_ID, &pos).unwrap();
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64);

    liquidate(&mut ctx, ALICE).unwrap();

    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET + 33_000_000 + 100_000_000
    );
    assert_eq!(position(&mut ctx, ALICE).margin_reserved, 0);
}
