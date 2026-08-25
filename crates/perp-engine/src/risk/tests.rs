use context::ContextTr;
use super::*;
use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, U256};

use crate::{
    funding::settle_position_funding,
    interface::IPerpDex::{
        addPositionMarginCall, getMarginTiersCall, getSymbolConfigCall, liquidateCall,
        placeOrderCall, removePositionMarginCall, setLeverageCall, setMarginTiersCall,
        updateIndexPriceCall,
    },
    run_perp_dex_call,
    trading::{run_place_order, MAX_LIQUIDATION_MAKER_ACCOUNTS},
    types::{
        AccountUpdateReason, FundingState, IndexPriceHistory, MarginTiers, PerpPosition,
        PremiumIndexAccumulator, PriceBasisWindow, UserAccount, UserFeeRates,
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
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    storage::save_account(
        ctx,
        MAKER,
        UserAccount {
            perp_wallet_balance: MAKER_WALLET as i64,
            ..UserAccount::default()
        },
        AccountUpdateReason::Adjustment,
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
        AccountUpdateReason::Adjustment,
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
        AccountUpdateReason::Adjustment,
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
            AccountUpdateReason::Adjustment,
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
        AccountUpdateReason::Adjustment,
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
        AccountUpdateReason::Adjustment,
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
        AccountUpdateReason::Adjustment,
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

/// REGRESSION for the stale-mark band centre: `run_update_index_price` used to hand the liquidation
/// sweep the `Market` it loaded BEFORE `save_mark_price`, so the maintenance check ran against the
/// NEW mark while `match_order` centred the fill-time price band on the OLD one — disagreeing by
/// exactly the move that triggered the sweep.
///
/// This fixture is the discriminating one: a bid at $80 with the mark moving $100 -> $75 is INSIDE
/// the correct band ([$67.50, $82.50]) and OUTSIDE the stale one ([$90, $110]). So the book must
/// absorb the close here. Under the bug it absorbed nothing and the residual routed to ADL —
/// inverting the intended book -> insurance-fund -> ADL precedence exactly when the sweep matters.
///
/// (Its sibling `adl_skips_opposite_holder_whose_only_order_reserves_no_margin` sits at $60, which
/// is out of band under BOTH centres and therefore cannot detect this; that is why this test
/// exists separately rather than as an assertion added there.)
#[test]
fn the_sweep_bands_the_close_on_the_post_update_mark_not_the_pre_update_one() {
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
    seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 5, 0);
    // KEEPER bids his whole short back at $80: pure reduce, reserves nothing, and in band once the
    // mark is $75.
    place_order(&mut ctx, KEEPER, 0, 8_000, QTY as u64);

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

    // The book took it: ALICE's long is gone and KEEPER's short closed against it. Under the stale
    // centre this bid was unreachable, ALICE would still hold `QTY`, and the residual would have
    // gone to ADL instead.
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "the close must be absorbed by the in-band bid, not routed past it"
    );
    assert_eq!(
        position(&mut ctx, KEEPER).amount,
        0,
        "the counterparty's short is closed by his own resting bid"
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
    // $60 is deep enough to sit below the fill-time band, which is what keeps the liquidation's
    // market sell from simply eating this bid — the whole residual has to reach ADL for the
    // predicate to be exercised. It does not move the mark: price1 and the contract price both
    // stay at the index, so their median is the index regardless of the basis this bid contributes.
    //
    // The band is centred on the POST-update mark of $75, so ±10% = [$67.50, $82.50]. This fixture
    // used to sit at $80 and relied on the sweep banding against the PRE-update mark of $100
    // ([$90, $110]) — i.e. on the stale-mark bug this file's sweep no longer has. $80 is in band
    // under the correct centre, so the bid would be eaten and ADL never reached: the assertions
    // below would still pass while testing nothing. $60 restores the scenario rather than the
    // expectation.
    place_order(&mut ctx, KEEPER, 0, 6_000, QTY as u64);
    let keeper_pos = position(&mut ctx, KEEPER);
    let keeper_market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    assert_eq!(
        crate::margin_view::position_open_order_margin(&keeper_market, &keeper_pos).unwrap(),
        0,
        "pure-reduce order requires no open-order margin — this is what makes any \
         `requirement != 0` proxy insufficient for 'has resting orders'"
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
    // Flat → no funding owed. Written directly rather than through the `save_position` helper
    // because that one always allocates `MARGIN`, and a FLAT position holding margin is a state the
    // engine cannot produce (every close zeroes `margin` alongside `amount`) — `storage::save_position`
    // now `debug_assert`s exactly that. The margin value is irrelevant to both assertions below.
    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: 0,
            v_quote_balance: 0,
            margin: 0,
            leverage: 5,
            ..PerpPosition::default()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();

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

    // CHANGED BY THE ESCROW REMOVAL: the placement itself debits NOTHING, so the wallet still
    // reads 50M. The claim under test is unchanged and is now expressed on the derived basis:
    // market 2's requirement (`ooIM` = 50M notional at 1x) consumed the whole available balance,
    // proving all 50M was spendable despite the market-1 funding charge.
    let market2 = storage::load_market(&mut ctx, OTHER_MARKET)
        .unwrap()
        .unwrap();
    let pos2 = storage::load_position(&mut ctx, ALICE, OTHER_MARKET).unwrap();
    assert_eq!(
        crate::margin_view::position_open_order_margin(&market2, &pos2).unwrap(),
        USER_WALLET,
        "the whole wallet is committable on market 2"
    );
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET,
        "resting escrows nothing — the wallet is untouched by the placement"
    );
    assert_eq!(
        crate::margin_view::derived_available_balance(&mut ctx, ALICE).unwrap(),
        0,
        "and the full 50M was available to commit — market-1 funding took none of it"
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

/// The DERIVED open-order requirement for `user` in the test market — the replacement for the
/// deleted `pos.margin_reserved` field in every assertion that used to read it.
fn oo_im(ctx: &mut TestCtx, user: Address) -> u64 {
    let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
    let pos = storage::load_position(ctx, user, MARKET_ID).unwrap();
    crate::margin_view::position_open_order_margin(&market, &pos).unwrap()
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
        AccountUpdateReason::Adjustment,
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

/// `getSymbolConfig` through the FULL call shell in a STATIC context — which is also the assertion
/// that the selector is registered `can_be_static = true` and priced at the flat `getPosition`
/// tier (5_000), not at the derived-margin-view tier.
fn symbol_config(ctx: &mut TestCtx, user: Address, market_id: u64) -> (u64, u64) {
    let input = getSymbolConfigCall {
        user,
        marketId: market_id,
    }
    .abi_encode();
    let out = run_perp_dex_call(&input, 1_000_000, user, U256::ZERO, true, ctx).unwrap();
    assert!(!out.reverted, "getSymbolConfig reverted: {:?}", out.bytes);
    assert_eq!(
        out.gas_used, 5_000,
        "getSymbolConfig's gas must stay FLAT per selector — never per-tier or dynamic"
    );
    let d = getSymbolConfigCall::abi_decode_returns(&out.bytes).unwrap();
    (d.leverage, d.maxNotionalValue)
}

/// The DEFAULT configuration: one tier `{0, 3}`, whose band runs to infinity, so
/// `maxNotionalValue` is `0` (UNBOUNDED) at every leverage — including for a user who has never
/// touched this market and reads back the unset `leverage = 1`.
#[test]
fn get_symbol_config_is_unbounded_on_the_default_single_tier_table() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);

    assert_eq!(
        symbol_config(&mut ctx, ALICE, MARKET_ID),
        (1, 0),
        "no position: leverage reads back the unset 1, and the single tier is unbounded"
    );

    for leverage in 1..=3 {
        set_leverage(&mut ctx, leverage).unwrap();
        assert_eq!(
            symbol_config(&mut ctx, ALICE, MARKET_ID),
            (leverage, 0),
            "single-tier table is unbounded at leverage {leverage}"
        );
    }
}

/// The BOUNDED configuration — the one that separates this selector from a client re-deriving the
/// bound out of `getMarginTiers`. `[{0, 3}, {$200, 2}, {$400, 1}]`, checked at every leverage in
/// `1..=3`: the reported bound is the upper edge of the LAST tier whose `maxLeverage` still admits
/// the leverage in force, and `0` when that tier is the final one.
///
/// ```text
/// leverage 3 -> only tier 0 admits it   -> table[1].lower = $200 = 200_000_000
/// leverage 2 -> tiers 0,1 admit it      -> table[2].lower = $400 = 400_000_000
/// leverage 1 -> all three admit it; tier 2 is FINAL -> 0 (unbounded)
/// ```
#[test]
fn get_symbol_config_reports_the_band_edge_at_each_leverage() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_margin_tiers(
        &mut ctx,
        ADMIN,
        MARKET_ID,
        &[0, 200_000_000, 400_000_000],
        &[3, 2, 1],
    )
    .unwrap();

    for (leverage, expected) in [(1u64, 0u64), (2, 400_000_000), (3, 200_000_000)] {
        set_leverage(&mut ctx, leverage).unwrap();
        assert_eq!(
            symbol_config(&mut ctx, ALICE, MARKET_ID),
            (leverage, expected),
            "leverage {leverage}"
        );
    }
}

/// The tier table is a required input, so an unknown market REVERTS rather than reporting `0` —
/// which would read as "unbounded" for a market that does not exist. Same shape as
/// `getMarginTiers` and `getMarginInfo`, deliberately NOT `getPosition`'s all-zeros.
#[test]
fn get_symbol_config_rejects_unknown_market() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let err = run_get_symbol_config(
        &getSymbolConfigCall {
            user: ALICE,
            marketId: 999,
        }
        .abi_encode(),
        &mut ctx,
    )
    .unwrap_err();
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
        AccountUpdateReason::Adjustment,
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
        AccountUpdateReason::Adjustment,
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

/// CHANGED BY THE DERIVED-ooIM SWITCH (values, not outcome). Two mechanisms move the numbers:
///   * the requirement is `ROUND_UP(Bid / L)`, where the escrow floored: `1e9 / 3` is
///     333_333_334 here, not 333_333_333;
///   * nothing is debited, so the WALLET stays at its funded value and the pressure shows up in
///     the derived available (`wallet − Σ ooIM`) instead.
/// The behaviour under test — a leverage DECREASE raises the open-order requirement and must be
/// funded — is unchanged, and it still exactly exhausts the account.
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
        AccountUpdateReason::Adjustment,
    )
    .unwrap();

    // Order notional (Bid) is 1e9, position flat so N = 0; ooIM = ROUND_UP(1e9 / L):
    // leverage 3 -> 333_333_334, leverage 2 -> 500_000_000 (both within the tier-0 cap of 3).
    set_leverage(&mut ctx, 3).unwrap();
    place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, QTY as u64);
    assert_eq!(oo_im(&mut ctx, ALICE), 333_333_334);
    assert_eq!(
        wallet(&mut ctx, ALICE),
        500_000_000,
        "placement debits nothing"
    );
    assert_eq!(
        crate::margin_view::derived_available_balance(&mut ctx, ALICE).unwrap(),
        166_666_666
    );

    // The top-up needed is 500_000_000 − 333_333_334 = 166_666_666 — exactly the available.
    set_leverage(&mut ctx, 2).unwrap();

    let pos = position(&mut ctx, ALICE);
    assert_eq!(pos.leverage, 2);
    assert_eq!(oo_im(&mut ctx, ALICE), 500_000_000);
    assert_eq!(wallet(&mut ctx, ALICE), 500_000_000);
    assert_eq!(
        crate::margin_view::derived_available_balance(&mut ctx, ALICE).unwrap(),
        0
    );
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
        AccountUpdateReason::Adjustment,
    )
    .unwrap();

    // CHANGED BY THE DERIVED-ooIM SWITCH (values, not outcome) — see the previous test.
    // Order notional 1e9; leverage 3 -> ooIM 333_333_334, leaving 66_666_666 available out of
    // the 400M wallet. Decreasing to leverage 2 needs 500M (+166_666_666) which 66_666_666
    // cannot fund -> reject, exactly as before.
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
    assert_eq!(oo_im(&mut ctx, ALICE), 333_333_334);
    assert_eq!(wallet(&mut ctx, ALICE), 400_000_000);
    assert_eq!(
        crate::margin_view::derived_available_balance(&mut ctx, ALICE).unwrap(),
        66_666_666
    );
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
        AccountUpdateReason::Adjustment,
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
        AccountUpdateReason::Adjustment,
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

