use context::ContextTr;
use super::*;
use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, U256};

use crate::{
    funding::settle_position_funding,
    interface::IPerpDex::{
        addPositionMarginCall, getMarginTiersCall, liquidateCall, placeOrderCall,
        removePositionMarginCall, setLeverageCall, setMarginTiersCall, updateIndexPriceCall,
    },
    run_perp_dex_call,
    trading::{run_place_order, MAX_LIQUIDATION_MAKER_ACCOUNTS},
    types::{
        FundingState, IndexPriceHistory, MarginTiers, PerpPosition, PremiumIndexAccumulator,
        PriceBasisWindow, UserAccount, UserFeeRates,
    },
    USDC_ADDRESS,
};

const ALICE: Address = address!("1111111111111111111111111111111111111111");
const KEEPER: Address = address!("2222222222222222222222222222222222222222");
const MAKER: Address = address!("3333333333333333333333333333333333333333");
const ADMIN: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
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
const LONG_LIQ_TAKER_FEE: u64 = 0;
const SHORT_LIQ_TAKER_FEE: u64 = 0;

type TestCtx = Context<BlockEnv, TxEnv, CfgEnv, InMemoryDB, Journal<InMemoryDB>, ()>;

fn make_ctx() -> TestCtx {
    let db = InMemoryDB::default();
    let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
    for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS, ALICE, KEEPER, MAKER, ADMIN] {
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

fn setup_market(ctx: &mut TestCtx) {
    storage::save_admin(ctx, ADMIN).unwrap();
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
    storage::save_mark_price(ctx, MARKET_ID, ENTRY_PRICE).unwrap();
    storage::save_account(
        ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: USER_WALLET as i64,
            ..UserAccount::default()
        },
    )
    .unwrap();
    storage::save_account(
        ctx,
        MAKER,
        UserAccount {
            perp_wallet_balance: MAKER_WALLET as i64,
            ..UserAccount::default()
        },
    )
    .unwrap();
}

#[test]
fn update_index_price_aligns_timestamp_to_market_interval() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let state = storage::load_index_price_state(&mut ctx, MARKET_ID).unwrap();
    let window = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(state.index_price, ENTRY_PRICE);
    assert_eq!(state.timestamp, 30);
    assert_eq!(window.count, 0);
    assert_eq!(window.last_sample_ts, 0);
}

#[test]
fn index_update_sweep_liquidates_underwater_and_skips_healthy() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx); // mark = ENTRY_PRICE ($100), funding off, clearance fee 0

    // ALICE: 5x long — healthy at $100, underwater once the mark crashes.
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    // MAKER: the same long but heavily collateralised — stays above maintenance
    // at the crashed mark, so the sweep must skip it.
    storage::save_position(
        &mut ctx,
        MAKER,
        MARKET_ID,
        &PerpPosition {
            amount: QTY,
            v_quote_balance: -ENTRY_VALUE,
            margin: 500_000_000,
            leverage: 2,
            ..PerpPosition::default()
        },
    )
    .unwrap();
    // Both are registered as open positions (insertion order).
    assert_eq!(
        storage::load_position_registry(&mut ctx, MARKET_ID).unwrap(),
        vec![ALICE, MAKER]
    );

    // Crash the mark to $85 via updateIndexPrice (admin) — runs the sweep. At $85 the
    // 5x long is BELOW maintenance but still SOLVENT (equity >= 0), so its empty-book
    // residual closes at mark (no ADL, no IF); an insolvent residual would instead go
    // to ADL (see `adl_*` tests).
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: 8_500,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let position_changes = take_position_changes(&mut ctx);
    assert_eq!(position_changes.len(), 1);
    assert_eq!(position_changes[0].user, ALICE);
    assert_eq!(position_changes[0].realizedPnl, -150_000_000);
    assert_eq!(position_changes[0].closedQuantity, QTY as u64);

    // ALICE was under maintenance -> swept (solvent residual closed at mark, empty book);
    // MAKER stayed healthy -> untouched. Registry now holds only MAKER.
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "underwater position must be liquidated by the sweep"
    );
    assert_eq!(
        position(&mut ctx, MAKER).amount,
        QTY,
        "healthy position must be left untouched"
    );
    assert_eq!(
        storage::load_position_registry(&mut ctx, MARKET_ID).unwrap(),
        vec![MAKER]
    );
}

#[test]
fn healthy_index_update_does_not_reserve_for_large_order_book() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    for _ in 0..200 {
        place_maker_order(&mut ctx, Side::Buy as u8, 100, 1);
    }
    assert_eq!(
        storage::load_bid_count(&mut ctx, MARKET_ID, 100).unwrap(),
        200
    );
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let output = run_perp_dex_call(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE,
            timestamp: 31,
        }
        .abi_encode(),
        50_000,
        ADMIN,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();

    assert!(!output.reverted);
    assert_eq!(output.gas_used, 50_000);
    assert_eq!(
        storage::load_index_price_state(&mut ctx, MARKET_ID)
            .unwrap()
            .timestamp,
        30
    );
    assert_eq!(position(&mut ctx, ALICE).amount, QTY);
}

#[test]
fn liquidation_matching_caps_distinct_maker_accounts() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let maker_count = MAX_LIQUIDATION_MAKER_ACCOUNTS + 1;
    let quantity = u64::try_from(maker_count).unwrap();
    let entry_value = calc_value(ENTRY_PRICE, quantity, 0, PRICE_DECIMALS).unwrap();
    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: i64::try_from(quantity).unwrap(),
            v_quote_balance: -i64::try_from(entry_value).unwrap(),
            margin: i64::try_from(entry_value / 5).unwrap(),
            leverage: 5,
            ..PerpPosition::default()
        },
    )
    .unwrap();

    for index in 0..maker_count {
        let maker = indexed_maker(index);
        storage::save_account(
            &mut ctx,
            maker,
            UserAccount {
                perp_wallet_balance: 200_000_000,
                ..UserAccount::default()
            },
        )
        .unwrap();
        place_order(&mut ctx, maker, Side::Buy as u8, LONG_LIQ_PRICE, 1);
    }

    let market = storage::load_market_ref(&mut ctx, MARKET_ID)
        .unwrap()
        .unwrap();
    let remaining =
        execute_liquidation_market_order(&mut ctx, ALICE, &market, Side::Sell, quantity).unwrap();

    assert_eq!(remaining, 1);
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        1,
        "the residual remains for the liquidation residual/ADL path"
    );
    assert_eq!(position(&mut ctx, indexed_maker(0)).amount, 1);
    assert_eq!(
        position(&mut ctx, indexed_maker(MAX_LIQUIDATION_MAKER_ACCOUNTS)).amount,
        0,
        "the first maker beyond the cap must remain untouched"
    );
    assert_eq!(
        storage::load_bid_count(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap(),
        1
    );
}

#[test]
fn sweep_liquidates_underwater_user_with_no_free_wallet_for_taker_fee() {
    // Regression for review fix B: a liquidation closing through the book used to
    // charge the liquidated user a taker fee and require free wallet to cover it,
    // so an underwater user with ~0 free wallet was un-liquidatable (the close
    // reverted -> the sweep silently skipped it -> the position stayed open every
    // update). The taker fee is now waived on liquidation closes.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // ALICE: 5x long with ZERO free wallet (all collateral in margin) and a nonzero
    // taker fee — the exact pre-fix un-liquidatable state.
    storage::save_account(
        &mut ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: 0,
            ..UserAccount::default()
        },
    )
    .unwrap();
    storage::save_user_fee_rates(
        &mut ctx,
        ALICE,
        UserFeeRates {
            maker_fee_bps: 0,
            taker_fee_bps: 10,
        },
    )
    .unwrap();
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    // MAKER rests a full-size bid at $90 (in band pre-crash) so the liquidation
    // closes through the book — a nonzero fill notional that would incur a taker
    // fee pre-fix.
    place_order(&mut ctx, MAKER, 0, 9_000, QTY as u64);

    // Crash to $85: ALICE below maintenance. The sweep must liquidate her despite
    // her zero free wallet (fee waived -> total_required 0 -> no wallet gate).
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: 8_500,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "underwater ALICE with 0 free wallet must still be liquidated (taker fee waived)"
    );
}

// ── ADL (auto-deleveraging, scheme X) ───────────────────────────────────────────

fn seed_position_account(
    ctx: &mut TestCtx,
    user: Address,
    amount: i64,
    v_quote: i64,
    margin: i64,
    leverage: u64,
    wallet: i64,
) {
    storage::save_account(
        ctx,
        user,
        UserAccount {
            perp_wallet_balance: wallet,
            ..UserAccount::default()
        },
    )
    .unwrap();
    storage::save_position(
        ctx,
        user,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance: v_quote,
            margin,
            leverage,
            ..PerpPosition::default()
        },
    )
    .unwrap();
}

// Σ(perp_wallet + margin + vQuote) over `users`, plus the global insurance fund.
fn conservation_sum(ctx: &mut TestCtx, users: &[Address]) -> i128 {
    let mut s = storage::load_insurance_fund(ctx).unwrap() as i128;
    for &u in users {
        let a = storage::load_account(ctx, u).unwrap();
        let p = storage::load_position(ctx, u, MARKET_ID).unwrap();
        s += a.perp_wallet_balance as i128 + p.margin as i128 + p.v_quote_balance as i128;
    }
    s
}

