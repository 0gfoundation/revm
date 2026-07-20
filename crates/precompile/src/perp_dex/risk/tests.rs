use super::*;
use alloy_sol_types::SolCall;
use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, U256};

use crate::perp_dex::{
    funding::settle_position_funding,
    interface::IPerpDex::{
        addPositionMarginCall, liquidateCall, placeOrderCall, removePositionMarginCall,
        setLeverageCall, updateIndexPriceCall,
    },
    trading::run_place_order,
    types::{
        FundingState, IndexPriceHistory, PerpPosition, PremiumIndexAccumulator, PriceBasisWindow,
        UserAccount, UserFeeRates,
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

    // Crash the mark to $70 via updateIndexPrice (admin) — runs the sweep.
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: 7_000,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    // ALICE was under maintenance -> swept (closed at mark, empty book -> residual);
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
    let mut account = storage::load_account(ctx, ALICE).unwrap();
    settle_position_funding(
        ctx,
        ALICE,
        &market,
        &mut pos,
        &mut account.perp_wallet_balance,
    )
    .unwrap();
    (pos, account)
}

#[test]
fn settle_funding_long_pays_from_wallet() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx); // ALICE wallet = USER_WALLET = 50_000_000
    set_funding_index(&mut ctx, 75_000_000); // charge for QTY long = 7_500_000
    save_position(&mut ctx, QTY, -ENTRY_VALUE); // anchor defaults to 0

    let (pos, account) = settle_alice_funding(&mut ctx);

    assert_eq!(account.perp_wallet_balance, USER_WALLET as i64 - 7_500_000);
    assert_eq!(
        pos.margin, MARGIN,
        "wallet covered the charge; margin untouched"
    );
    assert_eq!(pos.last_funding_index, 75_000_000, "re-anchored to index");
}

#[test]
fn settle_funding_short_receives_into_wallet() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_funding_index(&mut ctx, 75_000_000);
    save_position(&mut ctx, -QTY, ENTRY_VALUE); // short receives when rate > 0

    let (pos, account) = settle_alice_funding(&mut ctx);

    assert_eq!(account.perp_wallet_balance, USER_WALLET as i64 + 7_500_000);
    assert_eq!(pos.margin, MARGIN);
}

#[test]
fn settle_funding_charge_waterfalls_wallet_then_margin() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_funding_index(&mut ctx, 600_000_000); // charge = 60_000_000 > wallet 50M
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let (pos, account) = settle_alice_funding(&mut ctx);

    assert_eq!(account.perp_wallet_balance, 0, "wallet drained to 0 first");
    assert_eq!(
        pos.margin,
        MARGIN - 10_000_000,
        "remainder taken from margin"
    );
}

#[test]
fn settle_funding_charge_beyond_margin_absorbs_from_insurance_fund() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    set_funding_index(&mut ctx, 3_000_000_000); // charge = 300M > wallet 50M + margin 200M
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let (pos, account) = settle_alice_funding(&mut ctx);

    assert_eq!(account.perp_wallet_balance, 0);
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
    let out = liquidate_position(&mut ctx, ALICE, MARKET_ID, &market, ENTRY_PRICE, KEEPER).unwrap();
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
    set_funding_index(&mut ctx, 3_000_000_000); // charge 300M > wallet 50M + margin 200M → dips IF
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();

    // Funding drains the wallet to 0 → the add-margin can't be covered → reject.
    let err = add_position_margin(&mut ctx, 1_000_000).unwrap_err();
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
    let topic = crate::perp_dex::interface::IPerpDex::FundingSettled::SIGNATURE_HASH;
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

fn liquidate(ctx: &mut TestCtx, user: Address) -> Result<Bytes, PrecompileError> {
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

fn set_leverage(ctx: &mut TestCtx, leverage: u64) -> Result<Bytes, PrecompileError> {
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

fn wallet(ctx: &mut TestCtx, user: Address) -> u64 {
    storage::load_account(ctx, user)
        .unwrap()
        .visible_perp_wallet_balance()
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

fn add_position_margin(ctx: &mut TestCtx, amount: u64) -> Result<Bytes, PrecompileError> {
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

fn remove_position_margin(ctx: &mut TestCtx, amount: u64) -> Result<Bytes, PrecompileError> {
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
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    set_leverage(&mut ctx, 6).unwrap();

    assert_eq!(position(&mut ctx, ALICE).leverage, 6);
}

#[test]
fn set_leverage_rejects_above_max_cap() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // Cap is 6 (aligned with the 1/6 maintenance rate so opens never breach it).
    let err = set_leverage(&mut ctx, 7).unwrap_err();
    assert!(err.to_string().contains("leverage must be 1–6"), "{err}");
    assert!(set_leverage(&mut ctx, 6).is_ok(), "6x must be allowed");
}

#[test]
fn set_leverage_rejects_decrease_with_open_position() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let err = set_leverage(&mut ctx, 4).unwrap_err();
    assert!(
        err.to_string()
            .contains("cannot reduce leverage with open position"),
        "{err}"
    );
    assert_eq!(position(&mut ctx, ALICE).leverage, 5);
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

    // Order notional is 1e9; leverage 5 -> reserve 200M, leverage 2 -> reserve 500M
    // (both divide 1e9 cleanly and stay within the 1–6 leverage cap).
    set_leverage(&mut ctx, 5).unwrap();
    place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, QTY as u64);
    assert_eq!(position(&mut ctx, ALICE).margin_reserved, 200_000_000);
    assert_eq!(wallet(&mut ctx, ALICE), 300_000_000);

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
            perp_wallet_balance: 250_000_000,
            ..UserAccount::default()
        },
    )
    .unwrap();

    // Order notional 1e9; leverage 5 -> reserve 200M (wallet 250M -> 50M). Decreasing
    // to leverage 2 needs reserve 500M (+300M topup) which 50M cannot fund -> reject.
    set_leverage(&mut ctx, 5).unwrap();
    place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, QTY as u64);

    let err = set_leverage(&mut ctx, 2).unwrap_err();
    assert!(
        err.to_string()
            .contains("insufficient perp wallet for order margin"),
        "{err}"
    );
    let pos = position(&mut ctx, ALICE);
    assert_eq!(pos.leverage, 5);
    assert_eq!(pos.margin_reserved, 200_000_000);
    assert_eq!(wallet(&mut ctx, ALICE), 50_000_000);
}

#[test]
fn add_position_margin_moves_wallet_balance_into_position_margin() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    add_position_margin(&mut ctx, 10_000_000).unwrap();

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

#[test]
fn remove_position_margin_rejects_below_initial_margin_requirement() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let err = remove_position_margin(&mut ctx, 40_000_000).unwrap_err();
    assert!(
        err.to_string().contains("below initial margin requirement"),
        "{err}"
    );
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET);
    assert_eq!(position(&mut ctx, ALICE).margin, MARGIN);
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
    // Funding (7.5M) charged from wallet first, then 50M margin returned to wallet.
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET - 7_500_000 + 50_000_000
    );
    assert_eq!(pos.margin, 350_000_000);
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
    // 7.5M funding charged from wallet, then 10M moved wallet → margin.
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET - 7_500_000 - 10_000_000
    );
    assert_eq!(pos.margin, MARGIN + 10_000_000);
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