/// CHANGED BY THE ESCROW REMOVAL (mechanism, not outcome). This used to assert that liquidation
/// REFUNDS the cancelled orders' escrowed margin to the wallet — the fixture poked
/// `margin_reserved = 33_000_000` in by hand and the wallet came back 33_000_000 richer. There is
/// no escrow to refund: a cancel moves no money. What liquidation's cancel-all does is clear the
/// position's `Bid`/`Ask`, which drops its open-order REQUIREMENT to 0 and so frees the same
/// headroom without any transfer. The test now uses a REAL resting order (the hand-poked field is
/// gone) and pins both halves: the wallet gets exactly the position settlement and not a unit
/// more, and the requirement is 0 afterwards.
#[test]
fn liquidate_clears_the_open_order_requirement_without_refunding_anything() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    // A real resting bid, deep enough not to be hit by the liquidation's market sell (the fill
    // band around the $90 mark is [$81, $99]). 3 units @ $27.00 = 81_000_000 of `Bid`; against
    // the long's N = 900_000_000 at mark $90 and leverage 5:
    //   ooIM = ROUND_UP(981_000_000 / 5) − ROUND_UP(900_000_000 / 5) = 16_200_000,
    // comfortably inside ALICE's $50 wallet.
    place_order(&mut ctx, ALICE, Side::Buy as u8, 2_700, 3);
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    assert_eq!(
        oo_im(&mut ctx, ALICE),
        16_200_000,
        "requirement while resting"
    );
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET,
        "placing it debited nothing"
    );
    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64);

    liquidate(&mut ctx, ALICE).unwrap();

    // Only the position settlement reaches the wallet — no reservation refund term.
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET + 100_000_000 - LONG_LIQ_TAKER_FEE
    );
    // Cancel-all cleared the book side, so the requirement is gone.
    assert_eq!(oo_im(&mut ctx, ALICE), 0);
    let pos = position(&mut ctx, ALICE);
    assert_eq!((pos.total_buy_qty, pos.total_buy_notional), (0, 0));
    assert!(
        storage::load_buy_orders(&mut ctx, ALICE, MARKET_ID)
            .unwrap()
            .is_empty(),
        "cancel-all emptied the entry list"
    );
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

// ── VALUE CONSERVATION (the primary correctness gate for the escrow removal) ─────────────────
//
// Deleting the open-order escrow moves money between buckets, so the thing that has to be proved
// is that no bucket gains or loses a unit the others do not account for.
//
// # The identity
//
// Every USDC unit inside the perp system sits in exactly one of three places, and one accounting
// adjustment closes the loop:
//
// ```text
// E(p) = Σ_users perp_wallet_balance            // free collateral (the CROSS wallet)
//      + Σ_positions margin                     // collateral allocated to a position
//      + insurance_fund                         // the mutualised buffer
//      + Σ_positions (v_quote_balance + signed_value(p, amount))   // unrealised PnL at price p
// ```
//
// Notes on what is deliberately NOT in it:
//
// * **The open-order requirement is absent, and that is the point.** `Σ ooIM` is derived, never
//   held: after this migration there is no third bucket for it. (Before the migration the same
//   identity carried a `Σ margin_reserved` term.)
// * **`market_fee_total` is a counter, not a balance.** Trading fees are credited to the ADMIN's
//   `perp_wallet_balance`, which is already inside `Σ_users` — the admin is a user. Adding the
//   counter as well would double-count every fee.
// * **The unrealised-PnL term is needed** because a fill moves value between `v_quote_balance`
//   and the wallet/margin. Both counterparties of a fill attribute the SAME single-floored
//   `calc_value(price, qty)` (the split-floor rule), so `Σ v_quote` and `Σ amount` are each
//   conserved by trading and the term nets to zero across the book — but it must be present for
//   the per-user sum to balance.
// * **It is evaluated at a PRICE.** `E(p) = C + p·A` with `C` and `A` both readable, and a
//   liquidation residual that the book could not absorb is settled against no counterparty at the
//   CURRENT mark: that changes `C` and `A` individually but leaves `E(mark)` fixed. So every
//   assertion below evaluates the before-state and the after-state at the SAME price — the mark
//   in force after the operation, which is the mark any residual settled at.
//
// # What breaks it, legitimately
//
// * **Socialised bad debt** (`InsuranceFundDepleted`): value the insurance fund could not cover
//   is written off, and `E` rises by the uncovered amount. The fund is seeded far past anything
//   these scenarios can produce, and the test asserts the event never fires.
// * **Funding**, which is not a zero-sum transfer between longs and shorts here (it settles each
//   position against the index and spills into the insurance fund). Disabled in this market.
// * **`transferToPerp` / `transferFromPerp`**, which are genuine external flows. Not used here.
#[cfg(test)]
mod value_conservation {
    use super::*;

    const N: u64 = 8;
    const SEED_WALLET: i64 = 400_000_000; // $400 each
    const SEED_IF: u64 = 10_000_000_000; // deep enough that bad debt is always covered

    fn user(i: u64) -> Address {
        let mut b = [0u8; 20];
        b[0] = 0x7C;
        b[12..20].copy_from_slice(&i.to_be_bytes());
        Address::from(b)
    }

    /// `(C, A)`: the price-independent part of the identity, and the net open interest.
    /// `C = Σ(wallet + margin + v_quote) + insurance_fund`, `A = Σ amount`.
    fn state(ctx: &mut TestCtx) -> (i128, i128) {
        let mut c = storage::load_insurance_fund(ctx).unwrap() as i128;
        let mut a: i128 = 0;
        for i in 0..N {
            let u = user(i);
            let acc = storage::load_account(ctx, u).unwrap();
            let p = storage::load_position(ctx, u, MARKET_ID).unwrap();
            c += acc.perp_wallet_balance as i128 + p.margin as i128 + p.v_quote_balance as i128;
            a += p.amount as i128;
        }
        // ADMIN is the trading-fee sink, so it is part of the closed system.
        c += storage::load_account(ctx, ADMIN)
            .unwrap()
            .perp_wallet_balance as i128;
        (c, a)
    }

    /// `E(p) = C + signed_value(p, A)`. `base_decimals = 0` and `price_decimals = 2` here, so
    /// `calc_value` is exact (`p × q × 10^4`) and the per-user sum equals the value of the sum.
    fn equity(c: i128, a: i128, price: u64) -> i128 {
        let v = crate::math::calc_value(price, a.unsigned_abs() as u64, 0, PRICE_DECIMALS).unwrap()
            as i128;
        c + if a >= 0 { v } else { -v }
    }

    fn framed<F: FnOnce(&mut TestCtx) -> Result<Bytes, PerpError>>(ctx: &mut TestCtx, f: F) {
        // EVM-framed exactly like on-chain: a genuine reject reverts the frame and must leave the
        // system value untouched, so rejects are part of what this test covers.
        let cp = ctx.journal_mut().checkpoint();
        match f(ctx) {
            Ok(_) => ctx.journal_mut().checkpoint_commit(),
            Err(PerpError::Reject(_)) => ctx.journal_mut().checkpoint_revert(cp),
            Err(e) => panic!("hard failure: {e:?}"),
        }
        ctx.journal_mut().commit_tx();
    }

    /// Fills, partial fills, cancels, leverage changes, liquidations and ADL, in a randomised
    /// order, with the total system value re-checked after EVERY operation.
    #[test]
    fn a_randomised_operation_sequence_conserves_total_system_value() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        // A live clearance fee, so the liquidation → insurance-fund leg is exercised (it is a
        // transfer inside the identity, not an inflow).
        let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
        m.liquidation_fee_rate_bps = 50;
        m.price_band_bps = 2_000; // ±20%, wide enough for the mark walk below
        storage::save_market(&mut ctx, &m).unwrap();
        storage::save_insurance_fund(&mut ctx, SEED_IF).unwrap();
        for i in 0..N {
            let u = user(i);
            storage::save_account(
                &mut ctx,
                u,
                UserAccount {
                    perp_wallet_balance: SEED_WALLET,
                    // Live fee rates: fees must move between users and ADMIN without leaking.
                    maker_fee_bps: 5,
                    taker_fee_bps: 10,
                    ..UserAccount::default()
                },
                AccountUpdateReason::Adjustment,
            )
            .unwrap();
        }