#[test]
fn adl_closes_insolvent_residual_against_opposite_holder_conserving_no_if() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // ALICE: 5x long 10 @ $100 (margin $200, vq -$1000), $50 free wallet.
    seed_position_account(
        &mut ctx,
        ALICE,
        QTY,
        -ENTRY_VALUE,
        MARGIN,
        5,
        USER_WALLET as i64,
    );
    // KEEPER: the opposite side — 5x short 10 @ $100 (margin $200, vq +$1000), off-book,
    // profitable once the mark drops. This is the ADL counterparty.
    seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 5, 0);
    assert_eq!(
        storage::load_position_registry(&mut ctx, MARKET_ID).unwrap(),
        vec![ALICE, KEEPER]
    );

    let users = [ALICE, KEEPER];
    let value_before = conservation_sum(&mut ctx, &users);
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    let amount_before: i64 = users.iter().map(|&u| position(&mut ctx, u).amount).sum();
    assert_eq!(amount_before, 0, "zero-sum market");

    // Crash to $75: ALICE insolvent (equity -$50, bankruptcy price $80). Empty book →
    // full residual → ADL against KEEPER at $80.
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: 7_500,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let position_changes = take_position_changes(&mut ctx);
    assert_eq!(position_changes.len(), 2);
    assert_eq!(position_changes[0].user, ALICE);
    assert_eq!(position_changes[0].realizedPnl, -200_000_000);
    assert_eq!(position_changes[0].closedQuantity, QTY as u64);
    assert_eq!(position_changes[1].user, KEEPER);
    assert_eq!(position_changes[1].realizedPnl, 200_000_000);
    assert_eq!(position_changes[1].closedQuantity, QTY as u64);

    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "insolvent long fully ADL'd"
    );
    assert_eq!(
        position(&mut ctx, KEEPER).amount,
        0,
        "opposite short absorbed the residual"
    );
    assert!(storage::load_position_registry(&mut ctx, MARKET_ID)
        .unwrap()
        .is_empty());
    // Scheme X: ADL routes NO bad debt to the Insurance Fund.
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        if_before,
        "ADL must not touch the insurance fund"
    );
    // Full value conservation + Σamount conserved (ADL is a real trade, not synthetic).
    assert_eq!(
        conservation_sum(&mut ctx, &users),
        value_before,
        "ADL must conserve value"
    );
    let amount_after: i64 = users.iter().map(|&u| position(&mut ctx, u).amount).sum();
    assert_eq!(amount_after, 0, "Σamount conserved");
    // Isolated margin: the liquidated long's wallet is untouched by the loss.
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET,
        "loser wallet untouched by the loss"
    );
}

/// The "holder has open orders" exclusion is asked of the ORDER LISTS, not of
/// `margin_reserved`. A PURE-REDUCE resting order (fully absorbed by the holder's own position)
/// reserves ZERO margin, so the reservation proxy would wave such a holder through and ADL would
/// fill against him without the flip-aware reservation recompute / auto-cancel that v1 exists to
/// avoid. Before the `fee_reserved` escrow was removed, the ONLY thing catching this case was the
/// `fee_reserved != 0` half of the old predicate — deleting it without a replacement would have
/// silently regressed here.
#[test]
fn adl_skips_opposite_holder_whose_only_order_reserves_no_margin() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(
        &mut ctx,
        ALICE,
        QTY,
        -ENTRY_VALUE,
        MARGIN,
        5,
        USER_WALLET as i64,
    );
    // KEEPER: the sole opposite-side holder — 5x short 10 @ $100, deeply profitable once the
    // mark drops, i.e. exactly the candidate ADL wants. A maker fee rate makes this the case the
    // OLD `fee_reserved != 0` clause used to catch.
    seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 5, 0);
    storage::save_user_fee_rates(
        &mut ctx,
        KEEPER,
        UserFeeRates {
            maker_fee_bps: 200,
            taker_fee_bps: 0,
        },
    )
    .unwrap();

    // A BUY of his whole short is PURE REDUCE: it opens no new exposure, so it reserves nothing.
    // $80 is deep enough to sit below the fill-time band (the sweep bands against the pre-update
    // mark of $100, so ±10% = [$90, $110]), which is what keeps the liquidation's market sell
    // from simply eating this bid — the whole residual has to reach ADL for the predicate to be
    // exercised. It does not move the mark: price1 and the contract price both stay at the index,
    // so their median is the index regardless of the basis this bid contributes.
    place_order(&mut ctx, KEEPER, 0, 8_000, QTY as u64);
    let keeper_pos = position(&mut ctx, KEEPER);
    assert_eq!(
        keeper_pos.margin_reserved, 0,
        "pure-reduce order must reserve no margin — this is what makes the \
         `margin_reserved != 0` proxy insufficient"
    );
    assert_eq!(wallet(&mut ctx, KEEPER), 0, "and cost nothing to place");

    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: 7_500,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    // The only opposite holder is excluded → the insolvent residual DEFERS, exactly as if no
    // counterparty existed. Nothing moved on either side.
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        QTY,
        "residual must defer — the order-holding counterparty is not eligible"
    );
    assert_eq!(
        position(&mut ctx, KEEPER).amount,
        -QTY,
        "order-holding holder must not be ADL'd"
    );
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), if_before);
    assert_eq!(conservation_sum(&mut ctx, &[ALICE, KEEPER]), value_before);
}

#[test]
fn adl_defers_insolvent_residual_when_no_eligible_opposite_holder() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // Only ALICE (insolvent long) exists — no opposite-side holder to ADL against.
    seed_position_account(
        &mut ctx,
        ALICE,
        QTY,
        -ENTRY_VALUE,
        MARGIN,
        5,
        USER_WALLET as i64,
    );

    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    let value_before = conservation_sum(&mut ctx, &[ALICE]);

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: 7_500,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    // No opposite holder → the insolvent residual is DEFERRED (stays open, still
    // registered), NOT closed at mark to the IF (scheme X). Nothing moved.
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        QTY,
        "insolvent residual deferred, not force-closed"
    );
    assert_eq!(
        storage::load_position_registry(&mut ctx, MARKET_ID).unwrap(),
        vec![ALICE],
        "deferred position stays registered for the next sweep"
    );
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        if_before,
        "IF untouched on defer"
    );
    assert_eq!(
        conservation_sum(&mut ctx, &[ALICE]),
        value_before,
        "nothing moved on defer"
    );
}

#[test]
fn update_index_price_discards_same_or_older_aligned_timestamp() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE + 100,
            timestamp: 44,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let state = storage::load_index_price_state(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(state.index_price, ENTRY_PRICE);
    assert_eq!(state.timestamp, 30);

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE + 200,
            timestamp: 45,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let state = storage::load_index_price_state(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(state.index_price, ENTRY_PRICE + 200);
    assert_eq!(state.timestamp, 45);
}

#[test]
fn funding_epoch_jump_computes_once_and_advances_next_ts_to_future() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let mut market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    market.funding_interval = 15;
    storage::save_market(&mut ctx, &market).unwrap();

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE,
            timestamp: 16,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    let funding = storage::load_funding_state(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(funding.next_funding_ts, 30);

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE,
            timestamp: 61,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let funding = storage::load_funding_state(&mut ctx, MARKET_ID).unwrap();
    let acc = storage::load_premium_accumulator(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(funding.next_funding_ts, 75);
    assert_eq!(acc.sample_count, 1);
    assert_eq!(acc.epoch_start_ts, 60);
    assert_eq!(acc.last_sample_ts, 60);
}

#[test]
fn funding_index_accumulates_mark_times_rate_at_epoch_boundary() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let mut market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    market.funding_interval = 15;
    market.interest_rate = 100; // non-zero so the computed rate != 0 even when mark == index
    storage::save_market(&mut ctx, &market).unwrap();

    // First update starts the epoch; no rate computed yet, index untouched.
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE,
            timestamp: 16,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(
        storage::load_funding_state(&mut ctx, MARKET_ID)
            .unwrap()
            .cumulative_funding_index,
        0
    );

    // Second update crosses the funding boundary → one rate computed → the
    // index folds `mark_price * rate` exactly once.
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE,
            timestamp: 61,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let funding = storage::load_funding_state(&mut ctx, MARKET_ID).unwrap();
    let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
    assert_ne!(
        funding.last_funding_rate, 0,
        "precondition: rate must be non-zero to exercise accumulation"
    );
    assert_eq!(
        funding.cumulative_funding_index,
        mark as i128 * funding.last_funding_rate as i128
    );
}

fn set_funding_index(ctx: &mut TestCtx, index: i128) {
    storage::save_funding_state(
        ctx,
        MARKET_ID,
        &FundingState {
            last_funding_rate: 100,
            next_funding_ts: 0,
            cumulative_funding_index: index,
        },
    )
    .unwrap();
}

fn settle_alice_funding(ctx: &mut TestCtx) -> (PerpPosition, UserAccount) {
    let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    let mut pos = storage::load_position(ctx, ALICE, MARKET_ID).unwrap();
    settle_position_funding(ctx, ALICE, &market, &mut pos).unwrap();
    // Funding must not touch the account at all, so the account is read back from STORAGE after
    // settlement — asserting on a copy loaded before the call would be tautological.
    let account = storage::load_account(ctx, ALICE).unwrap();
    (pos, account)
}

#[test]
fn settle_funding_long_pays_from_position_margin() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx); // ALICE wallet = USER_WALLET = 50_000_000
    set_funding_index(&mut ctx, 75_000_000); // charge for QTY long = 7_500_000
    save_position(&mut ctx, QTY, -ENTRY_VALUE); // anchor defaults to 0

    let (pos, account) = settle_alice_funding(&mut ctx);

    // A1: funding settles against the POSITION. The charge lands on `pos.margin` — the term the
    // maintenance check reads — and the account-global wallet, which also backs every OTHER
    // market's orders, does not move.
    assert_eq!(pos.margin, MARGIN - 7_500_000, "charge taken from margin");
    assert_eq!(
        account.perp_wallet_balance, USER_WALLET as i64,
        "wallet untouched (isolated margin)"
    );
    assert_eq!(pos.last_funding_index, 75_000_000, "re-anchored to index");
}

#[test]
fn settle_funding_short_receives_into_position_margin() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_funding_index(&mut ctx, 75_000_000);
    save_position(&mut ctx, -QTY, ENTRY_VALUE); // short receives when rate > 0

    let (pos, account) = settle_alice_funding(&mut ctx);

    assert_eq!(
        pos.margin,
        MARGIN + 7_500_000,
        "credit lands on the position"
    );
    assert_eq!(account.perp_wallet_balance, USER_WALLET as i64);
}

#[test]
fn settle_funding_charge_never_touches_the_wallet() {
    // A1 (was `settle_funding_charge_waterfalls_wallet_then_margin`): the wallet→margin waterfall
    // is GONE. Same fixture — a 60M charge against a 50M wallet and a 200M margin — but the wallet
    // is no longer drained first; the whole charge bites the position.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_funding_index(&mut ctx, 600_000_000); // charge = 60_000_000 > wallet 50M
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();

    let (pos, account) = settle_alice_funding(&mut ctx);

    assert_eq!(
        account.perp_wallet_balance, USER_WALLET as i64,
        "wallet is not a funding source any more"
    );
    assert_eq!(pos.margin, MARGIN - 60_000_000, "full charge from margin");
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        if_before,
        "margin covered it — no insurance-fund spill"
    );
}

#[test]
fn settle_funding_charge_beyond_margin_absorbs_from_insurance_fund() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    // A1 re-scale: the pre-A1 index of 3_000_000_000 (charge 300M) left a 50M IF shortfall only
    // because the wallet absorbed the first 50M. With the wallet leg gone, margin alone absorbs
    // 200M, so the index is scaled to keep the SAME 50M shortfall this test was written to pin.
    set_funding_index(&mut ctx, 2_500_000_000); // charge = 250M > margin 200M
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let (pos, account) = settle_alice_funding(&mut ctx);

    assert_eq!(
        account.perp_wallet_balance, USER_WALLET as i64,
        "wallet untouched even when the position cannot cover the charge"
    );
    assert_eq!(pos.margin, 0);
    // 50M shortfall absorbed from the 80M insurance fund → 30M left.
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 30_000_000);
}

#[test]
fn healthy_candidate_scan_is_write_free_even_with_accrued_funding() {
    // commit-only #23: liquidate_position on a HEALTHY position computes funding in memory and
    // returns AboveMaintenance with ZERO writes — even when the accrued funding would dip into
    // the insurance fund. Previously it persisted the funding settle (IF draw + pos/account) and
    // relied on checkpoint_revert/tx-revert to discard it; unit tests never revert, so any
    // pre-reject write is visible here.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    set_funding_index(&mut ctx, 75_000_000); // small charge (7.5M < wallet 50M): stays healthy
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    let pos_before = position(&mut ctx, ALICE);
    let acct_before = storage::load_account(&mut ctx, ALICE).unwrap();

    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    let mut adl_budget = 128u32;
    let out = liquidate_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &market,
        ENTRY_PRICE,
        KEEPER,
        &mut adl_budget,
    )
    .unwrap();
    assert!(
        matches!(out, LiquidationOutcome::AboveMaintenance),
        "{out:?}"
    );

    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        if_before,
        "IF drawn"
    );
    assert_eq!(position(&mut ctx, ALICE), pos_before, "position written");
    assert_eq!(
        storage::load_account(&mut ctx, ALICE).unwrap(),
        acct_before,
        "account written"
    );
}

#[test]
fn add_margin_rejected_after_funding_leaves_insurance_fund_untouched() {
    // commit-only #23 (validate-then-apply): a margin op that settles funding (drawing from the
    // insurance fund) and THEN rejects must leave the IF untouched. Under the old ordering the
    // funding was applied (IF drawn) before the has_available reject and only undone by
    // checkpoint_revert; this unit test never invokes revert, so a pre-reject IF draw is visible.
    let mut ctx = make_ctx();
    setup_market(&mut ctx); // ALICE wallet = USER_WALLET = 50M
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    set_funding_index(&mut ctx, 3_000_000_000); // charge 300M > margin 200M → dips IF
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();

    // A1: funding no longer drains the wallet, so the reject is driven by asking for more than
    // the (untouched) 50M wallet holds. The pending IF draw is unchanged — that is the point.
    let err = add_position_margin(&mut ctx, 100_000_000).unwrap_err();
    assert!(
        err.to_string().contains("insufficient perp wallet"),
        "{err}"
    );
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        if_before,
        "rejected add-margin must not draw from the insurance fund"
    );
}

#[test]
fn remove_margin_rejected_after_funding_leaves_insurance_fund_untouched() {
    // Symmetric to the add-margin case: funding drains margin to 0, then the removal rejects
    // (insufficient position margin) — the IF must be untouched.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    set_funding_index(&mut ctx, 3_000_000_000); // drains wallet + margin, dips IF
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();

    let err = remove_position_margin(&mut ctx, 1_000_000).unwrap_err();
    assert!(
        err.to_string().contains("insufficient position margin"),
        "{err}"
    );
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        if_before,
        "rejected remove-margin must not draw from the insurance fund"
    );
}

#[test]
fn settle_funding_emits_funding_settled_event() {
    use alloy_sol_types::SolEvent;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_funding_index(&mut ctx, 75_000_000);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let _ = settle_alice_funding(&mut ctx);

    let logs = JournalTr::take_logs(ctx.journal_mut());
    let topic = crate::interface::IPerpDex::FundingSettled::SIGNATURE_HASH;
    assert!(
        logs.iter().any(|l| l.data.topics().first() == Some(&topic)),
        "a FundingSettled event must be emitted on a non-zero funding settlement"
    );
}

#[test]
fn settle_funding_noop_for_flat_position_but_reanchors() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_funding_index(&mut ctx, 75_000_000);
    save_position(&mut ctx, 0, 0); // flat → no funding owed

    let (pos, account) = settle_alice_funding(&mut ctx);

    assert_eq!(
        account.perp_wallet_balance, USER_WALLET as i64,
        "no charge on a flat position"
    );
    assert_eq!(pos.last_funding_index, 75_000_000, "still re-anchored");
}

// ── A1: funding settles against the POSITION ─────────────────────────────────
//
// Funding lands on `pos.margin`, the exact term `is_above_maintenance_margin` reads, so it MOVES
// the liquidation price (Binance's measured behaviour) instead of being absorbed by an
// account-global wallet that also backs every other market's orders.

/// Quote units of notional per one price tick, for a `QTY`-sized position on this fixture's market
/// (`base_decimals = 0`, `price_decimals = PRICE_DECIMALS`). Notional is
/// `price * qty * 10^(QUOTE_DECIMALS - PRICE_DECIMALS)`.
fn quote_per_price_tick(qty: i64) -> i128 {
    qty as i128 * 10i128.pow(crate::math::QUOTE_DECIMALS - PRICE_DECIMALS)
}

/// Maintenance headroom at `mark`: `equity − maintenance_margin`, i.e. the distance to the
/// liquidation boundary in quote units. Negative ⇒ liquidatable.
fn maintenance_headroom(tiers: &MarginTiers, mark: u64, pos: &PerpPosition) -> i64 {
    let notional = calc_value_i64(mark, pos.amount, 0, PRICE_DECIMALS).unwrap();
    let equity = notional + pos.v_quote_balance + pos.margin;
    equity - crate::math::maintenance_margin(tiers, notional.abs()).unwrap()
}