        let mut s: u64 = 0xC0FFEE_1234_5678;
        let mut rng = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };

        let mut resting: Vec<Option<[u8; 32]>> = vec![None; N as usize];
        let (mut c0, mut a0) = state(&mut ctx);
        let mut index_price: u64 = ENTRY_PRICE;
        let mut ts: u64 = 100;
        let mut liquidations = 0u32;
        let mut fills = 0u32;
        // Wallets that were negative going INTO this step, so a NEW negative can be attributed to
        // the step that produced it (see the gate below).
        let mut was_negative: Vec<bool> = vec![false; N as usize];
        let mut negative_wallet_steps = 0u32;

        for op in 0..1200u32 {
            let i = (rng() % N) as usize;
            let who = user(i as u64);
            let roll = rng() % 100;
            let step_can_debit_a_wallet = (20..90).contains(&roll);

            if roll < 12 {
                if let Some(id) = resting[i].take() {
                    framed(&mut ctx, |c| {
                        crate::trading::run_cancel_order(
                            &crate::interface::IPerpDex::cancelOrderCall {
                                orderId: id.into(),
                                marketId: MARKET_ID,
                            }
                            .abi_encode(),
                            who,
                            c,
                        )
                    });
                }
            } else if roll < 20 {
                let lev = 1 + rng() % 3;
                framed(&mut ctx, |c| {
                    run_set_leverage(
                        &setLeverageCall {
                            marketId: MARKET_ID,
                            leverage: lev,
                        }
                        .abi_encode(),
                        who,
                        c,
                    )
                });
            } else if roll < 90 {
                // Place: passive limits build the book, aggressive limits cross it (fills,
                // partial fills, and the taker cover path when the account is tight).
                let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
                let side = (rng() % 2) as u8;
                let aggressive = rng() % 100 < 40;
                let offset = (1 + rng() % 8) as u64 * 100; // $1..$8
                let price = if (side == 0) == aggressive {
                    mark.saturating_add(offset)
                } else {
                    mark.saturating_sub(offset).max(100)
                };
                let qty = 1 + rng() % 6;
                let before = storage::load_position(&mut ctx, who, MARKET_ID)
                    .unwrap()
                    .amount;
                let mut id = None;
                framed(&mut ctx, |c| {
                    let r = try_place_order(c, who, side, price, qty);
                    if let Ok(bytes) = &r {
                        id = Some(bytes[..32].try_into().unwrap());
                    }
                    r
                });
                if let Some(id) = id {
                    resting[i] = Some(id);
                }
                if storage::load_position(&mut ctx, who, MARKET_ID)
                    .unwrap()
                    .amount
                    != before
                {
                    fills += 1;
                }
            } else {
                // Oracle update: moves the mark and runs the liquidation sweep (which runs ADL on
                // any insolvent residual the book cannot absorb). The index is walked as a
                // SAWTOOTH between $60 and $160 rather than a random walk — positions here run at
                // leverage <= 3 against a 1/6 maintenance rate, so only a sustained ~35% adverse
                // move actually puts one under water, and the point of this test is to reach the
                // liquidation / residual-settle / ADL paths, not to wander near the entry price.
                let target: i64 = if (op / 150) % 2 == 0 { 6_000 } else { 16_000 };
                let drift = (target - index_price as i64) / 6;
                let jitter = (rng() % 600) as i64 - 300;
                index_price = (index_price as i64 + drift + jitter).clamp(4_000, 20_000) as u64;
                ts += 16;
                let registry_before = storage::load_position_registry(&mut ctx, MARKET_ID)
                    .unwrap()
                    .len();
                framed(&mut ctx, |c| {
                    run_update_index_price(
                        &updateIndexPriceCall {
                            marketId: MARKET_ID,
                            indexPrice: index_price,
                            timestamp: ts,
                        }
                        .abi_encode(),
                        ADMIN,
                        c,
                    )
                });
                let registry_after = storage::load_position_registry(&mut ctx, MARKET_ID)
                    .unwrap()
                    .len();
                liquidations += registry_before.saturating_sub(registry_after) as u32;
            }

            // ── The gate ──
            // Both sides evaluated at the POST-operation mark: that is the price any liquidation
            // residual was settled at, and the only price at which the identity closes across it.
            let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
            let (c1, a1) = state(&mut ctx);
            assert_eq!(
                equity(c1, a1, mark),
                equity(c0, a0, mark),
                "value leaked at op {op}: {} (mark {mark})",
                equity(c1, a1, mark) - equity(c0, a0, mark)
            );
            // Socialised bad debt would make `E` rise LEGITIMATELY, so the gate above would stop
            // meaning anything. `absorb_from_insurance_fund` can only leave a remainder once the
            // fund is exhausted, so a strictly-positive fund proves no write-off happened. (The
            // events themselves are unreadable here: `commit_tx` drains the journal's log buffer.)
            assert!(
                storage::load_insurance_fund(&mut ctx).unwrap() > 0,
                "insurance fund exhausted at op {op} — E is no longer comparable"
            );
            // ── A negative wallet is NO LONGER an invariant violation ──
            // This used to assert `w >= 0` on the grounds that an underfunded maker fill was
            // REFUSED. It is not any more: the fill happens and the wallet absorbs the shortfall
            // (Binance never sweeps an under-covered lien; see `settle_maker_fill_core`). So the
            // blanket assertion would now be asserting the OLD behaviour.
            //
            // What replaces it is the attribution: a wallet may only CROSS into negative on a step
            // that actually debits one, i.e. a place/fill. Nothing else in the system may produce a
            // deficit — a cancel moves no money, `setLeverage` moves no money, funding settles
            // against `pos.margin` (A1 isolated funding), and liquidation only ever CREDITS the
            // wallet (`settle_liquidation_residual_at_mark_price`: an insolvent residual routes its
            // shortfall to the Insurance Fund and never debits the user). If a mark move or a
            // liquidation sweep ever produced a negative wallet, that would be a real bug, and this
            // catches it while the relaxed assertion above no longer would.
            //
            // Value conservation itself is unaffected and is still the primary gate above: the
            // deficit is a TRANSFER (wallet down, `pos.margin` up), never a mint.
            for k in 0..N {
                let w = storage::load_account(&mut ctx, user(k))
                    .unwrap()
                    .perp_wallet_balance;
                let idx = k as usize;
                if w < 0 && !was_negative[idx] {
                    negative_wallet_steps += 1;
                    assert!(
                        step_can_debit_a_wallet,
                        "user {k} wallet went negative ({w}) at op {op} on a step that debits no \
                         wallet (roll {roll}: cancel / setLeverage / oracle+liquidation)"
                    );
                }
                was_negative[idx] = w < 0;
            }
            c0 = c1;
            a0 = a1;
        }

        // The scenario has to actually reach the interesting paths, or it proves nothing.
        println!(
            "conservation scenario: {fills} fills, {liquidations} liquidations, \
             {negative_wallet_steps} wallet-goes-negative steps"
        );
        assert!(fills > 100, "too few fills: {fills}");
        assert!(liquidations > 0, "no position was ever liquidated");
        // ADL is deliberately NOT asserted here: it needs an insolvent residual the book could not
        // absorb AND an opposite-side holder with no resting orders at all, which a book-building
        // fuzz almost never produces. It gets its own deterministic leg below (and
        // `adl_closes_insolvent_residual_against_opposite_holder_conserving_no_if` above covers
        // the mechanics).
    }

    /// The ADL leg of the same identity: a forced close against a real counterparty at the
    /// liquidated position's bankruptcy price must move value between the two participants and
    /// create none. Deterministic, because the fuzz above cannot reliably reach it.
    #[test]
    fn an_adl_forced_close_conserves_total_system_value() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_insurance_fund(&mut ctx, SEED_IF).unwrap();

        // user(0): 5x long 10 @ $100 (margin $200, vq −$1000) — the position that goes under.
        // user(1): the sole opposite holder, 5x short 10 @ $100, OFF-BOOK (no resting orders),
        //          deeply in profit once the mark crashes. This is the ADL counterparty.
        // (Same shape as `adl_closes_insolvent_residual_against_opposite_holder_conserving_no_if`,
        // re-measured here against the full system identity rather than a two-user sum.)
        for (i, amount, vq) in [(0u64, QTY, -ENTRY_VALUE), (1, -QTY, ENTRY_VALUE)] {
            storage::save_account(
                &mut ctx,
                user(i),
                UserAccount {
                    perp_wallet_balance: SEED_WALLET,
                    ..UserAccount::default()
                },
                AccountUpdateReason::Adjustment,
            )
            .unwrap();
            storage::save_position(
                &mut ctx,
                user(i),
                MARKET_ID,
                &PerpPosition {
                    amount,
                    v_quote_balance: vq,
                    margin: MARGIN,
                    leverage: 5,
                    ..PerpPosition::default()
                },
                AccountUpdateReason::Adjustment,
            )
            .unwrap();
        }

        let (c0, a0) = state(&mut ctx);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        // Crash to $75, past the long's $80 bankruptcy price, with an EMPTY book — so the
        // liquidation's market close fills nothing and the whole insolvent residual reaches ADL.
        // Called directly, not through `framed`: `commit_tx` drains the journal's log buffer, and
        // the ADL / depletion events are what this test reads back.
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

        let logs = JournalTr::take_logs(ctx.journal_mut());
        let adls = logs
            .iter()
            .filter(|l| {
                l.data.topics().first() == Some(&crate::interface::IPerpDex::Adl::SIGNATURE_HASH)
            })
            .count();
        let depleted = logs
            .iter()
            .filter(|l| {
                l.data.topics().first()
                    == Some(&crate::interface::IPerpDex::InsuranceFundDepleted::SIGNATURE_HASH)
            })
            .count();
        assert!(adls > 0, "the residual did not reach ADL");
        assert_eq!(
            depleted, 0,
            "no socialised write-off — E must be comparable"
        );

        let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
        let (c1, a1) = state(&mut ctx);
        assert_eq!(
            equity(c1, a1, mark),
            equity(c0, a0, mark),
            "ADL leaked {}",
            equity(c1, a1, mark) - equity(c0, a0, mark)
        );
        // ADL is a real trade, so it conserves Σ amount too — unlike a residual settled at mark.
        assert_eq!(a1, a0, "ADL must not mint or burn open interest");
        assert_eq!(
            storage::load_position(&mut ctx, user(0), MARKET_ID)
                .unwrap()
                .amount,
            0,
            "the insolvent residual was fully deleveraged"
        );
    }

    /// The UNDERFUNDED MAKER FILL leg of the same identity. A maker whose wallet cannot cover the
    /// opening margin FILLS anyway (`settle_maker_fill_core`), with the silo funded only to the cash
    /// at hand — model **M1**, measured by R11 (`derived-ooim-plan.md` §3a).
    ///
    /// **Why this leg still matters after the M1 switch.** Capping the opening margin is exactly the
    /// kind of change that looks conservative and mints: the cap has to move the wallet debit DOWN
    /// by the same amount it moves the silo credit down, or value appears/disappears. The identity
    /// `C = Σ(wallet + margin + v_quote) + IF` is blind to WHERE the shortfall sits — M1 and M1′
    /// conserve equally well, they just split `wallet + margin` differently — so it is the only
    /// gate that catches the two halves drifting apart.
    ///
    /// Deterministic, because the fuzz above cannot reliably reach it: its users start with $400
    /// each at leverage <= 3, and draining one to within a hair of an opening margin by chance is
    /// not something 1200 random ops produce.
    ///
    /// MUTATION-CHECKED, both directions of the M1 cap:
    /// * deleting `trial_wallet -= opening_margin` in `settle_maker_fill_core` (what "just let the
    ///   fill through" naively looks like) leaves the silo funded from nowhere — `equity` gains the
    ///   whole opening margin;
    /// * dropping the `.min((*wallet).max(0) as u64)` cap in `apply_position_fill` (i.e. reverting
    ///   to M1′) is caught by the two split assertions below, not by the conservation one.
    #[test]
    fn an_underfunded_maker_fill_conserves_total_system_value() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_insurance_fund(&mut ctx, SEED_IF).unwrap();

        let maker = user(0);
        let taker = user(1);
        // Short 10 @ $100 at leverage 1 needs exactly $1 000 of opening margin.
        let opening_margin: i64 = 1_000_000_000;
        for (u, w) in [(maker, 2 * opening_margin), (taker, 2 * opening_margin)] {
            storage::save_account(
                &mut ctx,
                u,
                UserAccount {
                    perp_wallet_balance: w,
                    ..UserAccount::default()
                },
                AccountUpdateReason::Adjustment,
            )
            .unwrap();
        }

        // The sell is admitted while the maker CAN afford it, then the wallet is drained to ONE
        // UNIT short — the production shape (a fee, a funding charge or an adverse mark move
        // between admission and fill).
        place_order(&mut ctx, maker, 1, ENTRY_PRICE, QTY as u64);
        let mut acc = storage::load_account(&mut ctx, maker).unwrap();
        acc.perp_wallet_balance = opening_margin - 1;
        storage::save_account(&mut ctx, maker, acc, AccountUpdateReason::Adjustment).unwrap();

        let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
        let (c0, a0) = state(&mut ctx);

        place_order(&mut ctx, taker, 0, ENTRY_PRICE, QTY as u64);

        // The fill happened, the SILO is short by the missing unit, and the wallet is emptied to
        // exactly zero: `opening_margin = min(1_000_000_000, 999_999_999)`.
        let maker_pos = storage::load_position(&mut ctx, maker, MARKET_ID).unwrap();
        assert_eq!(maker_pos.amount, -QTY, "the short was opened");
        assert_eq!(
            maker_pos.margin,
            opening_margin - 1,
            "silo funded to the cash at hand and no further — short by the 1 unit (M1)"
        );
        assert_eq!(
            storage::load_account(&mut ctx, maker)
                .unwrap()
                .perp_wallet_balance,
            0,
            "the wallet is emptied, not driven negative (M1′ would read −1 here)"
        );

        // ── The gate: a shortfall is a TRANSFER, not a mint ──
        let (c1, a1) = state(&mut ctx);
        assert_eq!(
            equity(c1, a1, mark),
            equity(c0, a0, mark),
            "underfunded maker fill leaked {}",
            equity(c1, a1, mark) - equity(c0, a0, mark)
        );
        assert_eq!(a1, a0, "a fill must not mint or burn open interest");
        assert_eq!(
            storage::load_insurance_fund(&mut ctx).unwrap(),
            SEED_IF,
            "the shortfall is NOT socialised at fill time — it rides on the position, and only \
             reaches the fund if that position later closes insolvent (usdc_custody covers that)"
        );
    }
}

// ── USDC CUSTODY — the EXTERNAL leg of conservation ────────────────────────────────────────────
//
// `mod value_conservation` above proves that an INTERNAL sum is conserved by every operation. That
// is necessary but NOT sufficient to answer "is a persistent negative `perp_wallet_balance` a
// receivable or a leak?", because the negative wallet is one of the terms of that sum: a
// self-consistent ledger stays self-consistent whether the number in it is collectable or fiction.
// The question that CAN tell them apart is whether the ledger still matches the real, on-trie USDC
// the precompile actually custodies.
//
// # The custody identity, derived from the write sites
//
// The DEX's ERC-20 USDC balance is written in exactly TWO places in the whole engine —
// `account::deposit_withdraw::run_deposit` (credit) and `run_withdraw` (debit); `grep
// save_erc20_balance` finds no third. So custody moves only on genuine external flows, and
// everything else in the engine can only ever REDISTRIBUTE it between internal buckets:
//
// ```text
// D  = erc20_balance(USDC, PERP_DEX_ADDRESS)             deposit_withdraw.rs:52 / :101
//
// D == Σ_users usdc_balance                              spot leg   (deposit_withdraw.rs:53/:99,
//                                                                    :136/:180)
//   +  Σ_users perp_wallet_balance     ← SIGNED          cross wallet (types/account.rs:62)
//   +  Σ_positions margin                                isolated silo (types/position.rs)
//   +  insurance_fund                                    mutualised buffer (storage.rs:2015)
//   +  Σ_positions (v_quote_balance + signed_value(p, amount))    mark-to-market
// ```
//
// with two things worth naming:
//
// * **`Σ_users` must include ADMIN** — trading fees are credited to the admin's own
//   `perp_wallet_balance` (`credit_fee_recipient`), and the insurance fund is seeded out of it
//   (`run_deposit_insurance_fund` debits the admin's wallet), so the admin is a user like any
//   other. `market_fee_total` is a COUNTER, not a bucket; adding it would double-count.
// * **The mark-to-market term is evaluated at ONE price** and vanishes whenever net open interest
//   `Σ amount` is zero, which is the case at every checkpoint below (every position change here is
//   half of a real two-sided trade). The scenario therefore checks a PRICE-INDEPENDENT identity,
//   and asserts `Σ amount == 0` so that claim is not taken on trust.
//
// The one legitimate way to break it is a socialised write-off (`InsuranceFundDepleted`), where the
// fund could not cover a loss and the excess is forgiven; the scenario asserts that never fires.
#[cfg(test)]
mod usdc_custody {
    use super::*;
    use crate::{
        account::{
            run_deposit, run_get_account, run_transfer_from_perp, run_transfer_to_perp,
            run_withdraw,
        },
        interface::IPerpDex::{
            depositCall, getAccountCall, getAccountMarginCall, transferFromPerpCall,
            transferToPerpCall, withdrawCall,
        },
        margin_view::run_get_account_margin,
        storage::keys::erc20_balance_slot,
    };

    /// The maker whose position ends up carrying the deficit.
    const MK: Address = address!("00000000000000000000000000000000000000d1");
    /// Taker of MK's opening sell.
    const T1: Address = address!("00000000000000000000000000000000000000d2");
    /// Taker of MK's flipping buy — the fill that leaves MK's silo SHORT.
    const T2: Address = address!("00000000000000000000000000000000000000d3");
    /// Rests the punitive bid MK's liquidation is forced to close into.
    const LQ: Address = address!("00000000000000000000000000000000000000d4");

    /// Every account whose claims are part of the closed system. ADMIN is in it: it is the fee sink
    /// and the source of the insurance-fund seed.
    const HOLDERS: [Address; 5] = [MK, T1, T2, LQ, ADMIN];

    const MK_DEPOSIT: u64 = 400_000_000; // $400 — all of MK's own money, ever
    const MK_TOP_UP: u64 = 20_000_000; //   $20 deposited AFTER the liquidation
    const T1_DEPOSIT: u64 = 2_000_000_000;
    const T2_DEPOSIT: u64 = 3_000_000_000;
    const LQ_DEPOSIT: u64 = 1_000_000_000;
    const IF_SEED: u64 = 1_000_000_000;

    const BUY: u8 = 0;
    const SELL: u8 = 1;

    /// Total USDC that ever enters the DEX in this scenario.
    const TOTAL_DEPOSITED: i128 =
        (MK_DEPOSIT + MK_TOP_UP + T1_DEPOSIT + T2_DEPOSIT + LQ_DEPOSIT + IF_SEED) as i128;