/// Solves OUR OWN liquidation condition for a LONG by bisecting the production predicate
/// (`is_above_maintenance_margin` — the exact call the sweep makes): the lowest mark at which the
/// position is still above maintenance. One tick below this is liquidation.
fn long_liquidation_price(tiers: &MarginTiers, pos: &PerpPosition) -> u64 {
    let above = |p: u64| {
        is_above_maintenance_margin(
            tiers,
            p,
            pos.amount,
            pos.v_quote_balance,
            pos.margin,
            0,
            PRICE_DECIMALS,
        )
        .unwrap()
    };
    let (mut lo, mut hi) = (1u64, 1_000_000u64); // hi = the fixture market's max_price
    assert!(
        above(hi),
        "precondition: solvent at the top of the price range"
    );
    assert!(!above(lo), "precondition: liquidatable at the bottom");
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if above(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

#[test]
fn funding_charge_moves_the_liquidation_price_by_the_closed_form() {
    // THE test for A1. Binance (mainnet-measured) moves the liquidation price by
    //     dLP = funding / (qty * (MMR - 1))
    // when funding hits a position — qty cancels out of the derivation, and for a CHARGE of `f`
    // on a long it reduces to a rise of `f / (qty * (1 - MMR))`. Pre-A1 the wallet absorbed the
    // charge, so a well-funded wallet pinned the liquidation price in place indefinitely and
    // subsidised a losing funding stream. Now the charge lands on `pos.margin` and the boundary
    // moves.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    save_position(&mut ctx, QTY, -ENTRY_VALUE); // long QTY @ $100, margin $200
    set_funding_index(&mut ctx, 75_000_000); // charge = 7_500_000 on a QTY long
    const CHARGE: i64 = 7_500_000;

    let before = position(&mut ctx, ALICE);
    let lp_before = long_liquidation_price(&market.tiers, &before);
    let headroom_before = maintenance_headroom(&market.tiers, ENTRY_PRICE, &before);

    let (after, _) = settle_alice_funding(&mut ctx);
    let lp_after = long_liquidation_price(&market.tiers, &after);
    let headroom_after = maintenance_headroom(&market.tiers, ENTRY_PRICE, &after);

    assert_eq!(
        before.margin - after.margin,
        CHARGE,
        "the charge must come out of the position"
    );
    // Headroom at the unchanged mark falls by exactly the charge (equity moves, MM does not).
    assert_eq!(headroom_before - headroom_after, CHARGE);

    // Closed form, in price ticks. The default single tier gives MMR = 1/(2*3) = 1/6.
    let expected_shift = (CHARGE as i128 * 6) / (quote_per_price_tick(QTY) * 5);
    assert_eq!(expected_shift, 90, "0.90 in $, i.e. 90 ticks at 2 decimals");
    assert_eq!(
        i128::from(lp_after) - i128::from(lp_before),
        expected_shift,
        "funding must move OUR liquidation price exactly as the closed form predicts"
    );
    // Absolute pin: $96.00 → $96.90 for a 10-lot long at $100 with $200 of margin.
    assert_eq!((lp_before, lp_after), (9_600, 9_690));
}

#[test]
fn funding_credit_lands_in_the_position_and_makes_it_safer() {
    // The credit side was unconditionally divergent pre-A1 (`*wallet += payment`), so a receiving
    // position never got safer. Now the credit is isolated margin: headroom grows by exactly the
    // payment and the wallet does not move.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    save_position(&mut ctx, -QTY, ENTRY_VALUE); // short receives when the rate is positive
    set_funding_index(&mut ctx, 75_000_000);
    const CREDIT: i64 = 7_500_000;

    let before = position(&mut ctx, ALICE);
    let headroom_before = maintenance_headroom(&market.tiers, ENTRY_PRICE, &before);

    let (after, account) = settle_alice_funding(&mut ctx);

    assert_eq!(
        after.margin - before.margin,
        CREDIT,
        "credit into the position"
    );
    assert_eq!(
        account.perp_wallet_balance, USER_WALLET as i64,
        "the wallet must not move on a credit either"
    );
    assert_eq!(
        maintenance_headroom(&market.tiers, ENTRY_PRICE, &after) - headroom_before,
        CREDIT,
        "a receiving position gets SAFER by exactly the payment"
    );
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET,
        "and nothing was written to the stored account"
    );
}

#[test]
fn funding_on_one_market_leaves_another_markets_headroom_intact() {
    // Isolated-margin CONTAINMENT. Pre-A1 `wallet` was the single account-global free pool, so a
    // funding charge on market A drained the collateral backing market B's orders. Here the
    // market-1 charge (60M) is LARGER than the whole wallet (50M) — pre-A1 that zeroed the wallet
    // and market 2 became unfundable — yet market 2 must still be able to reserve the full 50M.
    const OTHER_MARKET: u64 = 2;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let mut other = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    other.market_id = OTHER_MARKET;
    storage::save_market(&mut ctx, &other).unwrap();
    storage::save_mark_price(&mut ctx, OTHER_MARKET, ENTRY_PRICE).unwrap();

    save_position(&mut ctx, QTY, -ENTRY_VALUE); // market 1: long QTY, margin 200M
    set_funding_index(&mut ctx, 600_000_000); // market-1 charge = 60_000_000 > wallet 50M

    let (pos1, _) = settle_alice_funding(&mut ctx);
    assert_eq!(
        pos1.margin,
        MARGIN - 60_000_000,
        "charge contained in market 1"
    );
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET,
        "market-1 funding must not touch the account-global wallet"
    );

    // Market 2 order-placement headroom is therefore untouched: a bid whose margin reservation
    // consumes the ENTIRE wallet still places (price 5_000 * QTY/10 lot → 50M notional at 1x).
    let input = placeOrderCall {
        marketId: OTHER_MARKET,
        side: Side::Buy as u8,
        price: 5_000,
        quantity: 1,
        orderType: 0,
        tif: 0,
        clientOrderId: FixedBytes::default(),
    }
    .abi_encode();
    run_place_order(&input, ALICE, &mut ctx)
        .expect("market-2 headroom must survive market-1 funding");

    let pos2 = storage::load_position(&mut ctx, ALICE, OTHER_MARKET).unwrap();
    assert_eq!(
        pos2.margin_reserved, USER_WALLET,
        "the whole wallet is reservable on market 2"
    );
    assert_eq!(
        wallet(&mut ctx, ALICE),
        0,
        "and only market 2's OWN reservation debits it — the full 50M was available to spend"
    );
}

#[test]
fn funding_charge_past_margin_absorbs_from_insurance_fund_and_logs() {
    use alloy_sol_types::SolEvent;
    // The IF remainder path is REACHABLE now that the wallet no longer cushions charges — this is
    // the correct isolated semantics (the position, not the account, backs its own funding).
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    set_funding_index(&mut ctx, 2_500_000_000); // charge 250M vs margin 200M → 50M remainder
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let (pos, _) = settle_alice_funding(&mut ctx);
    assert_eq!(pos.margin, 0);
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 30_000_000);

    let logs = JournalTr::take_logs(ctx.journal_mut());
    let changed: Vec<_> = logs
        .iter()
        .filter(|l| {
            l.data.topics().first()
                == Some(&crate::interface::IPerpDex::InsuranceFundChanged::SIGNATURE_HASH)
        })
        .map(|l| {
            crate::interface::IPerpDex::InsuranceFundChanged::decode_raw_log(
                l.data.topics(),
                &l.data.data,
            )
            .unwrap()
        })
        .collect();
    assert_eq!(
        changed.len(),
        1,
        "one absorption → one InsuranceFundChanged"
    );
    assert_eq!(changed[0].delta, -50_000_000);
    assert_eq!(changed[0].newBalance, 30_000_000);
    assert!(
        !logs.iter().any(|l| l.data.topics().first()
            == Some(&crate::interface::IPerpDex::InsuranceFundDepleted::SIGNATURE_HASH)),
        "the fund covered it — no depletion event"
    );
}

#[test]
fn funding_charge_past_the_insurance_fund_emits_depletion() {
    use alloy_sol_types::SolEvent;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    set_funding_index(&mut ctx, 5_000_000_000); // charge 500M vs margin 200M → 300M remainder
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let (pos, _) = settle_alice_funding(&mut ctx);
    assert_eq!(pos.margin, 0);
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET,
        "wallet still untouched"
    );
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), 0);

    let logs = JournalTr::take_logs(ctx.journal_mut());
    let changed = logs
        .iter()
        .find(|l| {
            l.data.topics().first()
                == Some(&crate::interface::IPerpDex::InsuranceFundChanged::SIGNATURE_HASH)
        })
        .map(|l| {
            crate::interface::IPerpDex::InsuranceFundChanged::decode_raw_log(
                l.data.topics(),
                &l.data.data,
            )
            .unwrap()
        })
        .expect("InsuranceFundChanged for the 80M it could absorb");
    assert_eq!((changed.delta, changed.newBalance), (-80_000_000, 0));

    let depleted = logs
        .iter()
        .find(|l| {
            l.data.topics().first()
                == Some(&crate::interface::IPerpDex::InsuranceFundDepleted::SIGNATURE_HASH)
        })
        .map(|l| {
            crate::interface::IPerpDex::InsuranceFundDepleted::decode_raw_log(
                l.data.topics(),
                &l.data.data,
            )
            .unwrap()
        })
        .expect("InsuranceFundDepleted for the 220M written off");
    assert_eq!(depleted.marketId, MARKET_ID);
    assert_eq!(depleted.badDebt, 220_000_000);
}

#[test]
fn funding_alone_pushes_a_position_into_the_sweep() {
    // SWEEP ORDERING: `run_update_index_price` persists the new funding index and THEN runs
    // `run_liquidation_sweep`, and `liquidate_position` settles funding in memory BEFORE its
    // `is_above_maintenance_margin` call — so the sweep sees the POST-funding margin. That is the
    // point of A1: funding alone can now liquidate. Pre-A1 the charge went to the wallet, the
    // margin was untouched, and this position survived.
    //
    // At mark $100 the QTY long has equity 200M against a 166.67M maintenance requirement. A 40M
    // funding charge leaves 160M — under the threshold — with the mark completely unchanged.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    set_funding_index(&mut ctx, 400_000_000); // charge = 40_000_000

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE, // SAME price — nothing but funding moved
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "funding alone must be able to trigger the sweep"
    );

    // Control: the identical update with NO accrued funding leaves the position open, proving the
    // liquidation above is caused by funding and not by the price update.
    let mut ctl = make_ctx();
    setup_market(&mut ctl);
    save_position(&mut ctl, QTY, -ENTRY_VALUE);
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctl,
    )
    .unwrap();
    assert_eq!(position(&mut ctl, ALICE).amount, QTY, "control stays open");
}

#[test]
fn premium_accumulator_forward_fills_missing_slots_with_linear_weights() {
    let mut acc = PremiumIndexAccumulator::default();
    acc.start_epoch(15, 15, 100).unwrap();
    acc.fill_slots_until(60, 15, Some(300)).unwrap();

    // Slots: 15=100, 30=100, 45=100, 60=300.
    // Weighted average = (1*100 + 2*100 + 3*100 + 4*300) / 10 = 180.
    assert_eq!(acc.sample_count, 4);
    assert_eq!(acc.average().unwrap(), 180);
}