    fn make_ctx_with_usdc(seeds: &[(Address, u64)]) -> TestCtx {
        let mut db = InMemoryDB::default();
        for (addr, amount) in seeds {
            db.insert_account_storage(
                USDC_ADDRESS,
                erc20_balance_slot(*addr).into(),
                U256::from(*amount),
            )
            .unwrap();
        }
        let mut ctx: TestCtx = Context::new(db, SpecId::CANCUN);
        for addr in [USDC_ADDRESS, PERP_DEX_ADDRESS]
            .iter()
            .copied()
            .chain(HOLDERS)
        {
            JournalTr::load_account(ctx.journal_mut(), addr).unwrap();
        }
        ctx
    }

    /// The test market. Deliberately NOT `setup_market`: that one seeds two wallets by direct
    /// write, which is money from nowhere and would make the custody identity meaningless. Here
    /// every unit of collateral arrives through `deposit`.
    fn setup_market_no_funding(ctx: &mut TestCtx) {
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
                // Funding OFF: it is not a zero-sum transfer inside the identity (it settles each
                // position against the index and spills into the insurance fund), and it is not
                // what is under test here.
                funding_interval: 0,
                interest_rate: 0,
                // LIVE clearance fee, so the liquidation charges one. It also pins `risk/mod.rs`'s
                // `.min(account.perp_wallet_balance.max(0))`: MK's wallet is EMPTY when the
                // liquidation lands (the fill drained it into the silo), so the fee must come out as
                // ZERO rather than pushing him under water. Under M1′ this pinned the same clamp
                // against an already-NEGATIVE wallet; the `max(0)` half of it is now covered by
                // `a_maker_close_fee_is_the_only_remaining_way_to_a_negative_wallet` plus the unit
                // tests in `types::account`.
                liquidation_fee_rate_bps: 50,
                // ±50%. Wide, and it has to be: the liquidation close below must reach a bid
                // well under the new mark. Note WHY the default (`0` → ±10%) is not enough —
                // `run_update_index_price` threads the `Market` it loaded BEFORE `save_mark_price`
                // into `run_liquidation_sweep`, so the close's fill-time band is centred on the
                // PRE-update mark, not the one that just triggered the liquidation. At ±10% that
                // makes an in-band bid unreachable after any large move and every close lands in
                // ADL (which never touches the insurance fund), so the IF leg would go untested.
                price_band_bps: 5_000,
                mark_price: 0,
                tiers: MarginTiers::default(), // one tier, max leverage 3, mmr 1/6
            },
        )
        .unwrap();
        storage::save_mark_price(ctx, MARKET_ID, ENTRY_PRICE).unwrap();
    }

    /// `deposit` then `transferToPerp` — the only route collateral has into the perp layer.
    fn fund_perp_wallet(ctx: &mut TestCtx, user: Address, amount: u64) {
        run_deposit(
            &depositCall {
                amount: U256::from(amount),
            }
            .abi_encode(),
            user,
            ctx,
        )
        .unwrap();
        run_transfer_to_perp(&transferToPerpCall { amount }.abi_encode(), user, ctx).unwrap();
    }

    /// The protocol's whole liability side, bucket by bucket.
    #[derive(Debug, Default, Clone, Copy, PartialEq)]
    struct Claims {
        /// Σ `UserAccount::usdc_balance` — the spot/withdrawal leg.
        spot: i128,
        /// Σ `UserAccount::perp_wallet_balance`, **SIGNED**. This is where a receivable lives.
        wallet: i128,
        /// Σ `PerpPosition::margin` — collateral allocated to open positions.
        margin: i128,
        /// The insurance fund.
        insurance: i128,
        /// Σ `PerpPosition::v_quote_balance` — the mark-to-market term's cash half.
        v_quote: i128,
        /// Σ `PerpPosition::amount` — NET open interest.
        amount: i128,
    }

    impl Claims {
        fn total_at(&self, mark: u64) -> i128 {
            let v =
                crate::math::calc_value(mark, self.amount.unsigned_abs() as u64, 0, PRICE_DECIMALS)
                    .unwrap() as i128;
            self.spot
                + self.wallet
                + self.margin
                + self.insurance
                + self.v_quote
                + if self.amount >= 0 { v } else { -v }
        }
    }

    /// `clamp_wallets` reads each wallet through `visible_perp_wallet_balance()` instead of the
    /// stored signed value — i.e. exactly what every `uint64` ABI surface reports.
    fn claims(ctx: &mut TestCtx, clamp_wallets: bool) -> Claims {
        let mut c = Claims {
            insurance: storage::load_insurance_fund(ctx).unwrap() as i128,
            ..Claims::default()
        };
        for user in HOLDERS {
            let acc = storage::load_account(ctx, user).unwrap();
            let pos = storage::load_position(ctx, user, MARKET_ID).unwrap();
            let spot: U256 = acc.usdc_balance.clone().into();
            c.spot += spot.to::<u128>() as i128;
            c.wallet += if clamp_wallets {
                acc.visible_perp_wallet_balance() as i128
            } else {
                acc.perp_wallet_balance as i128
            };
            c.margin += pos.margin as i128;
            c.v_quote += pos.v_quote_balance as i128;
            c.amount += pos.amount as i128;
        }
        c
    }

    /// The USDC the precompile really holds, on-trie.
    fn custodied_usdc(ctx: &mut TestCtx) -> i128 {
        storage::load_erc20_balance(ctx, USDC_ADDRESS, PERP_DEX_ADDRESS)
            .unwrap()
            .to::<u128>() as i128
    }

    /// Assert the custody identity at the current mark, and return the buckets.
    fn assert_custody_closes(ctx: &mut TestCtx, expected_custody: i128, at: &str) -> Claims {
        let mark = storage::load_mark_price(ctx, MARKET_ID).unwrap();
        let c = claims(ctx, false);
        let d = custodied_usdc(ctx);
        assert_eq!(
            d, expected_custody,
            "custodied USDC moved unexpectedly at {at}"
        );
        assert_eq!(
            c.amount, 0,
            "net open interest must be flat at {at} (every fill here is two-sided), \
             otherwise the mark-to-market term is not price-independent"
        );
        assert_eq!(
            c.total_at(mark),
            d,
            "CUSTODY BROKEN at {at}: claims {c:?} total {} vs custodied {d} (mark {mark})",
            c.total_at(mark)
        );
        c
    }

    fn update_index(ctx: &mut TestCtx, index: u64, ts: u64) {
        run_update_index_price(
            &updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: index,
                timestamp: ts,
            }
            .abi_encode(),
            ADMIN,
            ctx,
        )
        .unwrap();
    }

    fn mk_realized_pnl(ctx: &mut TestCtx) -> i128 {
        take_position_changes(ctx)
            .into_iter()
            .filter(|e| e.user == MK)
            .map(|e| e.realizedPnl as i128)
            .sum()
    }

    /// A maker fill that the wallet cannot fund opens a position whose silo is **SHORT** (model M1),
    /// and that position is then liquidated INSOLVENT so the Insurance Fund covers the part the silo
    /// could not. Two questions, one scenario:
    ///
    /// 1. **does the custodied USDC still equal the sum of everyone's claims?** (the identity, at
    ///    every checkpoint — the original purpose of this test, unchanged);
    /// 2. **does the shortfall RESOLVE, leaving nothing stranded?** (the structural property that
    ///    motivated switching M1′ → M1).
    ///
    /// # What moved, and why this scenario is still the right one
    ///
    /// This test was written against **M1′**, where the silo was funded IN FULL and MK's
    /// `perp_wallet_balance` went NEGATIVE by the shortfall. It then proved that the negative was a
    /// RECEIVABLE rather than a leak (the identity closed with it in as a negative claim, and it was
    /// CLAMPING it that broke the identity). That was all true — but a receivable that only a
    /// voluntary deposit can clear is a strictly worse structure than one the protocol resolves by
    /// itself, and R11 measured that Binance does the latter (`derived-ooim-plan.md` §3a).
    ///
    /// Under **M1** the same scenario, unchanged call for call, produces the deficit INSIDE the
    /// position instead: the silo receives the $300 of cash actually at hand against a $366.67
    /// requirement, so it is $66.67 short. Nothing else about the fill changes. The consequence
    /// shows up at liquidation, and it is the whole point:
    ///
    /// ```text
    /// M1′  silo 366.67 → close loses 460.00 → IF absorbs  93.33 ,  wallet stranded at −66.67
    /// M1   silo 300.00 → close loses 460.00 → IF absorbs 160.00 ,  wallet at 0
    ///                                             ↑ +66.67 = exactly the stranded balance
    /// ```
    ///
    /// The shortfall travels with the position and is absorbed by the EXISTING
    /// liquidation-deficit → insurance-fund path (`absorb_bad_debt_into_insurance_fund`). No new
    /// mechanism, no new term in the identity: `Σ margin` is $66.67 lower and `Σ wallet` $66.67
    /// higher, and the identity is a sum over both.
    ///
    /// # What the custody identity now covers
    ///
    /// Every checkpoint is kept verbatim, and the identity is EXERCISED THE SAME WAY — it is still
    /// asserted across an underfunded maker fill, an insolvent liquidation and a later deposit, and
    /// it still has to close against real on-trie USDC. What it no longer *contains* is a negative
    /// `wallet` term, because this path no longer produces one. That leg has not been deleted, it has
    /// MOVED: the only remaining producer of a negative `perp_wallet_balance` is a maker fee the
    /// capped opening margin could not absorb (chiefly a pure close whose proceeds an insolvent close
    /// ate), which is commission-sized and is the one place Binance's own wallet goes negative too.
    /// It is pinned by `trading::tests::a_maker_close_fee_is_the_only_remaining_way_to_a_negative_wallet`,
    /// and the signed field + clamped ABI view are kept for it.
    ///
    /// # ⚠️ Still deliberately hostile to "absorb the shortfall from the Insurance Fund" at FILL time
    ///
    /// Crediting a fill-time shortfall out of the IF keeps the SUM unchanged (IF down, wallet or silo
    /// up), so the identity alone would not notice. It remains wrong: it socialises a loss that has
    /// not happened yet — the position may well close in profit — and it double-pays, because the IF
    /// already absorbs the beyond-silo slice at liquidation. The pinned split at the end (own cash +
    /// IF == the whole realized loss, with the IF term equal to the beyond-silo slice and nothing
    /// more) fails if anyone wires that up. That is on purpose.
    #[test]
    fn custodied_usdc_still_equals_all_claims_when_a_short_silo_position_is_liquidated_insolvent() {
        let mut ctx = make_ctx_with_usdc(&[
            (MK, MK_DEPOSIT + MK_TOP_UP),
            (T1, T1_DEPOSIT),
            (T2, T2_DEPOSIT),
            (LQ, LQ_DEPOSIT),
            (ADMIN, IF_SEED),
        ]);
        setup_market_no_funding(&mut ctx);

        // ── 1. All collateral enters through `deposit`, so custody is a MEASURED number ──
        for (user, amount) in [
            (MK, MK_DEPOSIT),
            (T1, T1_DEPOSIT),
            (T2, T2_DEPOSIT),
            (LQ, LQ_DEPOSIT),
            (ADMIN, IF_SEED),
        ] {
            fund_perp_wallet(&mut ctx, user, amount);
        }
        // The insurance fund is NOT written directly (that would be a mint): it is seeded out of
        // the admin's own perp wallet, which is where `run_deposit_insurance_fund` takes it from.
        run_deposit_insurance_fund(
            &depositInsuranceFundCall { amount: IF_SEED }.abi_encode(),
            ADMIN,
            &mut ctx,
        )
        .unwrap();
        let custody_before_top_up = TOTAL_DEPOSITED - MK_TOP_UP as i128;
        assert_custody_closes(&mut ctx, custody_before_top_up, "after funding");

        // ── 2. MK opens a SHORT 10 @ $100 at leverage 3, as the maker ──
        run_set_leverage(
            &setLeverageCall {
                marketId: MARKET_ID,
                leverage: 3,
            }
            .abi_encode(),
            MK,
            &mut ctx,
        )
        .unwrap();
        place_order(&mut ctx, MK, SELL, ENTRY_PRICE, QTY as u64);
        place_order(&mut ctx, T1, BUY, ENTRY_PRICE, QTY as u64);
        let mk_short = storage::load_position(&mut ctx, MK, MARKET_ID).unwrap();
        assert_eq!((mk_short.amount, mk_short.margin), (-QTY, 333_333_333));
        assert_eq!(
            storage::load_account(&mut ctx, MK)
                .unwrap()
                .perp_wallet_balance,
            66_666_667,
            "MK has spent all but $66.67 of its deposit on the short's own margin"
        );
        assert_custody_closes(&mut ctx, custody_before_top_up, "after MK's short opens");

        // ── 3. Mark to $110 — MK's short is underwater but still above maintenance ──
        update_index(&mut ctx, 11_000, 31);
        let mark = storage::load_mark_price(&mut ctx, MARKET_ID).unwrap();
        assert_eq!(
            mark, 11_000,
            "median(price1, price2, lastTraded) must land on the index"
        );
        assert_custody_closes(
            &mut ctx,
            custody_before_top_up,
            "after the mark rises to $110",
        );

        // ── 4. The FLIP: a resting buy of 2× the short costs ooIM = 0, and fills for $366.67 ──
        // This is the production shape of an underfunded maker fill, and it needs no hand-written
        // state at all. `ooIM = IM − PIM` nets the close a fill would perform, so at
        // `Bid ≈ 2|N|` the two branches of the joint `max()` tie and the order is admitted FREE
        // (`trading/mod.rs`'s buy arm even documents this band: unlike a sell, a resting BUY gets
        // no Assuming-Price uplift, so for us the `ooIM = 0` band is both free AND fillable).
        // The fill then closes the short at a LOSS — so the margin it releases is less than the
        // margin the new long leg needs — and the shortfall stays in the SILO (M1).
        assert_eq!(
            oo_im(&mut ctx, MK),
            0,
            "the flipping buy must be admitted free"
        );
        place_order(&mut ctx, MK, BUY, 11_000, 2 * QTY as u64);
        assert_eq!(oo_im(&mut ctx, MK), 0);
        place_order(&mut ctx, T2, SELL, 11_000, 2 * QTY as u64);

        // Cash at hand for the opening leg = wallet 66_666_667 + released margin 333_333_333
        //   + realized −100_000_000 (short 10 @ $100 closed at $110) = 300_000_000,
        // against a requirement of 1_100_000_000 / 3 = 366_666_666. So
        //   opening_margin = min(366_666_666, 300_000_000) = 300_000_000   ⇒ silo SHORT 66_666_666
        //   wallet         = 300_000_000 − 300_000_000     = 0             ⇒ NOT negative
        const SILO_SHORTFALL: i64 = 66_666_666;
        let mk_long = storage::load_position(&mut ctx, MK, MARKET_ID).unwrap();
        assert_eq!(
            (mk_long.amount, mk_long.v_quote_balance, mk_long.margin),
            (QTY, -1_100_000_000, 300_000_000),
            "the flip filled: short closed, long 10 @ $110 opened, silo funded to the CASH AT \
             HAND — short of its 366_666_666 requirement (M1, not M1′)"
        );
        assert_eq!(
            366_666_666 - mk_long.margin,
            SILO_SHORTFALL,
            "the shortfall is exactly the cash the wallet could not produce"
        );
        assert_eq!(
            storage::load_account(&mut ctx, MK)
                .unwrap()
                .perp_wallet_balance,
            0,
            "the wallet is emptied into the silo and stops there — M1′ read −66_666_666 here"
        );
        // K9 passed on the SHORT silo, which is the honest test: equity at the $110 mark is
        // `300_000_000 − 1_100_000_000 + 1_100_000_000` against a `1_100_000_000 / 6` maintenance
        // requirement. Had the fill been priced against a full silo it would have looked safer than
        // it is — that optimism is what M1 removes.
        let mkt_tiers = storage::load_market(&mut ctx, MARKET_ID)
            .unwrap()
            .unwrap()
            .tiers;
        assert!(
            crate::math::is_above_maintenance_margin(
                &mkt_tiers,
                11_000,
                mk_long.amount,
                mk_long.v_quote_balance,
                mk_long.margin,
                0,
                PRICE_DECIMALS,
            )
            .unwrap(),
            "the short-silo position is solvent at the mark it opened at"
        );
        assert_custody_closes(
            &mut ctx,
            custody_before_top_up,
            "after the short-silo flip fill",
        );

        // ── 5. A punitive resting bid at $64, then crash the mark to $70 ──
        // MK's long is insolvent below $73.33 (its bankruptcy price), so a forced close into a $64
        // bid realizes a loss the margin cannot cover and the Insurance Fund pays the remainder.
        // Closing through the BOOK is what reaches the fund at all — a residual the book cannot
        // absorb goes to ADL instead, which never touches it. $64 is chosen to sit INSIDE the
        // close's fill-time price band (see `price_band_bps` in the market fixture above): a bid
        // outside it is skipped and the close would land in ADL.
        place_order(&mut ctx, LQ, BUY, 6_400, QTY as u64);
        assert_custody_closes(&mut ctx, custody_before_top_up, "after LQ's bid rests");

        let if_before_liq = storage::load_insurance_fund(&mut ctx).unwrap();
        let _ = mk_realized_pnl(&mut ctx); // drain: only the liquidation's own PnL is read below
        update_index(&mut ctx, 7_000, 60);
        assert_eq!(
            storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(),
            7_000
        );

        let mk_after = storage::load_position(&mut ctx, MK, MARKET_ID).unwrap();
        assert_eq!(
            (mk_after.amount, mk_after.margin, mk_after.v_quote_balance),
            (0, 0, 0),
            "the sweep liquidated MK's insolvent long through LQ's bid"
        );
        let liq_realized = mk_realized_pnl(&mut ctx);
        assert_eq!(
            liq_realized, -460_000_000,
            "closed 10 @ $110 entry into a $64 bid"
        );
        let if_after_liq = storage::load_insurance_fund(&mut ctx).unwrap();
        let if_absorbed = (if_before_liq - if_after_liq) as i128;
        // ── THE STRUCTURAL PROPERTY M1 BUYS ──
        // The IF absorbs the loss beyond the silo, and the silo is SHORT, so the fill-time
        // shortfall is absorbed here too — through the pre-existing bad-debt path, with no new
        // mechanism. $460 loss − $300 silo = $160, which is the $93.33 M1′ would have routed PLUS
        // the $66.67 M1′ would have stranded on the wallet forever.
        assert_eq!(
            if_absorbed, 160_000_000,
            "the IF covers the loss BEYOND the silo — and the silo was short, so the fill-time \
             shortfall resolves here"
        );
        assert_eq!(
            if_absorbed - 93_333_334,
            SILO_SHORTFALL as i128,
            "and the extra the IF takes vs the full-silo case IS the shortfall, to the unit"
        );
        assert!(
            if_after_liq > 0,
            "no socialised write-off: the identity must stay comparable"
        );

        // MK ends with NOTHING STRANDED: no position, no silo, and a wallet at exactly zero. The
        // wallet is untouched by the liquidation itself — isolated margin never debits it, and the
        // clearance fee is capped at `perp_wallet_balance.max(0)` = 0 (risk/mod.rs). This is the
        // assertion M1′ could not make: there it ended at −66_666_666, with the position that
        // justified the debit gone and nothing in the engine able to clear it.
        assert_eq!(
            storage::load_account(&mut ctx, MK)
                .unwrap()
                .perp_wallet_balance,
            0,
            "no stranded balance: the deficit left with the position it was attached to"
        );

        // ── 6. THE VERDICT: custody still equals the sum of all claims ──
        let c = assert_custody_closes(
            &mut ctx,
            custody_before_top_up,
            "after the insolvent liquidation",
        );

        // ...and, unlike under M1′, the CLAMPED view closes too. Under M1′ this assertion measured
        // the identity being broken by exactly the deficit (`uint64` ABI surfaces read the negative
        // wallet as 0, over-stating claims): that was the mechanical proof the negative term was
        // load-bearing. Under M1 there is no negative term left on this path, so the ledger and
        // everything `getAccount` / `AccountBalanceChanged` report AGREE — which is the stronger
        // statement and the one worth pinning: an operator reading only the clamped ABI can no
        // longer be misled about this scenario. Reintroducing M1′ makes this fail.
        let clamped = claims(&mut ctx, true);
        assert_eq!(
            clamped.total_at(7_000) - custody_before_top_up,
            0,
            "no clamped-vs-stored gap: nothing on this path drives a wallet negative any more"
        );
        assert_eq!(
            clamped, c,
            "and that is because the two views are now identical, not because two errors cancel"
        );

        // ── 7. NO DOUBLE COUNT — the loss splits into two DISJOINT slices ──
        // MK's whole realized loss over both fills, funded by: MK's own deposited cash, and the
        // Insurance Fund. Under M1′ this was a THREE-way split — own cash + receivable + IF — and
        // the receivable term has not vanished, it has MERGED into the IF term (that is the
        // `if_absorbed − 93_333_334 == SILO_SHORTFALL` assertion above). If the IF ever absorbed the
        // shortfall a SECOND time (e.g. someone also "resolves" it at fill time) this over-shoots by
        // the overlap. It is exact, so there is no overlap. (Fees are 0 and funding is off in this
        // market, so realized PnL is MK's entire P&L.)
        let mk_total_realized = liq_realized + -100_000_000; // flip close: short @ $100 → $110
        assert_eq!(
            MK_DEPOSIT as i128 + if_absorbed,
            -mk_total_realized,
            "MK's loss must decompose EXACTLY into own-cash + IF, with a ZERO receivable term"
        );
        assert!(
            if_absorbed < -mk_total_realized,
            "the IF must cover strictly less than the whole loss"
        );
        assert_eq!(
            c.margin,
            claims(&mut ctx, false).margin,
            "sanity: `claims` is a pure read"
        );

        // ── 8. A user at zero is refused money-out; a user who tops up is NOT working off a debt ──
        // Under M1′ this section proved receivable semantics: money-out refused because `available`
        // was negative, and the next deposit swallowed by the deficit. Under M1 the first half still
        // holds (MK is at exactly 0, and `derived_can_afford` refuses every POSITIVE requirement),
        // while the second half INVERTS — and that inversion is the user-visible payoff of the
        // switch: MK's $20 is his to spend, because the shortfall was settled against the insurance
        // fund when the position it belonged to died.
        for (label, r) in [
            (
                "transferFromPerp",
                run_transfer_from_perp(
                    &transferFromPerpCall { amount: 1 }.abi_encode(),
                    MK,
                    &mut ctx,
                ),
            ),
            (
                "withdraw",
                run_withdraw(
                    &withdrawCall {
                        amount: U256::from(1u64),
                    }
                    .abi_encode(),
                    MK,
                    &mut ctx,
                ),
            ),
        ] {
            assert!(
                r.is_err(),
                "{label} must refuse a user whose perp wallet has nothing available"
            );
        }

        fund_perp_wallet(&mut ctx, MK, MK_TOP_UP);
        assert_eq!(
            storage::load_account(&mut ctx, MK)
                .unwrap()
                .perp_wallet_balance,
            MK_TOP_UP as i64,
            "the deposit lands in full — there is no deficit left for it to net against"
        );
        assert!(
            run_transfer_from_perp(
                &transferFromPerpCall { amount: 1 }.abi_encode(),
                MK,
                &mut ctx,
            )
            .is_ok(),
            "and it is SPENDABLE: under M1′ MK would still be under water here, refused"
        );
        assert_custody_closes(&mut ctx, TOTAL_DEPOSITED, "after MK tops up");

        // ── 9. The two ABI views AGREE, and both are signed ──
        // Both account views are unclamped now: `getAccount` returns the roll-up over the per-user
        // market index, `getAccountMargin` the same arithmetic over a caller list. Under M1′ the
        // clamp on the old `getAccount().availablePerpBalance` was the whole difference between "we
        // carry a receivable" and "we carry an INVISIBLE receivable" — an operator watching only the
        // event stream could not see a deficit accumulate. Under M1 there is nothing to hide on this
        // path, so this asserts the two views land on the same number from the two different market
        // sets (the index holds MARKET_ID, which is what the list below passes).
        let expected = MK_TOP_UP as i64 - 1; // the 1 unit transferred out just above
        let index_view = getAccountCall::abi_decode_returns(
            &run_get_account(&getAccountCall { user: MK }.abi_encode(), &mut ctx).unwrap(),
        )
        .unwrap();
        assert_eq!(
            (
                index_view.totalCrossWalletBalance,
                index_view.availableBalance
            ),
            (expected, expected),
            "getAccount reports the full available balance — nothing is being floored away"
        );
        let signed_view = getAccountMarginCall::abi_decode_returns(
            &run_get_account_margin(
                &getAccountMarginCall {
                    user: MK,
                    marketIds: vec![MARKET_ID],
                }
                .abi_encode(),
                &mut ctx,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            (
                signed_view.totalCrossWalletBalance,
                signed_view.availableBalance
            ),
            (expected, expected),
            "getAccountMargin agrees with the index-driven view (no resting orders, so ooIM = 0)"
        );
    }
}