#[test]
fn mid_window_closes_interval_with_current_index_price() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_index_price_state(
        &mut ctx,
        MARKET_ID,
        &IndexPriceState {
            index_price: ENTRY_PRICE,
            timestamp: 0,
        },
    )
    .unwrap();

    storage::save_best_bid(&mut ctx, MARKET_ID, ENTRY_PRICE - 100).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, ENTRY_PRICE + 100).unwrap();

    ctx.block.timestamp = U256::from(20);
    record_mid_price_sample_for_best_quote_change(
        &mut ctx,
        MARKET_ID,
        ENTRY_PRICE - 100,
        ENTRY_PRICE + 100,
    )
    .unwrap();

    storage::save_best_ask(&mut ctx, MARKET_ID, ENTRY_PRICE + 300).unwrap();
    record_mid_price_sample_for_best_quote_change(
        &mut ctx,
        MARKET_ID,
        ENTRY_PRICE - 100,
        ENTRY_PRICE + 300,
    )
    .unwrap();

    let window = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(window.count, 1);
    assert_eq!(window.last_sample_ts, 20);
    assert_eq!(window.mid_prices[0], ENTRY_PRICE);
    assert_eq!(window.last_mid_price, ENTRY_PRICE);

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE + 10,
            timestamp: 16,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let window = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
    let state = storage::load_index_price_state(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(state.index_price, ENTRY_PRICE + 10);
    assert_eq!(state.timestamp, 15);
    assert_eq!(window.last_sample_ts, 20);
    let history = storage::load_index_price_history(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(window.moving_average_basis(&history, 15).unwrap(), 0);
    assert_eq!(window.moving_average_basis(&history, 30).unwrap(), -3);

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE + 20,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let window = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
    let state = storage::load_index_price_state(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(state.index_price, ENTRY_PRICE + 20);
    assert_eq!(state.timestamp, 30);
    assert_eq!(window.last_sample_ts, 20);
    assert_eq!(window.count, 1);
    let history = storage::load_index_price_history(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(window.moving_average_basis(&history, 30).unwrap(), -3);
}

#[test]
fn mid_window_uses_index_checkpoints_across_full_basis_window() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_index_price_state(
        &mut ctx,
        MARKET_ID,
        &IndexPriceState {
            index_price: ENTRY_PRICE,
            timestamp: 0,
        },
    )
    .unwrap();

    storage::save_best_bid(&mut ctx, MARKET_ID, ENTRY_PRICE).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, ENTRY_PRICE + 200).unwrap();
    ctx.block.timestamp = U256::from(1);
    record_mid_price_sample_for_best_quote_change(
        &mut ctx,
        MARKET_ID,
        ENTRY_PRICE,
        ENTRY_PRICE + 200,
    )
    .unwrap();

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE + 10,
            timestamp: 16,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: ENTRY_PRICE + 20,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    let window = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
    let history = storage::load_index_price_history(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(history.checkpoints.len(), 3);
    // [1,15): 100 basis for 14s; [15,30): 90 basis for 15s; [0,1): no mid = 0.
    assert_eq!(window.moving_average_basis(&history, 30).unwrap(), 91);
}

#[test]
fn mid_window_uses_ring_order_after_wrap() {
    let mut window = PriceBasisWindow::default();
    for timestamp in 1..=35 {
        window.push_sample(timestamp, ENTRY_PRICE + timestamp);
    }

    let mut history = IndexPriceHistory::default();
    history.push(
        IndexPriceState {
            index_price: ENTRY_PRICE,
            timestamp: 0,
        },
        10,
    );

    // The 30-slot ring now holds timestamps 6..=35. For [5,35), timestamp 35 is
    // right-exclusive, so [5,6) has no retained mid sample and contributes 0.
    assert_eq!(window.moving_average_basis(&history, 35).unwrap(), 19);
}

fn liquidate(ctx: &mut TestCtx, user: Address) -> Result<Bytes, PerpError> {
    let input = liquidateCall {
        user,
        marketId: MARKET_ID,
    }
    .abi_encode();
    run_liquidate(&input, KEEPER, ctx)
}

fn place_maker_order(ctx: &mut TestCtx, side: u8, price: u64, qty: u64) {
    place_order(ctx, MAKER, side, price, qty);
}

fn indexed_maker(index: usize) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = 0x44;
    bytes[12..].copy_from_slice(&u64::try_from(index).unwrap().to_be_bytes());
    Address::from(bytes)
}

fn place_order(ctx: &mut TestCtx, user: Address, side: u8, price: u64, qty: u64) {
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
    run_place_order(&input, user, ctx).unwrap();
}

fn try_place_order(
    ctx: &mut TestCtx,
    user: Address,
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
    run_place_order(&input, user, ctx)
}

fn set_leverage(ctx: &mut TestCtx, leverage: u64) -> Result<Bytes, PerpError> {
    run_set_leverage(
        &setLeverageCall {
            marketId: MARKET_ID,
            leverage,
        }
        .abi_encode(),
        ALICE,
        ctx,
    )
}

fn set_margin_tiers(
    ctx: &mut TestCtx,
    caller: Address,
    market_id: u64,
    lower_bounds: &[u64],
    max_leverages: &[u32],
) -> Result<Bytes, PerpError> {
    run_set_margin_tiers(
        &setMarginTiersCall {
            marketId: market_id,
            lowerBounds: lower_bounds.to_vec(),
            maxLeverages: max_leverages.to_vec(),
        }
        .abi_encode(),
        caller,
        ctx,
    )
}

fn get_margin_tiers(ctx: &mut TestCtx, market_id: u64) -> (Vec<u64>, Vec<u32>) {
    let out = run_get_margin_tiers(
        &getMarginTiersCall {
            marketId: market_id,
        }
        .abi_encode(),
        ctx,
    )
    .unwrap();
    let decoded = getMarginTiersCall::abi_decode_returns(&out).unwrap();
    (decoded.lowerBounds, decoded.maxLeverages)
}

fn wallet(ctx: &mut TestCtx, user: Address) -> u64 {
    storage::load_account(ctx, user)
        .unwrap()
        .visible_perp_wallet_balance()
}

fn position(ctx: &mut TestCtx, user: Address) -> PerpPosition {
    storage::load_position(ctx, user, MARKET_ID).unwrap()
}

fn save_position(ctx: &mut TestCtx, amount: i64, v_quote_balance: i64) {
    save_position_with_leverage(ctx, amount, v_quote_balance, 5);
}

/// `save_position` with an explicit leverage — the leverage tests need one INSIDE the
/// market's tier-0 cap (3) so `setLeverage` reaches the branch under test.
fn save_position_with_leverage(
    ctx: &mut TestCtx,
    amount: i64,
    v_quote_balance: i64,
    leverage: u64,
) {
    storage::save_position(
        ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount,
            v_quote_balance,
            margin: MARGIN,
            leverage,
            ..PerpPosition::default()
        },
    )
    .unwrap();
}

fn add_position_margin(ctx: &mut TestCtx, amount: u64) -> Result<Bytes, PerpError> {
    run_add_position_margin(
        &addPositionMarginCall {
            marketId: MARKET_ID,
            amount,
        }
        .abi_encode(),
        ALICE,
        ctx,
    )
}

fn remove_position_margin(ctx: &mut TestCtx, amount: u64) -> Result<Bytes, PerpError> {
    run_remove_position_margin(
        &removePositionMarginCall {
            marketId: MARKET_ID,
            amount,
        }
        .abi_encode(),
        ALICE,
        ctx,
    )
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
fn set_leverage_allows_increase_with_open_position() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position_with_leverage(&mut ctx, QTY, -ENTRY_VALUE, 2);

    set_leverage(&mut ctx, 3).unwrap();

    assert_eq!(position(&mut ctx, ALICE).leverage, 3);
}

#[test]
fn set_leverage_rejects_above_max_cap() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // The cap is tier 0's max_leverage — 3 for a default (single-tier) market.
    let err = set_leverage(&mut ctx, 4).unwrap_err();
    assert!(err.to_string().contains("leverage must be 1–3"), "{err}");
    assert!(set_leverage(&mut ctx, 3).is_ok(), "3x must be allowed");
}

#[test]
fn set_leverage_cap_follows_the_market_tier_table() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    assert!(set_leverage(&mut ctx, 3).is_ok(), "3x is the default cap");

    // Retune tier 0 down to 2x: the setLeverage cap must move with it.
    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0], &[2]).unwrap();

    let err = set_leverage(&mut ctx, 3).unwrap_err();
    assert!(err.to_string().contains("leverage must be 1–2"), "{err}");
    assert!(set_leverage(&mut ctx, 2).is_ok(), "2x is the new cap");
}

/// The cap is looked up by the position's CURRENT notional, not hardcoded to tier 0.
///
/// Tier 0 is only what that lookup collapses to for a flat position (notional 0) or a
/// single-tier table — both true today, which is exactly why hardcoding it would bake in a
/// special case that silently becomes wrong the moment a real table is configured. A trader
/// already sitting in a tighter bracket must be held to THAT bracket: otherwise they could
/// raise leverage past it here and `rebalance_order_margin_for_leverage` would release
/// resting-order margin down to a requirement their bracket does not allow.
///
/// `setup_market` runs base_decimals 0 / price_decimals 2 with mark $100.00, so
/// `notional = amount * 1e8` in 6-dp quote units: amount 4 → $400 (tier 0), amount 5 → $500
/// (tier 1).
#[test]
fn set_leverage_cap_follows_the_position_s_own_tier_not_tier_zero() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // [0, $500) → 3x ;  [$500, ∞) → 2x
    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 500_000_000], &[3, 2]).unwrap();

    // Flat: notional 0 lands in tier 0, so the loosest cap applies — this is the case that
    // makes a hardcoded `tiers[0]` look correct.
    assert!(
        set_leverage(&mut ctx, 3).is_ok(),
        "a flat position gets tier 0's 3x"
    );

    // Still inside tier 0 at $400.
    save_position_with_leverage(&mut ctx, 4, -400_000_000, 1);
    assert!(
        set_leverage(&mut ctx, 3).is_ok(),
        "$400 notional is still tier 0"
    );

    // $500 crosses into tier 1, whose cap is 2x — a hardcoded tiers[0] would wrongly allow 3x.
    save_position_with_leverage(&mut ctx, 5, -500_000_000, 1);
    let err = set_leverage(&mut ctx, 3).unwrap_err();
    assert!(
        err.to_string().contains("leverage must be 1–2"),
        "a position in the 2x tier must not be allowed 3x, got {err}"
    );
    assert!(
        set_leverage(&mut ctx, 2).is_ok(),
        "2x is allowed inside the 2x tier"
    );

    // A short of the same size is the same notional — the lookup uses |notional|.
    save_position_with_leverage(&mut ctx, -5, 500_000_000, 1);
    let err = set_leverage(&mut ctx, 3).unwrap_err();
    assert!(
        err.to_string().contains("leverage must be 1–2"),
        "the tier lookup must be side-agnostic, got {err}"
    );
}

// ── setMarginTiers / getMarginTiers ───────────────────────────────────────────

#[test]
fn add_market_installs_the_default_single_tier_table() {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    call_add_market(&mut ctx, 7, 10_000);

    assert_eq!(get_margin_tiers(&mut ctx, 7), (vec![0u64], vec![3u32]));
}

#[test]
fn update_market_preserves_the_tier_table() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_margin_tiers(
        &mut ctx,
        ADMIN,
        MARKET_ID,
        &[0, 1_000_000, 5_000_000],
        &[3, 2, 1],
    )
    .unwrap();

    run_update_market(
        &crate::interface::IPerpDex::updateMarketCall {
            marketId: MARKET_ID,
            tickSize: 2,
            stepSize: 1,
            minQuantity: 1,
            maxQuantity: 1_000_000,
            maxPrice: 1_000_000,
            priceUpdateInterval: 30,
            active: false,
            fundingInterval: 0,
            interestRate: 0,
            liquidationFeeRateBps: 7,
            priceBandBps: 0,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    // Retuning tick/step/funding must not touch the risk table.
    assert_eq!(
        get_margin_tiers(&mut ctx, MARKET_ID),
        (vec![0, 1_000_000, 5_000_000], vec![3, 2, 1])
    );
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    assert_eq!(
        market.tick_size, 2,
        "the rest of updateMarket still applied"
    );
}

#[test]
fn set_margin_tiers_accepts_and_round_trips_a_multi_tier_table() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);

    set_margin_tiers(
        &mut ctx,
        ADMIN,
        MARKET_ID,
        &[0, 50_000_000_000, 250_000_000_000],
        &[3, 2, 1],
    )
    .unwrap();

    assert_eq!(
        get_margin_tiers(&mut ctx, MARKET_ID),
        (vec![0, 50_000_000_000, 250_000_000_000], vec![3, 2, 1])
    );
    // And it survives a storage round-trip through the msgpack blob.
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    let bytes = perp_core::codec::encode(&market).unwrap();
    let decoded: crate::types::Market = perp_core::codec::decode(&bytes).unwrap();
    assert_eq!(decoded.tiers, market.tiers);
}

#[test]
fn set_margin_tiers_accepts_the_maximum_table_size() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let bounds: Vec<u64> = (0..crate::types::MAX_MARGIN_TIERS as u64)
        .map(|i| i * 1_000_000)
        .collect();
    let levs: Vec<u32> = (0..crate::types::MAX_MARGIN_TIERS as u32)
        .map(|i| 8 - i)
        .collect();

    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &bounds, &levs).unwrap();

    assert_eq!(get_margin_tiers(&mut ctx, MARKET_ID), (bounds, levs));
}

#[test]
fn set_margin_tiers_requires_admin_or_market_manager() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);

    let err = set_margin_tiers(&mut ctx, ALICE, MARKET_ID, &[0], &[2]).unwrap_err();
    assert!(err.to_string().contains("not authorised"), "{err}");
    // Unchanged.
    assert_eq!(get_margin_tiers(&mut ctx, MARKET_ID), (vec![0], vec![3]));
}

/// Every invariant gets its OWN error string, and every reject leaves the stored table
/// untouched (validate-then-apply: perp writes are commit-only).
#[test]
fn set_margin_tiers_rejects_each_invariant_individually() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);

    let cases: &[(&[u64], &[u32], &str)] = &[
        (
            &[0, 1_000],
            &[3],
            "lowerBounds and maxLeverages length mismatch",
        ),
        (&[], &[], "at least one tier required"),
        (
            &[0, 1, 2, 3, 4, 5, 6, 7, 8],
            &[9, 8, 7, 6, 5, 4, 3, 2, 1],
            "at most 8 tiers allowed",
        ),
        (&[1_000], &[3], "first tier must start at 0"),
        (
            &[0, 1_000, 1_000],
            &[3, 2, 1],
            "lowerBounds must be strictly increasing",
        ),
        (
            &[0, 2_000, 1_000],
            &[3, 2, 1],
            "lowerBounds must be strictly increasing",
        ),
        (&[0], &[0], "maxLeverage must be 1–100"),
        (&[0], &[101], "maxLeverage must be 1–100"),
        (&[0, 1_000], &[2, 3], "maxLeverages must be non-increasing"),
    ];

    for (bounds, levs, expected) in cases {
        let err = set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, bounds, levs).unwrap_err();
        assert!(
            err.to_string().contains(expected),
            "expected {expected:?}, got {err}"
        );
        assert_eq!(
            get_margin_tiers(&mut ctx, MARKET_ID),
            (vec![0], vec![3]),
            "a reject must not write ({expected})"
        );
    }
}

#[test]
fn set_margin_tiers_rejects_unknown_market() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let err = set_margin_tiers(&mut ctx, ADMIN, 999, &[0], &[2]).unwrap_err();
    assert!(err.to_string().contains("unknown market"), "{err}");
}

#[test]
fn get_margin_tiers_rejects_unknown_market() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let err = run_get_margin_tiers(&getMarginTiersCall { marketId: 999 }.abi_encode(), &mut ctx)
        .unwrap_err();
    assert!(err.to_string().contains("unknown market"), "{err}");
}

#[test]
fn set_margin_tiers_emits_the_full_replacement_table() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 1_000_000], &[3, 2]).unwrap();

    let updated: Vec<_> = JournalTr::take_logs(ctx.journal_mut())
        .into_iter()
        .filter(|log| {
            log.data.topics().first() == Some(&IPerpDex::MarginTiersUpdated::SIGNATURE_HASH)
        })
        .map(|log| {
            IPerpDex::MarginTiersUpdated::decode_raw_log(log.data.topics(), &log.data.data).unwrap()
        })
        .collect();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].marketId, MARKET_ID);
    assert_eq!(updated[0].lowerBounds, vec![0, 1_000_000]);
    assert_eq!(updated[0].maxLeverages, vec![3, 2]);
}

// ── Per-open margin-tier guard ────────────────────────────────────────────────

/// Fund ALICE, sit her at `leverage`, and rest a MAKER sell of `qty` at the mark so an
/// ALICE buy of `qty` opens a fresh long of that size.
fn stage_open_into_tier(ctx: &mut TestCtx, leverage: u64, qty: u64) {
    storage::save_account(
        ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: 2_000_000_000,
            ..UserAccount::default()
        },
    )
    .unwrap();
    set_leverage(ctx, leverage).unwrap();
    place_order(ctx, MAKER, Side::Sell as u8, ENTRY_PRICE, qty);
}

/// v1 no-op proof: under the DEFAULT single-tier table the guard can never fire, because
/// its bound is exactly the `setLeverage` cap.
#[test]
fn per_open_tier_guard_is_a_no_op_under_the_default_table() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // qty 2 at $100 = 200_000_000 quote units of notional — well past any tier boundary
    // the multi-tier test below installs.
    stage_open_into_tier(&mut ctx, 3, 2);

    try_place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, 2).unwrap();

    assert_eq!(position(&mut ctx, ALICE).amount, 2);
}

/// NOT redundant with K9: this position is solvent at mark (K9 passes) and is refused
/// purely because its size crossed into a tier whose max leverage is below the position's.
#[test]
fn per_open_tier_guard_rejects_growing_into_a_lower_leverage_tier() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    stage_open_into_tier(&mut ctx, 3, 2);
    // Tier 1 starts below the 200_000_000 notional this open produces and caps leverage at 1.
    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 150_000_000], &[3, 1]).unwrap();

    let err = try_place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, 2).unwrap_err();

    assert!(
        err.to_string()
            .contains("leverage exceeds the margin tier for this position size"),
        "{err}"
    );
    // Pre-write reject: no position, and the maker's ask is untouched.
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    assert_eq!(position(&mut ctx, MAKER).amount, 0);
}