// ── Out-of-band resting-order expiry (band GC) ─────────────────────────────────
//
// `match_order` bands FILLS, not placements, so a mark move can leave a level sitting AT THE TOUCH
// and unmatchable forever. `run_out_of_band_expiry_sweep` (inside `updateIndexPrice`, where the band
// moves) collects the NEAR-SIDE prefix of such levels — asks below the lower edge, bids above the
// upper edge — and leaves the FAR ends alone, because those are the ordinary deep passive orders the
// fill-time band is written to let rest.
mod band_expiry {
    use super::*;
    use crate::interface::IPerpDex::OrderCancelled;
    use crate::types::CancelReason;

    fn oracle_update(ctx: &mut TestCtx, index: u64, ts: u64) {
        run_update_index_price(
            &updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: index,
                timestamp: ts,
            }
            .abi_encode(),
            ADMIN,
            ctx,
        )
        .unwrap();
    }

    /// The band tests rest dozens of orders, and one unit of this market is $100 of notional, so
    /// `setup_market`'s $50 / $2 000 wallets do not stretch. Fund generously — none of these tests
    /// is about the affordability gate.
    fn fund_big(ctx: &mut TestCtx, user: Address) {
        storage::save_account(
            ctx,
            user,
            UserAccount {
                perp_wallet_balance: 100_000_000_000_000, // $100M
                ..UserAccount::default()
            },
            AccountUpdateReason::Adjustment,
        )
        .unwrap();
    }

    fn asks(ctx: &mut TestCtx) -> Vec<u64> {
        storage::load_ask_prices(ctx, MARKET_ID).unwrap()
    }

    fn bids(ctx: &mut TestCtx) -> Vec<u64> {
        storage::load_bid_prices(ctx, MARKET_ID).unwrap()
    }

    fn cancel_reasons(ctx: &mut TestCtx) -> Vec<u8> {
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

    fn post_only(
        ctx: &mut TestCtx,
        user: Address,
        side: u8,
        price: u64,
    ) -> Result<Bytes, PerpError> {
        run_place_order(
            &placeOrderCall {
                marketId: MARKET_ID,
                side,
                price,
                quantity: 1,
                orderType: 0, // Limit
                tif: 3,       // PostOnly
                clientOrderId: FixedBytes::default(),
            }
            .abi_encode(),
            user,
            ctx,
        )
    }

    /// An **IOC** taker. Used where the fixture needs a crossing order that provably reaches an
    /// out-of-band level but must not REST: crossing a level past the far band edge requires a limit
    /// past that edge too, and a resting order that would become a too-good best is refused at
    /// placement. IOC drops the remainder, so the only thing under test is the fill-time band.
    fn ioc(ctx: &mut TestCtx, user: Address, side: u8, price: u64) -> Result<Bytes, PerpError> {
        run_place_order(
            &placeOrderCall {
                marketId: MARKET_ID,
                side,
                price,
                quantity: 1,
                orderType: 0, // Limit
                tif: 1,       // IOC
                clientOrderId: FixedBytes::default(),
            }
            .abi_encode(),
            user,
            ctx,
        )
    }

    /// PRECONDITION helper: `price` really is outside the band at the CURRENT mark, and really is
    /// the cached best on `side` — the two things every fixture below silently depends on. Read from
    /// stored state, never recomputed from a comment, so it cannot drift away from the market the
    /// test actually runs against.
    fn assert_out_of_band_best(ctx: &mut TestCtx, side: Side, price: u64) {
        let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
        let (upper, lower) =
            crate::math::mark_band_bounds(market.mark_price, market.price_band_bps);
        let p = price as u128;
        assert!(
            p < lower || p > upper,
            "precondition: {price} must be OUTSIDE the band [{lower}, {upper}] at mark {}",
            market.mark_price
        );
        let best = match side {
            Side::Buy => storage::load_best_bid(ctx, MARKET_ID),
            Side::Sell => storage::load_best_ask(ctx, MARKET_ID),
        }
        .unwrap();
        assert_eq!(
            best, price,
            "precondition: the out-of-band level must be the cached best — that is the entire \
             reason it is harmful, and if it is not, every assertion below passes vacuously"
        );
    }

    /// Move the mark WITHOUT running `updateIndexPrice`, i.e. without the band-expiry sweep.
    ///
    /// This is the only way to set up a stranded book now that placement refuses a too-good new
    /// best: the level must be placed while the band contains it and then be stranded by a mark
    /// move. Going through the oracle instead would run the very GC these fixtures are trying to
    /// observe (or to prove has NOT yet run).
    fn drift_mark(ctx: &mut TestCtx, mark: u64) {
        storage::save_mark_price(ctx, MARKET_ID, mark).unwrap();
    }

    /// A mark JUMP UP strands the whole cheap end of the ask book below the new lower edge. Those
    /// levels are the touch and can never fill, so they go; the level that is still in band, and the
    /// far-side level ABOVE the new upper edge, both survive.
    #[test]
    fn a_mark_jump_expires_the_stranded_ask_prefix_and_spares_the_far_side() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        // In band at the OLD mark ($100 ⇒ [$90, $110]) …
        fund_big(&mut ctx, MAKER);
        place_order(&mut ctx, MAKER, 1, 9_500, 1);
        place_order(&mut ctx, MAKER, 1, 9_600, 1);
        // … and two that are out of band already, ABOVE the upper edge. Placement allows these: an
        // ask above the band is a FAR-side quote, which is exactly the deep passive liquidity the
        // one-sided placement reject deliberately keeps
        // (`trading::tests::a_far_side_out_of_band_quote_still_rests_even_as_the_new_best`). Note
        // they never become the best ask either — 9_500 is already in front of them.
        place_order(&mut ctx, MAKER, 1, 14_000, 1);
        place_order(&mut ctx, MAKER, 1, 17_000, 1);
        assert_eq!(asks(&mut ctx), vec![9_500, 9_600, 14_000, 17_000]);

        // Mark $100 → $150 ⇒ band [$135, $165]. 9_500/9_600 are now the stranded near-side prefix;
        // 14_000 is in band; 17_000 is a deep passive order on the far side.
        oracle_update(&mut ctx, 15_000, 31);
        assert_eq!(
            storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(),
            15_000
        );

        assert_eq!(
            asks(&mut ctx),
            vec![14_000, 17_000],
            "the contiguous below-band prefix is expired; the in-band level and the far-side deep \
             ask both survive"
        );
        let best_ask = storage::load_best_ask(&mut ctx, MARKET_ID).unwrap();
        assert_eq!(best_ask, 14_000, "the BBO cache follows the removals");
        let (upper, lower) = crate::math::mark_band_bounds(15_000, 0);
        assert!(
            (best_ask as u128) >= lower && (best_ask as u128) <= upper,
            "the post-GC best ask is IN band: {best_ask} vs [{lower}, {upper}]"
        );
    }

    /// The bid mirror: bids descend, so the stranded prefix is the levels ABOVE the upper edge, and
    /// the deep bid BELOW the lower edge is the one that must be left alone.
    #[test]
    fn a_mark_drop_expires_the_stranded_bid_prefix_and_spares_the_far_side() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        fund_big(&mut ctx, MAKER);
        // The two TOP bids are the ones this test needs stranded, and a stranded bid is only
        // reachable by drift: a bid above the upper edge is too-good, and bids rank better-first, so
        // it is necessarily the best bid — which placement refuses. So the whole book is built at a
        // mark that CONTAINS the top bids ($120 ⇒ [$108, $132]) and the drop below strands them.
        drift_mark(&mut ctx, 12_000);
        place_order(&mut ctx, MAKER, 0, 5_000, 1); // far side after the drop
        place_order(&mut ctx, MAKER, 0, 6_000, 1); // in band after the drop
        place_order(&mut ctx, MAKER, 0, 11_500, 1);
        place_order(&mut ctx, MAKER, 0, 12_000, 1);
        assert_eq!(bids(&mut ctx), vec![5_000, 6_000, 11_500, 12_000]);
        let (upper_at_placement, _) = crate::math::mark_band_bounds(12_000, 0);
        assert!(
            12_000u128 <= upper_at_placement,
            "precondition: the top bid really was INSIDE the band when it was placed"
        );

        // Mark $120 → $60 ⇒ band [$54, $66].
        oracle_update(&mut ctx, 6_000, 31);
        assert_eq!(
            storage::load_mark_price(&mut ctx, MARKET_ID).unwrap(),
            6_000
        );
        // The three roles this fixture asserts must actually be occupied, or "the far side is
        // spared" is a claim about nothing.
        let (upper_new, lower_new) = crate::math::mark_band_bounds(6_000, 0);
        for doomed in [11_500u128, 12_000] {
            assert!(
                doomed > upper_new,
                "precondition: removed bid {doomed} really is in the ABOVE-band prefix vs \
                 [{lower_new}, {upper_new}]"
            );
        }
        assert!(
            5_000u128 < lower_new,
            "precondition: the spared bid really is on the FAR side (below the lower edge), not \
             merely in band"
        );
        assert!(
            6_000u128 >= lower_new && 6_000u128 <= upper_new,
            "precondition: and the survivor at the touch really is in band"
        );

        assert_eq!(
            bids(&mut ctx),
            vec![5_000, 6_000],
            "the contiguous above-band prefix is expired; the in-band bid and the deep passive bid \
             below the band both survive"
        );
        let best_bid = storage::load_best_bid(&mut ctx, MARKET_ID).unwrap();
        assert_eq!(best_bid, 6_000);
        let (upper, lower) = crate::math::mark_band_bounds(6_000, 0);
        assert!(
            (best_bid as u128) >= lower && (best_bid as u128) <= upper,
            "the post-GC best bid is IN band: {best_bid} vs [{lower}, {upper}]"
        );
    }

    /// The cap is on ORDERS, not levels: 70 orders at ONE price take two updates to clear, 64 then
    /// 6. Between them the level keeps resting — deferral, not loss.
    #[test]
    fn the_cap_is_on_orders_and_a_second_update_finishes_the_job() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        fund_big(&mut ctx, MAKER);
        let total = MAX_BAND_EXPIRIES_PER_UPDATE + 6;
        for _ in 0..total {
            place_order(&mut ctx, MAKER, 1, 9_500, 1);
        }
        assert_eq!(
            storage::load_ask_count(&mut ctx, MARKET_ID, 9_500).unwrap(),
            total as u64
        );

        oracle_update(&mut ctx, 15_000, 31);
        assert_eq!(
            storage::load_ask_count(&mut ctx, MARKET_ID, 9_500).unwrap(),
            6,
            "exactly MAX_BAND_EXPIRIES_PER_UPDATE orders expired in the first update"
        );
        assert_eq!(
            asks(&mut ctx),
            vec![9_500],
            "the level is not empty yet, so its price is still in the index"
        );

        oracle_update(&mut ctx, 15_000, 61);
        assert_eq!(
            storage::load_ask_count(&mut ctx, MARKET_ID, 9_500).unwrap(),
            0
        );
        assert!(
            asks(&mut ctx).is_empty(),
            "the second update drains the remainder and drops the price"
        );
    }

    /// The expiry is ATTRIBUTED: `OrderCancelled.reason` separates a protocol kill from the owner's
    /// own cancel, which the event could not do when it was three indexed fields and no data.
    #[test]
    fn the_expiry_event_carries_the_gc_reason_and_a_user_cancel_carries_its_own() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        fund_big(&mut ctx, MAKER);
        place_order(&mut ctx, MAKER, 1, 9_500, 1);
        let keep = try_place_order(&mut ctx, MAKER, 1, 14_000, 1).unwrap();
        let keep: [u8; 32] = keep[..32].try_into().unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());

        oracle_update(&mut ctx, 15_000, 31);
        assert_eq!(
            cancel_reasons(&mut ctx),
            vec![CancelReason::PriceBandExpiry as u8],
            "the stranded ask is expired by the protocol, and says so"
        );

        crate::trading::run_cancel_order(
            &crate::interface::IPerpDex::cancelOrderCall {
                orderId: FixedBytes(keep),
                marketId: MARKET_ID,
            }
            .abi_encode(),
            MAKER,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(
            cancel_reasons(&mut ctx),
            vec![CancelReason::User as u8],
            "the owner's own cancel is distinguishable from the kill above"
        );
    }

    /// The aggregates shrink by the entry's FROZEN `assuming_price`, never by the limit price and
    /// never by a re-derivation at the current mark. A marked-up sell makes the three disagree: the
    /// limit basis would strand the markup in `Ask` forever, and a live re-derivation at the jumped
    /// mark would OVERSHOOT and trip the underflow invariant.
    #[test]
    fn expiry_shrinks_the_aggregates_by_the_frozen_assuming_price() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        fund_big(&mut ctx, MAKER);
        // Sell limit $95 with the mark at $100: Assuming Price = max(lastTraded×1.0015, mark, limit)
        // = 10_000 ⇒ notional 100, where the limit basis would say 95.
        place_order(&mut ctx, MAKER, 1, 9_500, 1);
        let pos = position(&mut ctx, MAKER);
        assert_eq!(
            (pos.total_sell_qty, pos.total_sell_notional),
            (1, 100_000_000),
            "Ask is priced at the Assuming Price ($100.00), not the limit price ($95.00)"
        );

        let before = conservation_sum(&mut ctx, &[MAKER]);
        oracle_update(&mut ctx, 15_000, 31);

        let pos = position(&mut ctx, MAKER);
        assert_eq!(
            (pos.total_sell_qty, pos.total_sell_notional),
            (0, 0),
            "the aggregate returns EXACTLY to zero — the frozen basis, not the limit price \
             (would leave 5_000_000) and not the new mark (would underflow)"
        );
        assert_eq!(
            conservation_sum(&mut ctx, &[MAKER]),
            before,
            "expiry moves no money: there is no escrow to refund"
        );
    }

    /// Harm #1, and its fix. `ensure_post_only_does_not_cross` reads the cached best and knows
    /// nothing about the band, so an unmatchable bid ABOVE the upper edge makes it reject a
    /// legitimate PostOnly sell as "would match" against a level that can never fill.
    #[test]
    fn a_post_only_sell_mis_rejected_by_an_unmatchable_bid_is_accepted_after_the_gc() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        fund_big(&mut ctx, MAKER);
        fund_big(&mut ctx, ALICE);
        // A bid at $120 against an empty ask side, placed while the band contains it ($120 mark ⇒
        // [$108, $132]) and then stranded by a mark DRIFT back to $100 ⇒ [$90, $110]. Drift is the
        // only route: at the $100 mark this bid is a too-good new best and placement refuses it.
        // `drift_mark`, not the oracle, because an oracle update runs the GC — and the whole point
        // here is to observe the state BEFORE the GC collects it.
        drift_mark(&mut ctx, 12_000);
        place_order(&mut ctx, MAKER, 0, 12_000, 1);
        drift_mark(&mut ctx, 10_000);
        // It is out of band and it is nonetheless `best_bid` — the two properties that make the
        // PostOnly mis-reject below possible at all.
        assert_out_of_band_best(&mut ctx, Side::Buy, 12_000);

        // Proof the level is unmatchable: a crossing taker sell fills nothing. IOC, because a
        // resting sell that crossed $120 would have to be below the lower edge and would be refused
        // as a too-good best — which would prove nothing about the fill-time band.
        let taker = ioc(&mut ctx, ALICE, 1, 100).unwrap();
        let taker: [u8; 32] = taker[..32].try_into().unwrap();
        assert_eq!(
            position(&mut ctx, ALICE).amount,
            0,
            "the fill-time band already refuses this level"
        );
        assert!(
            storage::load_order(&mut ctx, &taker).unwrap().is_none(),
            "the IOC expired its whole remainder (delete-on-terminal), so it rested nothing and \
             left the book below untouched"
        );
        assert_eq!(
            bids(&mut ctx),
            vec![12_000],
            "and the unmatchable bid is still there, unfilled"
        );

        let err = post_only(&mut ctx, ALICE, 1, 11_500).unwrap_err();
        assert!(
            err.to_string().contains("PostOnly order would match"),
            "the pre-GC mis-reject: {err}"
        );

        // The mark update collects it (the band centre does not even have to move — the trigger is
        // the update, which is where a stranded level becomes reachable).
        oracle_update(&mut ctx, 10_000, 31);
        assert!(
            bids(&mut ctx).is_empty(),
            "the unmatchable bid is gone: {:?}",
            bids(&mut ctx)
        );

        post_only(&mut ctx, ALICE, 1, 11_500).expect("the same PostOnly sell is now accepted");
        assert_eq!(bids(&mut ctx).len(), 0);
        assert_eq!(asks(&mut ctx), vec![11_500]);
    }

    /// A market with the band DISABLED must GC nothing — `mark_band_bounds` collapses the lower edge
    /// to 0 and blows the upper edge past any price, so neither prefix exists. (This is what keeps
    /// the benches and the golden commitment scenario, both of which disable the band, untouched.)
    #[test]
    fn a_disabled_band_expires_nothing() {
        // Each side needs its own book: with the band disabled a bid above an ask simply TRADES, so
        // the two cases cannot share one context.
        fn disable_band(ctx: &mut TestCtx) {
            let market = storage::load_market(ctx, MARKET_ID).unwrap().unwrap();
            storage::save_market(
                ctx,
                &Market {
                    price_band_bps: 1_000_000,
                    ..market
                },
            )
            .unwrap();
        }

        // Ask below what would be the lower edge of an active band at the new mark.
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        fund_big(&mut ctx, MAKER);
        disable_band(&mut ctx);
        place_order(&mut ctx, MAKER, 1, 9_500, 1);
        oracle_update(&mut ctx, 15_000, 31);
        assert_eq!(
            asks(&mut ctx),
            vec![9_500],
            "an active ±10% band at mark 15_000 would have expired this; a disabled one must not"
        );

        // Bid above what would be the upper edge.
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        fund_big(&mut ctx, MAKER);
        disable_band(&mut ctx);
        place_order(&mut ctx, MAKER, 0, 12_000, 1);
        oracle_update(&mut ctx, 6_000, 31);
        assert_eq!(bids(&mut ctx), vec![12_000]);
    }

    /// Harm #2, executable: an unmatchable level at the touch reaches the MARK's own inputs. Every
    /// best-quote change records a mid-price sample into the price-basis window, with no knowledge of
    /// the band, and that window is `moving_average_basis` → `price2` → the `median(price1, price2,
    /// contract)` that IS the mark. So a quote that can never trade steers the mark.
    #[test]
    fn an_unmatchable_quote_at_the_touch_reaches_the_mark_input_window() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        fund_big(&mut ctx, MAKER);
        assert_eq!(
            storage::load_price_basis_window(&mut ctx, MARKET_ID)
                .unwrap()
                .count,
            0
        );

        // $120 placed while the band contains it ($120 mark), then stranded by a drift back to the
        // $100 mark ⇒ [$90, $110]. Its own placement sample is recorded on the way in.
        drift_mark(&mut ctx, 12_000);
        place_order(&mut ctx, MAKER, 0, 12_000, 1);
        let w = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
        assert_eq!(
            (w.count, w.last_mid_price),
            (1, 12_000),
            "the quote entered the mark-input window at placement"
        );
        drift_mark(&mut ctx, 10_000);
        assert_out_of_band_best(&mut ctx, Side::Buy, 12_000);

        // THE POLLUTION, live and post-drift: the level can no longer trade, and it is STILL a mark
        // input. The next best-quote change on the other side samples the MID, and the mid is
        // dragged up by the unmatchable bid — 11_250 instead of the 10_500 the only tradeable quote
        // in the book would give on its own. (A fresh block timestamp is required: `record_
        // observation` takes at most one sample per timestamp, so the placement sample above would
        // otherwise swallow this one.)
        ctx.block.timestamp = U256::from(2);
        place_order(&mut ctx, MAKER, 1, 10_500, 1);
        let w = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
        assert_eq!(
            (w.count, w.last_mid_price),
            (2, 11_250),
            "an unmatchable level still steers the mark: mid = (12_000 + 10_500)/2, where the \
             tradeable side alone says 10_500"
        );

        // The GC takes the level out, so it can contribute no further samples.
        oracle_update(&mut ctx, 10_000, 31);
        assert!(bids(&mut ctx).is_empty());
        assert_eq!(storage::load_best_bid(&mut ctx, MARKET_ID).unwrap(), 0);
        assert_eq!(
            asks(&mut ctx),
            vec![10_500],
            "and only the unmatchable side went: the in-band ask is untouched"
        );
        assert_eq!(
            storage::load_price_basis_window(&mut ctx, MARKET_ID)
                .unwrap()
                .count,
            2,
            "the removal itself adds no sample (same block timestamp as the sample above)"
        );
    }

    /// A mass expiry across many users moves **NO MONEY AT ALL** — not just "conserves value".
    /// There is no escrow behind a resting order, so expiry only shrinks `Bid`/`Ask`; every wallet
    /// is byte-identical afterwards.
    ///
    /// And it publishes **NO `AccountBalanceChanged`**, which is worth pinning because it is a real
    /// consequence, not an oversight: shrinking `Bid`/`Ask` RAISES each of these users'
    /// `availableBalance` (ooIM is derived from those aggregates), yet the removal is an
    /// aggregates-only write, and the snapshot trigger deliberately does not mark those — see the
    /// table on `storage::mark_account_snapshot_dirty`, whose rule is "no wallet moved and no
    /// position state moved" and whose citation is Binance's measured "cancelled orders will not
    /// make the event ACCOUNT_UPDATE pushed". The trigger is consistent here, and it is exactly why
    /// `OrderCancelled.reason` had to be part of this change: `reason != 0` is the only signal a
    /// stream consumer gets that its cached `availableBalance` needs a `getAccount` re-read.
    #[test]
    fn a_mass_expiry_moves_no_money_and_publishes_no_account_snapshot() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let users = [ALICE, MAKER, KEEPER];
        for u in users {
            fund_big(&mut ctx, u);
        }
        // Both sides, several levels each, several orders per level, three owners interleaved so no
        // per-user grouping is implied by the input order.
        for (n, u) in users.iter().enumerate() {
            for k in 0..4u64 {
                place_order(&mut ctx, *u, 1, 9_100 + k * 100, 1); // asks, stranded by the jump
                place_order(&mut ctx, *u, 0, 9_000 - k * 100, 1); // bids, far side after the jump
            }
            assert!(n < 3);
        }
        let wallets_before: Vec<i64> = users
            .iter()
            .map(|u| {
                storage::load_account(&mut ctx, *u)
                    .unwrap()
                    .perp_wallet_balance
            })
            .collect();
        let value_before = conservation_sum(&mut ctx, &users);
        let _ = JournalTr::take_logs(ctx.journal_mut());
        // These tests drive handlers directly, so nothing has drained the snapshot marks that the
        // `fund_big` account writes left behind. The shell does this at the head of every call.
        storage::begin_perp_call(&mut ctx);

        // Mark $100 → $150 ⇒ band [$135, $165]: every ask (91–94) is stranded below it, and every
        // bid (87–90) is below it too — i.e. on the FAR side, where bids are harmless.
        oracle_update(&mut ctx, 15_000, 31);
        storage::flush_account_snapshots(&mut ctx).unwrap();

        assert!(
            asks(&mut ctx).is_empty(),
            "all four ask levels are in the stranded prefix: {:?}",
            asks(&mut ctx)
        );
        assert_eq!(
            bids(&mut ctx),
            vec![8_700, 8_800, 8_900, 9_000],
            "the bids are BELOW the new band — deep passive liquidity, not the touch. They must all \
             survive; expiring them would be the mass-kill this scoping exists to avoid"
        );

        let wallets_after: Vec<i64> = users
            .iter()
            .map(|u| {
                storage::load_account(&mut ctx, *u)
                    .unwrap()
                    .perp_wallet_balance
            })
            .collect();
        assert_eq!(
            wallets_after, wallets_before,
            "expiry moves no money: a resting order escrows nothing, so there is nothing to refund"
        );
        assert_eq!(conservation_sum(&mut ctx, &users), value_before);
        for u in users {
            let pos = position(&mut ctx, u);
            assert_eq!(
                (pos.total_sell_qty, pos.total_sell_notional),
                (0, 0),
                "every expired sell left the Ask aggregate"
            );
            assert!(pos.total_buy_notional > 0, "the surviving bids still count");
        }

        let logs = JournalTr::take_logs(ctx.journal_mut());
        let snapshots = logs
            .iter()
            .filter(|l| {
                l.data.topics().first()
                    == Some(&crate::interface::IPerpDex::AccountBalanceChanged::SIGNATURE_HASH)
            })
            .count();
        assert_eq!(
            snapshots, 0,
            "aggregates-only writes do not publish a snapshot — availableBalance rose for all \
             three users SILENTLY, which is the documented trigger rule, and is why the cancel \
             event now carries a reason"
        );
        let expiries = logs
            .iter()
            .filter(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
            .count();
        assert_eq!(expiries, 12, "four levels x three owners");
    }

    /// The near-side `continue` in `match_order` must STAY. The GC is capped and therefore lags by
    /// design, so between the mark move and the update that collects it the fill-time band is the
    /// only thing standing between a taker and an off-mark price. Pinned here because the GC makes
    /// the `continue` look redundant.
    #[test]
    fn the_fill_time_band_still_backstops_a_prefix_the_cap_has_not_reached() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        fund_big(&mut ctx, MAKER);
        fund_big(&mut ctx, ALICE);
        for _ in 0..MAX_BAND_EXPIRIES_PER_UPDATE + 1 {
            place_order(&mut ctx, MAKER, 1, 9_500, 1);
        }
        oracle_update(&mut ctx, 15_000, 31);
        // One order survived the cap, so the stranded level is still the touch.
        assert_eq!(
            storage::load_ask_count(&mut ctx, MARKET_ID, 9_500).unwrap(),
            1
        );
        assert_eq!(storage::load_best_ask(&mut ctx, MARKET_ID).unwrap(), 9_500);

        // A crossing buy still cannot take it: 9_500 is below the new lower edge (13_500).
        assert_out_of_band_best(&mut ctx, Side::Sell, 9_500);
        // The taker's own limit sits INSIDE the new band, so nothing about the taker is out of band
        // and it rests legitimately — the previous $200 limit is now refused as a too-good best,
        // which would have made this a placement test rather than a fill-band one. $140 still
        // crosses the stranded $95 ask by a wide margin, which is all the walk needs.
        let taker_price = 14_000;
        let (upper, lower) = crate::math::mark_band_bounds(15_000, 0);
        assert!(
            (taker_price as u128) >= lower && (taker_price as u128) <= upper,
            "precondition: the taker itself is in band [{lower}, {upper}]"
        );
        assert!(
            taker_price >= 9_500,
            "precondition: and it crosses the stranded ask in PRICE, so only the band stops it"
        );
        place_order(&mut ctx, ALICE, 0, taker_price, 1);
        assert_eq!(
            position(&mut ctx, ALICE).amount,
            0,
            "the fill-time band is still the backstop while the GC lags"
        );
        assert_eq!(
            storage::load_ask_count(&mut ctx, MARKET_ID, 9_500).unwrap(),
            1,
            "and the survivor really was still there to be taken — the band, not an empty level, \
             is what stopped the fill"
        );
    }
}