/// The same open is ACCEPTED once the trader's leverage is inside the reached tier's cap —
/// so the guard gates on the tier, not on the trade.
#[test]
fn per_open_tier_guard_accepts_leverage_inside_the_reached_tier() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    stage_open_into_tier(&mut ctx, 1, 2);
    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 150_000_000], &[3, 1]).unwrap();

    try_place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, 2).unwrap();

    assert_eq!(position(&mut ctx, ALICE).amount, 2);
}

/// The maker leg of the same guard: a resting maker whose fill would grow him into a
/// lower-leverage tier is refused (his order is cancelled) instead of being filled.
#[test]
fn per_open_tier_guard_rejects_the_maker_leg_too() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_account(
        &mut ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: 2_000_000_000,
            ..UserAccount::default()
        },
    )
    .unwrap();
    // MAKER rests at 3x BEFORE the retune (resting itself is never tier-gated).
    run_set_leverage(
        &setLeverageCall {
            marketId: MARKET_ID,
            leverage: 3,
        }
        .abi_encode(),
        MAKER,
        &mut ctx,
    )
    .unwrap();
    place_order(&mut ctx, MAKER, Side::Sell as u8, ENTRY_PRICE, 2);

    set_margin_tiers(&mut ctx, ADMIN, MARKET_ID, &[0, 150_000_000], &[3, 1]).unwrap();

    // ALICE is at the default 1x, so HER leg is fine; the maker's is not.
    try_place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, 2).unwrap();

    assert_eq!(position(&mut ctx, MAKER).amount, 0, "maker fill refused");
    assert_eq!(position(&mut ctx, ALICE).amount, 0, "nothing matched");
}

#[test]
fn set_leverage_rejects_decrease_with_open_position() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position_with_leverage(&mut ctx, QTY, -ENTRY_VALUE, 3);

    let err = set_leverage(&mut ctx, 2).unwrap_err();
    assert!(
        err.to_string()
            .contains("cannot reduce leverage with open position"),
        "{err}"
    );
    assert_eq!(position(&mut ctx, ALICE).leverage, 3);
}

#[test]
fn set_leverage_decrease_without_position_tops_up_order_margin() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_account(
        &mut ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: 500_000_000,
            ..UserAccount::default()
        },
    )
    .unwrap();

    // Order notional is 1e9; leverage 3 -> reserve 333_333_333, leverage 2 -> reserve 500M
    // (both within the tier-0 leverage cap of 3).
    set_leverage(&mut ctx, 3).unwrap();
    place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, QTY as u64);
    assert_eq!(position(&mut ctx, ALICE).margin_reserved, 333_333_333);
    assert_eq!(wallet(&mut ctx, ALICE), 166_666_667);

    set_leverage(&mut ctx, 2).unwrap();

    let pos = position(&mut ctx, ALICE);
    assert_eq!(pos.leverage, 2);
    assert_eq!(pos.margin_reserved, 500_000_000);
    assert_eq!(wallet(&mut ctx, ALICE), 0);
}

#[test]
fn set_leverage_decrease_without_position_rejects_when_order_margin_topup_is_unfunded() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_account(
        &mut ctx,
        ALICE,
        UserAccount {
            perp_wallet_balance: 400_000_000,
            ..UserAccount::default()
        },
    )
    .unwrap();

    // Order notional 1e9; leverage 3 -> reserve 333_333_333 (wallet 400M -> 66_666_667).
    // Decreasing to leverage 2 needs reserve 500M (+166_666_667 topup) which 66_666_667
    // cannot fund -> reject.
    set_leverage(&mut ctx, 3).unwrap();
    place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, QTY as u64);

    let err = set_leverage(&mut ctx, 2).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for order margin"),
        "{err}"
    );
    let pos = position(&mut ctx, ALICE);
    assert_eq!(pos.leverage, 3);
    assert_eq!(pos.margin_reserved, 333_333_333);
    assert_eq!(wallet(&mut ctx, ALICE), 66_666_667);
}

#[test]
fn add_position_margin_moves_wallet_balance_into_position_margin() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    add_position_margin(&mut ctx, 10_000_000).unwrap();

    let position_changes = take_position_changes(&mut ctx);
    assert_eq!(position_changes.len(), 1);
    assert_eq!(position_changes[0].realizedPnl, 0);
    assert_eq!(position_changes[0].closedQuantity, 0);
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET - 10_000_000);
    assert_eq!(position(&mut ctx, ALICE).margin, MARGIN + 10_000_000);
}

#[test]
fn add_position_margin_rejects_insufficient_wallet() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let err = add_position_margin(&mut ctx, USER_WALLET + 1).unwrap_err();
    assert!(
        err.to_string().contains("insufficient perp wallet balance"),
        "{err}"
    );
}

#[test]
fn remove_position_margin_returns_safe_excess_margin_to_wallet() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    add_position_margin(&mut ctx, 30_000_000).unwrap();

    remove_position_margin(&mut ctx, 30_000_000).unwrap();

    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET);
    assert_eq!(position(&mut ctx, ALICE).margin, MARGIN);
}

/// B2: MAINTENANCE margin is the only requirement gate. The initial-margin gate that used to sit
/// in front of it is gone (Binance checks MM continuously and never re-checks IM).
///
/// Same scenario as before the change — remove $40 from a $200 margin on a $1 000 position — and
/// it is still REFUSED, but by the maintenance gate, which is the tighter and more correct one:
/// notional $1 000 ⇒ MM = 1000/6 = $166.67, and $200 − $40 = $160 < $166.67.
#[test]
fn remove_position_margin_rejects_when_it_would_breach_maintenance() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let err = remove_position_margin(&mut ctx, 40_000_000).unwrap_err();
    assert!(
        err.to_string().contains("below maintenance margin"),
        "the maintenance gate, not the removed initial-margin gate: {err}"
    );
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET);
    assert_eq!(position(&mut ctx, ALICE).margin, MARGIN);

    // And the boundary is genuinely the maintenance threshold, not the old IM one: removing
    // $33.33 leaves $166_666_667, one unit above MM = $166_666_666, and is ALLOWED. Under the
    // old gate every one of these was refused, because IM = N/5 = $200 == the whole margin.
    assert!(remove_position_margin(&mut ctx, 33_333_333).is_ok());
    assert_eq!(position(&mut ctx, ALICE).margin, 166_666_667);
}

/// B2, the case that made the old gate unusable: at a market's MAX leverage a freshly opened
/// position is already BELOW the initial-margin requirement, because since `7cc26360` the
/// opening fill funds the trading fee out of the margin (`floor(N/L) − fee`). The old gate
/// therefore refused EVERY removal — and even refused `add(X)` followed by `remove(X)`, a round
/// trip that leaves the position exactly where it started.
#[test]
fn remove_position_margin_round_trips_at_max_leverage() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // Max leverage 3, and margin one fee-tick BELOW floor(N/3) — exactly the post-fill state.
    let post_fee_margin = 1_000_000_000i64 / 3 - 1_000;
    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: QTY,
            v_quote_balance: -ENTRY_VALUE,
            margin: post_fee_margin,
            leverage: 3,
            ..PerpPosition::default()
        },
    )
    .unwrap();
    let wallet_before = wallet(&mut ctx, ALICE);

    // add then remove the same amount: a no-op round trip that the old IM gate rejected.
    add_position_margin(&mut ctx, 10_000_000).unwrap();
    remove_position_margin(&mut ctx, 10_000_000).unwrap();
    assert_eq!(position(&mut ctx, ALICE).margin, post_fee_margin);
    assert_eq!(wallet(&mut ctx, ALICE), wallet_before);

    // And genuine excess above maintenance is withdrawable: MM = 1000/6 = 166_666_666, so
    // dropping from 333_332_333 to 233_332_333 stays comfortably clear.
    remove_position_margin(&mut ctx, 100_000_000).unwrap();
    assert_eq!(position(&mut ctx, ALICE).margin, post_fee_margin - 100_000_000);
}

#[test]
fn remove_position_margin_rejects_when_result_below_maintenance() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // Long 10 @ $100, margin $200, leverage 5.
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    // Mark drops to $90 → position carries a $100 unrealized loss.
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();

    // Removing $20 passes the (PnL-blind) initial-margin check — new margin
    // $180 == notional($900)/leverage(5) — but leaves equity at
    // 900 − 1000 + 180 = $80, below the maintenance threshold $900/6 = $150.
    // The PnL-aware maintenance check must reject it.
    let err = remove_position_margin(&mut ctx, 20_000_000).unwrap_err();
    assert!(
        err.to_string().contains("below maintenance margin"),
        "{err}"
    );
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET);
    assert_eq!(position(&mut ctx, ALICE).margin, MARGIN);
}

#[test]
fn remove_position_margin_rejects_when_mark_price_unset() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    // Market with no oracle price yet: mark price 0 must not be usable.
    storage::save_mark_price(&mut ctx, MARKET_ID, 0).unwrap();

    let err = remove_position_margin(&mut ctx, 10_000_000).unwrap_err();
    assert!(err.to_string().contains("mark price unavailable"), "{err}");
    assert_eq!(position(&mut ctx, ALICE).margin, MARGIN);
}

#[test]
fn liquidate_rejects_when_mark_price_unset() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    // Without a mark price the maintenance gate degenerates and would wrongly
    // treat a healthy position as bankrupt; liquidation must refuse instead.
    storage::save_mark_price(&mut ctx, MARKET_ID, 0).unwrap();

    let err = liquidate(&mut ctx, ALICE).unwrap_err();
    assert!(err.to_string().contains("mark price unavailable"), "{err}");
    assert_eq!(position(&mut ctx, ALICE).amount, QTY);
}

#[test]
fn remove_position_margin_settles_pending_funding_first() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_funding_index(&mut ctx, 75_000_000); // charge 7.5M for a QTY long
                                             // Long with excess margin so a 50M removal is allowed after funding.
    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: QTY,
            v_quote_balance: -ENTRY_VALUE,
            margin: 400_000_000,
            leverage: 5,
            ..PerpPosition::default()
        },
    )
    .unwrap();

    remove_position_margin(&mut ctx, 50_000_000).unwrap();

    let pos = position(&mut ctx, ALICE);
    // A1: funding (7.5M) is charged to the MARGIN (400M → 392.5M), then 50M of margin is returned
    // to the wallet. The wallet therefore only sees the removal, never the funding.
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET + 50_000_000);
    assert_eq!(pos.margin, 400_000_000 - 7_500_000 - 50_000_000);
    assert_eq!(pos.last_funding_index, 75_000_000);
}

#[test]
fn add_position_margin_settles_pending_funding_first() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_funding_index(&mut ctx, 75_000_000); // charge 7.5M for a QTY long
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    add_position_margin(&mut ctx, 10_000_000).unwrap();

    let pos = position(&mut ctx, ALICE);
    // A1: 7.5M funding charged to the margin, then 10M moved wallet → margin. Only the 10M
    // top-up leaves the wallet.
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET - 10_000_000);
    assert_eq!(pos.margin, MARGIN - 7_500_000 + 10_000_000);
    assert_eq!(pos.last_funding_index, 75_000_000);
}

#[test]
fn liquidate_settles_funding_into_insolvency() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // Healthy long at mark $100 (equity 200M > maintenance 166.7M), but a large
    // pending funding charge wipes margin and pushes it under maintenance.
    set_funding_index(&mut ctx, 1_200_000_000); // charge = 120M > wallet 50M
    save_position(&mut ctx, QTY, -ENTRY_VALUE); // wallet 50M, margin 200M
    place_maker_order(&mut ctx, Side::Buy as u8, ENTRY_PRICE, QTY as u64);

    // Without funding settlement this position is above maintenance and would
    // be rejected; funding must be applied first so liquidation proceeds.
    liquidate(&mut ctx, ALICE).unwrap();

    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "position fully liquidated"
    );
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
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET + 100_000_000 - LONG_LIQ_TAKER_FEE
    );
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
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET + 100_000_000 - SHORT_LIQ_TAKER_FEE
    );
    assert_eq!(wallet(&mut ctx, KEEPER), 0);
}

#[test]
fn liquidate_settles_residual_at_mark_when_orderbook_cannot_fully_close() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    // Book only provides QTY-1 of closing liquidity.
    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, (QTY as u64) - 1);

    // New behaviour: close what the book can absorb, then settle the 1-unit
    // residual directly at mark price. The position is fully closed, not rejected.
    liquidate(&mut ctx, ALICE).unwrap();

    let alice = position(&mut ctx, ALICE);
    assert_eq!(
        alice.amount, 0,
        "position fully closed via book + residual-at-mark"
    );
    assert_eq!(alice.v_quote_balance, 0);
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
        USER_WALLET + 33_000_000 + 100_000_000 - LONG_LIQ_TAKER_FEE
    );
    assert_eq!(position(&mut ctx, ALICE).margin_reserved, 0);
}

// ── mark_price > 0 is a MARKET-LIFETIME INVARIANT ───────────────────────────
//
// Every market that exists has a non-zero mark, and no reachable call can zero it:
//   * `addMarket` rejects `initialMarkPrice == 0` and stores it as the market's mark;
//   * `updateMarket` never touches `mark_price`;
//   * `updateIndexPrice` rejects `indexPrice == 0`, and each of the three mark
//     components is floored away from zero (`price1`/`price2` via `.max(1)`,
//     `contract_price` falls back to the non-zero index before any trade), so their
//     median is >= 1 and the tick-snap floor keeps it >= `tick_size`.
//
// These tests pin that chain. Margin/risk math is allowed to treat a live market's
// mark as non-zero — the `mark_price > 0` guards elsewhere are defensive only, and
// reachable exclusively by writing a mark-0 Market straight to storage (which the
// `*_when_mark_price_unset` tests do deliberately). If any of these fail, that
// assumption is broken and every consumer of the mark must be re-audited.

/// The `addMarket` gas floor from the selector table.
const ADD_MARKET_GAS: u64 = 100_000;

/// A valid `addMarket` payload for `market_id`, parameterised on the initial mark.
fn add_market_call(market_id: u64, initial_mark_price: u64) -> Vec<u8> {
    crate::interface::IPerpDex::addMarketCall {
        marketId: market_id,
        baseDecimals: 0,
        priceDecimals: PRICE_DECIMALS,
        tickSize: 1,
        stepSize: 1,
        minQuantity: 1,
        maxQuantity: 1_000_000,
        maxPrice: 1_000_000,
        priceUpdateInterval: 15,
        fundingInterval: 0,
        interestRate: 0,
        liquidationFeeRateBps: 0,
        initialMarkPrice: initial_mark_price,
        priceBandBps: 0,
    }
    .abi_encode()
}

fn call_add_market(ctx: &mut TestCtx, market_id: u64, initial_mark_price: u64) -> Bytes {
    let output = run_perp_dex_call(
        &add_market_call(market_id, initial_mark_price),
        ADD_MARKET_GAS,
        ADMIN,
        U256::ZERO,
        false,
        ctx,
    )
    .unwrap();
    assert!(
        !output.reverted,
        "addMarket reverted: {:?}",
        String::from_utf8_lossy(output.bytes.as_ref())
    );
    output.bytes
}

#[test]
fn add_market_rejects_zero_initial_mark_price() {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();

    let output = run_perp_dex_call(
        &add_market_call(7, 0),
        ADD_MARKET_GAS,
        ADMIN,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();

    assert!(output.reverted, "a zero initialMarkPrice must be rejected");
    let reason = String::from_utf8_lossy(output.bytes.as_ref()).to_string();
    assert!(
        reason.contains("initialMarkPrice must be > 0"),
        "unexpected revert reason: {reason}"
    );
    // Rejected pre-write: no market was created.
    assert!(storage::load_market(&mut ctx, 7).unwrap().is_none());
}

#[test]
fn add_market_stores_a_nonzero_mark_price() {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();

    call_add_market(&mut ctx, 7, ENTRY_PRICE);

    assert_eq!(
        storage::load_mark_price(&mut ctx, 7).unwrap(),
        ENTRY_PRICE,
        "the initial mark must be the market's mark from creation"
    );
}

#[test]
fn mark_price_stays_nonzero_across_index_updates() {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    call_add_market(&mut ctx, 7, ENTRY_PRICE);

    // Walk the mark down toward zero with the smallest index the market accepts (1 tick).
    // No trade has happened, so contract_price falls back to the index — this is the
    // component combination most likely to floor at zero if the guards were missing.
    let mut ts = 15u64;
    for index_price in [ENTRY_PRICE / 2, ENTRY_PRICE / 10, 100, 10, 1] {
        let output = run_perp_dex_call(
            &updateIndexPriceCall {
                marketId: 7,
                indexPrice: index_price,
                timestamp: ts,
            }
            .abi_encode(),
            50_000,
            ADMIN,
            U256::ZERO,
            false,
            &mut ctx,
        )
        .unwrap();
        assert!(
            !output.reverted,
            "updateIndexPrice({index_price}) reverted: {:?}",
            String::from_utf8_lossy(output.bytes.as_ref())
        );
        let mark = storage::load_mark_price(&mut ctx, 7).unwrap();
        assert!(
            mark > 0,
            "mark hit zero at index {index_price} — the mark>0 invariant is broken"
        );
        ts += 15;
    }
}

#[test]
fn update_index_price_rejects_zero_index() {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    call_add_market(&mut ctx, 7, ENTRY_PRICE);

    let output = run_perp_dex_call(
        &updateIndexPriceCall {
            marketId: 7,
            indexPrice: 0,
            timestamp: 15,
        }
        .abi_encode(),
        50_000,
        ADMIN,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();

    assert!(output.reverted, "a zero indexPrice must be rejected");
    // The mark is untouched by the rejected update.
    assert_eq!(storage::load_mark_price(&mut ctx, 7).unwrap(), ENTRY_PRICE);
}

#[test]
fn update_market_cannot_zero_the_mark_price() {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    call_add_market(&mut ctx, 7, ENTRY_PRICE);

    let output = run_perp_dex_call(
        &crate::interface::IPerpDex::updateMarketCall {
            marketId: 7,
            tickSize: 1,
            stepSize: 1,
            minQuantity: 1,
            maxQuantity: 1_000_000,
            maxPrice: 1_000_000,
            priceUpdateInterval: 15,
            active: true,
            fundingInterval: 0,
            interestRate: 0,
            liquidationFeeRateBps: 0,
            priceBandBps: 0,
        }
        .abi_encode(),
        ADD_MARKET_GAS,
        ADMIN,
        U256::ZERO,
        false,
        &mut ctx,
    )
    .unwrap();
    assert!(
        !output.reverted,
        "updateMarket reverted: {:?}",
        String::from_utf8_lossy(output.bytes.as_ref())
    );

    // updateMarket carries no mark field, so the mark survives untouched.
    assert_eq!(storage::load_mark_price(&mut ctx, 7).unwrap(), ENTRY_PRICE);
}