// ── ORDERED `ACCOUNT_UPDATE` STREAM SHAPE: liquidation and ADL ───────────────────────────────────
//
// Both of these were structurally uncoverable by the old hand-attached snapshot helper (it ran only
// in `trading::tests::account_snapshot_events`), and both are the paths where an orphan
// `PositionChanged` is most damaging: a liquidation close and an ADL fill each move TWO users'
// positions in one call. The invariant itself is enforced at emit time by the guard in
// `crate::events`; these pin the exact ordered `(event, subject)` sequence, which is the only form
// that catches a row landing one slot early or one slot late.
mod account_update_stream {
    use super::*;
    use crate::events::stream_test_support::{
        account_update_reasons, assert_account_update_groups, stream_shape,
    };
    use crate::types::AccountUpdateReason as R;

    /// **A liquidation is SEVERAL `ACCOUNT_UPDATE` pushes, not one settled end state.**
    ///
    /// The liquidated user's close goes through `match_order` as a taker, so it publishes the taker
    /// group like any other order — the header immediately before the close's own position row. That
    /// emit used to be SUPPRESSED for a `liquidation_close` (on the grounds that the account is only
    /// half-liquidated at that point), which under the adjacency rule left the close's
    /// `PositionChanged` an ORPHAN trailing the MAKER's group: an indexer would have booked the
    /// liquidated user's flattened position onto the maker's account.
    ///
    /// The later legs (here: nothing further moves ALICE's wallet, since
    /// `liquidation_fee_rate_bps == 0`) re-mark the user where they exist and the drain publishes the
    /// settled end state as a 0-position group.
    #[test]
    fn a_liquidation_close_publishes_the_liquidated_users_group_not_an_orphan_row() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
        place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        storage::begin_perp_call(&mut ctx);
        liquidate(&mut ctx, ALICE).unwrap();
        storage::flush_account_snapshots(&mut ctx).unwrap();

        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_account_update_groups(&logs);
        assert_eq!(
            stream_shape(&logs),
            vec![
                // the synthetic close order the liquidation places on ALICE's behalf
                ("OrderPlaced", Some(ALICE)),
                // the book leg: `Trade` outside the groups, MAKER's group, then ALICE's close group
                ("Trade", Some(ALICE)),
                ("AccountBalanceChanged", Some(MAKER)),
                ("PositionChanged", Some(MAKER)),
                ("AccountBalanceChanged", Some(ALICE)),
                ("PositionChanged", Some(ALICE)),
                ("Liquidation", Some(ALICE)),
                // …and NOTHING after it. `liquidation_fee_rate_bps == 0` here, so the clearance fee
                // writes nothing and re-marks nobody, and the book fill zeroed both position legs
                // exactly, so `execute_liquidation_market_order`'s residual cleanup has nothing to
                // write either. With a non-zero rate the clearance fee would add one legitimate
                // 0-position group carrying the debited wallet.
            ],
            "the liquidated user's close is her OWN push — the row must not trail the maker's group"
        );
        // Both rows report `ORDER`, and that is not a shortcut: the liquidation close really IS an
        // order — `execute_liquidation_market_order` builds one and runs it through `match_order`,
        // so the maker leg and the liquidated user's taker leg are the same code path any fill takes.
        // Binance reports its own liquidations the same way (their liquidation engine places orders;
        // their `a.m` vocabulary has no LIQUIDATION value at all).
        assert_eq!(
            account_update_reasons(&logs),
            vec![(MAKER, R::Order), (ALICE, R::Order)],
        );
    }

    /// **The clearance fee is `INSURANCE_CLEAR`.** Same fixture as the test above with a non-zero
    /// `liquidation_fee_rate_bps`, which adds the one leg that test's comment predicts: a 0-position
    /// drain group for the liquidated user carrying the fee debited to the insurance fund.
    ///
    /// `INSURANCE_CLEAR` is assigned by a rule that is checkable rather than a vibe — *the
    /// counterparty of this wallet move is the insurance fund* — which is why the same value covers
    /// both directions (`depositInsuranceFund` / `withdrawInsuranceFund` move the admin's wallet
    /// against the same pot).
    #[test]
    fn the_liquidation_clearance_fee_reports_insurance_clear() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        // 5% of the pre-liquidation margin, charged out of ALICE's wallet to the fund.
        let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
        m.liquidation_fee_rate_bps = 500;
        storage::save_market(&mut ctx, &m).unwrap();
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
        place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64);
        let wallet_before = storage::load_account(&mut ctx, ALICE)
            .unwrap()
            .perp_wallet_balance;
        let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());

        storage::begin_perp_call(&mut ctx);
        liquidate(&mut ctx, ALICE).unwrap();
        storage::flush_account_snapshots(&mut ctx).unwrap();

        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_account_update_groups(&logs);
        assert_eq!(
            account_update_reasons(&logs),
            vec![
                (MAKER, R::Order),
                (ALICE, R::Order),
                // the drain row: nothing but the clearance fee moved ALICE after her close
                (ALICE, R::InsuranceClear),
            ],
            "the fee leg is its own push, and it names the insurance fund as the counterparty"
        );
        // …and the fee was real, so the row is not an artefact of a zero move: 5% of the $200
        // pre-liquidation margin, capped at the positive wallet, credited to the fund.
        assert_eq!(
            storage::load_insurance_fund(&mut ctx).unwrap() - if_before,
            MARGIN as u64 * 500 / 10_000,
            "the fund really was credited the clearance fee"
        );
        assert!(
            storage::load_account(&mut ctx, ALICE)
                .unwrap()
                .perp_wallet_balance
                != wallet_before,
            "…and it came out of ALICE's wallet"
        );
    }

    /// **`ADJUSTMENT` for the residual closed at MARK.** The book absorbs half the position and the
    /// rest is closed at the mark price with no counterparty and no order — so `ORDER` would send a
    /// consumer looking for an order id and a `Trade` row that do not exist for this leg.
    #[test]
    fn a_residual_closed_at_mark_reports_adjustment() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
        // Only HALF the size is bid for, so the other half becomes the residual. The position is
        // below maintenance but not bankrupt at this mark, so the residual is SOLVENT and takes the
        // close-at-mark path rather than ADL.
        place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64 / 2);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        storage::begin_perp_call(&mut ctx);
        liquidate(&mut ctx, ALICE).unwrap();
        storage::flush_account_snapshots(&mut ctx).unwrap();

        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_account_update_groups(&logs);
        assert_eq!(
            position(&mut ctx, ALICE).amount,
            0,
            "fixture: the residual really did have to be settled at mark"
        );
        assert_eq!(
            account_update_reasons(&logs),
            vec![
                // the book leg, an ordinary fill for both sides
                (MAKER, R::Order),
                (ALICE, R::Order),
                // the residual: valued at mark, no counterparty, no order
                (ALICE, R::Adjustment),
            ],
        );
    }

    /// **One ADL fill is TWO pushes, one per party.** The `Adl` row sits outside both groups; the
    /// loser's header is derived from the in-memory working copies (its position/account are written
    /// once after the fill loop) and the winner's off the settled store its own two writes just made
    /// current.
    ///
    /// Merging the two was never an option: under the adjacency rule the second row would belong to
    /// the first header, i.e. the counterparty's position booked onto the liquidated user's account.
    /// This is the same fixture as
    /// `adl_closes_insolvent_residual_against_opposite_holder_conserving_no_if`, asserted on the
    /// stream instead of on the balances.
    #[test]
    fn an_adl_fill_publishes_two_groups_one_per_party() {
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
        seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 5, 0);
        let _ = JournalTr::take_logs(ctx.journal_mut());

        // Crash to $75: ALICE is insolvent (bankruptcy price $80) and the book is empty, so the
        // whole residual goes to ADL against KEEPER. `run_update_index_price` is the shell-free
        // sweep entry, so the drain has to be driven by hand.
        storage::begin_perp_call(&mut ctx);
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
        storage::flush_account_snapshots(&mut ctx).unwrap();

        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_account_update_groups(&logs);
        assert_eq!(
            stream_shape(&logs),
            vec![
                // the synthetic close order: the book is empty, so it fills nothing and the whole
                // position becomes the insolvent residual ADL closes
                ("OrderPlaced", Some(ALICE)),
                ("Adl", Some(ALICE)),
                ("AccountBalanceChanged", Some(ALICE)),
                ("PositionChanged", Some(ALICE)),
                ("AccountBalanceChanged", Some(KEEPER)),
                ("PositionChanged", Some(KEEPER)),
                ("Liquidation", Some(ALICE)),
                // the oracle update that drove the sweep, emitted after it
                ("IndexPriceUpdated", None),
                ("MarkPriceUpdated", None),
            ],
            "one ADL fill → two pushes, each owning exactly its own party's position row"
        );
        // BOTH ADL legs are `ADJUSTMENT`. The winner is the clearer half of the argument: candidates
        // holding ANY resting order are excluded, so KEEPER provably placed no order, and the only
        // log naming them is `Adl` — there is no `Trade` row and no order id for a consumer to look
        // up, which is exactly what `ORDER` would promise.
        assert_eq!(
            account_update_reasons(&logs),
            vec![(ALICE, R::Adjustment), (KEEPER, R::Adjustment)],
        );
    }

    /// The admin's own two insurance-fund selectors report `INSURANCE_CLEAR` as well — the same rule
    /// the clearance fee is assigned by (*the counterparty of this wallet move is the fund*), just
    /// with the admin on the other end and both directions of it.
    ///
    /// Driven directly rather than through the shell, so the call boundary that drains the coalescing
    /// set is explicit; both selectors produce a plain 0-position group.
    #[test]
    fn the_insurance_fund_selectors_report_insurance_clear() {
        use crate::interface::IPerpDex::{depositInsuranceFundCall, withdrawInsuranceFundCall};
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        // The fund is never written directly (that would be a mint); it is seeded out of the admin's
        // own perp wallet, which is exactly the move under test.
        storage::save_account(
            &mut ctx,
            ADMIN,
            UserAccount {
                perp_wallet_balance: 1_000_000,
                ..UserAccount::default()
            },
            R::Adjustment,
        )
        .unwrap();

        for (call, label) in [
            (depositInsuranceFundCall { amount: 400_000 }.abi_encode(), "deposit"),
            (
                withdrawInsuranceFundCall { amount: 400_000 }.abi_encode(),
                "withdraw",
            ),
        ] {
            let _ = JournalTr::take_logs(ctx.journal_mut());
            storage::begin_perp_call(&mut ctx);
            if label == "deposit" {
                run_deposit_insurance_fund(&call, ADMIN, &mut ctx).unwrap();
            } else {
                run_withdraw_insurance_fund(&call, ADMIN, &mut ctx).unwrap();
            }
            storage::flush_account_snapshots(&mut ctx).unwrap();
            let logs = JournalTr::take_logs(ctx.journal_mut());
            assert_account_update_groups(&logs);
            assert_eq!(
                account_update_reasons(&logs),
                vec![(ADMIN, R::InsuranceClear)],
                "{label}InsuranceFund moves the admin's wallet against the fund"
            );
        }
    }

    /// **The one genuinely REACHABLE `Multiple`.** The drain publishes one row per user per call, so
    /// a user moved by two different causes with neither row published inline gets a single row that
    /// no single label describes — and rather than pick one and lie about the other, it says
    /// `Multiple`.
    ///
    /// This is the funding-plus-clearance-fee shape, and reaching it takes all four legs: ALICE is
    /// liquidatable, the book is EMPTY (so the close publishes nothing), her residual is INSOLVENT
    /// (so it takes the ADL path rather than the close-at-mark path, which would have published), and
    /// no eligible opposite holder exists (so `run_adl` defers and writes nothing). Nothing publishes
    /// for her — so the `FundingFee` mark from the write that persisted her funding settlement is
    /// still standing when the clearance fee marks her `InsuranceClear`.
    ///
    /// Remove any one leg and the row becomes single-valued, which is the point: `Multiple` is what
    /// the rule produces in a corner, not a bucket the common paths fall into.
    #[test]
    fn two_causes_with_no_inline_emit_collapse_to_multiple() {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
        m.liquidation_fee_rate_bps = 500;
        storage::save_market(&mut ctx, &m).unwrap();
        // A long that is insolvent at $75 (bankruptcy price $80) with a positive wallet, so the
        // clearance fee has something to bite. No KEEPER position at all → no ADL candidate.
        seed_position_account(
            &mut ctx,
            ALICE,
            QTY,
            -ENTRY_VALUE,
            MARGIN,
            5,
            USER_WALLET as i64,
        );
        // An epoch a LONG owes against, so `compute_funding_settlement` really moves `pos.margin`
        // and `liquidate_position`'s post-funding position write marks her `FundingFee`.
        storage::save_funding_state(
            &mut ctx,
            MARKET_ID,
            &FundingState {
                last_funding_rate: 1_000,
                next_funding_ts: u64::MAX,
                cumulative_funding_index: ENTRY_PRICE as i128 * 1_000,
            },
        )
        .unwrap();
        let _ = JournalTr::take_logs(ctx.journal_mut());

        storage::begin_perp_call(&mut ctx);
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
        storage::flush_account_snapshots(&mut ctx).unwrap();

        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_account_update_groups(&logs);
        assert_ne!(
            position(&mut ctx, ALICE).amount,
            0,
            "fixture: the residual must be left OPEN — an ADL that found a counterparty would have              published inline and cleared the mark"
        );
        let reasons = account_update_reasons(&logs);
        assert_eq!(
            reasons
                .iter()
                .filter(|(u, _)| *u == ALICE)
                .map(|(_, r)| *r)
                .collect::<Vec<_>>(),
            vec![
                // the funding group, published inline by `apply_funding_settlement`…
                R::FundingFee,
                // …and the ONE drained row, covering both the funding write and the clearance fee
                R::Multiple,
            ],
            "got {reasons:?}"
        );
    }
}
