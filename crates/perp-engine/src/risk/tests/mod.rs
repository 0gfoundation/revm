use context::ContextTr;
use super::*;
use alloy_sol_types::{SolCall, SolEvent};
use context::{BlockEnv, CfgEnv, Context, Journal, JournalTr, TxEnv};
use database::InMemoryDB;
use primitives::{address, hardfork::SpecId, U256};

use crate::{
    risk::funding::settle_position_funding,
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
/// An off-book opposite-side holder, used as the ADL counterparty wherever a fixture needs a
/// liquidation to actually COMPLETE. Every residual the book cannot absorb is now a forced trade
/// against a real position (there is no counterparty-free close any more), so a one-sided fixture
/// liquidates nothing — it defers. See `seed_absorbing_short`.
// `0xEE..` deliberately: `liquidation_cap` seeds its losers at `Address::from([i; 20])`
// for i in 1..=n, so anything in that range would COLLIDE and silently overwrite a loser.
const WHALE: Address = address!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
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
    // Isolated margin, explicitly: every position this suite opens is ISOLATED unless a test
    // switches it (`margin_type::test_gate::isolated_by_default`), under the real gate.
    crate::margin::margin_type::test_gate::isolated_by_default();
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
            basis_mode: 0,
            deferred_work: 0,
            cumulative_funding_index: 0,
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
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    // WHALE: the opposite side, so ALICE's book-unfillable residual has something to close
    // AGAINST. MAKER is a LONG, i.e. the same side as ALICE, so it is never an ADL candidate and
    // its "untouched" assertion below still measures what it did before.
    seed_absorbing_short(&mut ctx, 1);
    // All three are registered as open positions (insertion order).
    assert_eq!(
        storage::load_position_registry(&mut ctx, MARKET_ID).unwrap(),
        vec![ALICE, MAKER, WHALE]
    );

    // Crash the mark to $85 via updateIndexPrice (admin) — runs the sweep. At $85 the 5x long is
    // BELOW maintenance but still SOLVENT (equity >= 0), so its empty-book residual is ADL'd
    // AT MARK against WHALE — a real trade. (An insolvent residual takes the same path at the
    // bankruptcy price instead; see `adl_*` tests.)
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

    // TWO legs now, one per party — the close has a counterparty. ALICE's own economics are
    // unchanged from the counterparty-free settle this replaced (both value the residual at the
    // same mark), which is why `realizedPnl` is still exactly -$150.
    let position_changes = take_position_changes(&mut ctx);
    assert_eq!(position_changes.len(), 2);
    assert_eq!(position_changes[0].user, ALICE);
    assert_eq!(position_changes[0].realizedPnl, -150_000_000);
    assert_eq!(position_changes[0].closedQuantity, QTY as u64);
    assert_eq!(position_changes[1].user, WHALE);
    assert_eq!(
        position_changes[1].realizedPnl, 150_000_000,
        "a fill at MARK is zero-sum and economically neutral for the absorbing side"
    );
    assert_eq!(position_changes[1].closedQuantity, QTY as u64);

    // ALICE was under maintenance -> swept; MAKER stayed healthy -> untouched; WHALE absorbed
    // ALICE's whole residual and is now flat, so the registry holds only MAKER.
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
            ..crate::margin::margin_type::never_set_position()
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
    let close =
        execute_liquidation_market_order(&mut ctx, ALICE, &market, Side::Sell, quantity, None)
            .unwrap();

    assert_eq!(close.remaining, 1);
    // The 1 unit left over is NOT "the book ran out" — there is a 129th maker sitting right there
    // with a live bid. The walk stopped on the cap, and because that cap is per MATCH, retrying in
    // the same block gets a fresh allowance and eats it. That distinction is what the drain bit
    // needs, so it is reported rather than inferred from `remaining`.
    assert!(
        close.maker_cap_deferred,
        "the distinct-maker cap, not the book, is what stopped this walk"
    );
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
            maker_fee_ppm: 0,
            taker_fee_ppm: 1_000,
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
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
}

/// Seeds [`WHALE`] as a 5x SHORT of `contracts` @ $100, off-book and with no resting orders — the
/// mirror image of the `QTY`/`ENTRY_VALUE`/`MARGIN` long every fixture here uses, scaled up.
///
/// It exists because ADL is now the ONLY way a book-unfillable residual gets closed: there is no
/// counterparty-free settle left, so without an eligible opposite holder a liquidation defers and
/// the position stays open. Any mark below $100 leaves this short in profit and above maintenance,
/// so it qualifies as a candidate and stays healthy while absorbing.
fn seed_absorbing_short(ctx: &mut TestCtx, contracts: i64) {
    seed_position_account(
        ctx,
        WHALE,
        -(QTY * contracts),
        ENTRY_VALUE * contracts,
        MARGIN * contracts,
        5,
        0,
    );
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

/// **Σ amount is conserved across a liquidation — unconditionally.**
///
/// This is the invariant the solvent-residual rework exists for. A perp position can only ever be
/// closed against ANOTHER position: through the book, or through ADL. There is no longer any path
/// that zeroes a position with nothing on the other side.
///
/// It used to have one. A SOLVENT book-unfillable residual was "closed at mark against no
/// counterparty" — the protocol valued it and zeroed it. Money balanced at that instant (the
/// residual was valued at the same mark everyone else is marked at), which is why the
/// `conservation_sum` checks elsewhere never caught it, but Σ amount did not: afterwards
/// `Σ long != Σ short` and every later price move created or destroyed money system-wide with no
/// position on the other side to absorb it. Nothing recorded the imbalance either — `open_interest`
/// has no production writer.
///
/// So this asserts the thing the money check cannot see. The fixture is deliberately the SOLVENT
/// case (equity +$50 at the $85 mark), i.e. precisely the one that used to take the synthetic path.
#[test]
fn liquidation_conserves_sigma_amount() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    seed_absorbing_short(&mut ctx, 1);

    let users = [ALICE, WHALE];
    let value_before = conservation_sum(&mut ctx, &users);
    let amount_before: i64 = users.iter().map(|&u| position(&mut ctx, u).amount).sum();
    assert_eq!(amount_before, 0, "a market is zero-sum by construction");

    // $100 -> $85: ALICE is below maintenance but still SOLVENT (equity +$50). Empty book, so the
    // whole position is a residual.
    run_update_index_price(
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: 8_500, timestamp: 31 }.abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(position(&mut ctx, ALICE).amount, 0, "liquidated");
    assert_eq!(
        users.iter().map(|&u| position(&mut ctx, u).amount).sum::<i64>(),
        0,
        "Σ amount conserved — the residual was ABSORBED by a real position, not zeroed"
    );
    assert_eq!(
        position(&mut ctx, WHALE).amount,
        0,
        "WHALE took all 10 and is flat"
    );
    assert_eq!(conservation_sum(&mut ctx, &users), value_before, "and value conserved");
}

/// Cross P2: `UserAccount::open_position_count` (maintained at `save_position`'s zero-crossing)
/// through the liquidation paths — a liquidated loser and an ADL'd winner both closed to flat drop
/// to 0, and a winner only partly ADL'd keeps its 1. (The debug cross-check against a walk of the
/// index also runs on every snapshot these calls publish.)
#[test]
fn the_open_position_count_follows_a_liquidation_and_an_adl() {
    fn count(ctx: &mut TestCtx, user: Address) -> u8 {
        storage::load_account(ctx, user).unwrap().open_position_count
    }
    let crash = |ctx: &mut TestCtx, index_price: u64| {
        run_update_index_price(
            &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index_price, timestamp: 31 }
                .abi_encode(),
            ADMIN,
            ctx,
        )
        .unwrap();
    };

    // Insolvent at $75: ALICE is liquidated, her whole residual ADL'd against KEEPER — both flat.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, MARGIN, 5, 0);
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, KEEPER)), (1, 1));
    crash(&mut ctx, 7_500);
    assert_eq!((position(&mut ctx, ALICE).amount, position(&mut ctx, KEEPER).amount), (0, 0));
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, KEEPER)), (0, 0));

    // Solvent at $85 against a short twice her size: WHALE absorbs 10 of 20 and stays open.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    seed_absorbing_short(&mut ctx, 2);
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, WHALE)), (1, 1));
    crash(&mut ctx, 8_500);
    assert_eq!((position(&mut ctx, ALICE).amount, position(&mut ctx, WHALE).amount), (0, -QTY));
    assert_eq!((count(&mut ctx, ALICE), count(&mut ctx, WHALE)), (0, 1));
}

/// The fallback when there is nobody to absorb: the position STAYS OPEN. Accepted deliberately —
/// conserving Σ amount means a residual cannot be force-closed into thin air, so when ADL finds no
/// eligible counterparty the only honest outcome is to defer and re-sweep.
///
/// ⚠️ This is a real behaviour change for SOLVENT residuals, which previously always resolved. A
/// deferred position keeps accruing funding and can drift solvent -> insolvent, at which point the
/// ADL filter is STRICTER (it must absorb at the bankruptcy price, not at mark). The sibling test
/// above this one pins the same deferral for the insolvent case, which was already the behaviour.
#[test]
fn a_solvent_residual_with_no_counterparty_defers_instead_of_closing_into_thin_air() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    // No opposite side at all, and no book.
    let value_before = conservation_sum(&mut ctx, &[ALICE]);

    run_update_index_price(
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: 8_500, timestamp: 31 }.abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();

    assert_eq!(
        position(&mut ctx, ALICE).amount,
        QTY,
        "solvent residual DEFERRED, not force-closed against nobody"
    );
    assert_eq!(
        storage::load_position_registry(&mut ctx, MARKET_ID).unwrap(),
        vec![ALICE],
        "and it stays registered for the next sweep"
    );
    assert_eq!(
        conservation_sum(&mut ctx, &[ALICE]),
        value_before,
        "nothing moved on defer"
    );
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
    // `cr` on BOTH ADL legs. The legs reach `apply_position_fill` through `adl_fill`'s
    // TRIAL-then-commit dance (each side is filled on a CLONE and kept only if both come out
    // bad-debt free), so this is also the assertion that the accumulation is not lost with a
    // discarded trial — and, on the loser side, that it is not lost when the position is written
    // FLAT after the fill loop.
    assert_eq!(
        (
            position_changes[0].cumulativeRealizedPnl,
            position_changes[1].cumulativeRealizedPnl
        ),
        (-200_000_000, 200_000_000),
        "each ADL leg's lifetime total is its own realised PnL — this is each party's first close"
    );

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
    // …and it is still there on the FLAT positions both legs left behind, in the blob rather than
    // only in the event.
    assert_eq!(
        (
            position(&mut ctx, ALICE).cumulative_realized_pnl,
            position(&mut ctx, KEEPER).cumulative_realized_pnl
        ),
        (-200_000_000, 200_000_000),
        "`cr` survives the ADL close on both sides"
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

/// **INVERTED, deliberately.** This used to pin the opposite claim: a holder with ANY resting
/// order was EXCLUDED from ADL, and the fixture existed to prove the exclusion was asked of the
/// order lists rather than of a margin proxy (a PURE-REDUCE order reserves zero margin, so a
/// requirement-based proxy would have waved this holder through).
///
/// The exclusion is gone. It was the binding constraint on ADL in practice — the deeply-in-profit
/// holders ADL ranks first are exactly the ones sitting on a take-profit — and Binance does not
/// have it either: it selects such a holder and CANCELS their open orders. So does this engine now,
/// which is what removes the need for any flip-aware recompute: there is nothing left to re-price.
///
/// The fixture is kept verbatim because it is still the sharpest one — a pure-reduce order is the
/// hardest "has resting orders" case to detect — only the expectations are flipped.
#[test]
fn adl_fills_against_an_order_holder_and_cancels_their_orders() {
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
            maker_fee_ppm: 20_000,
            taker_fee_ppm: 0,
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
        crate::margin::view::position_open_order_margin(&keeper_market, &keeper_pos).unwrap(),
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

    // KEEPER is eligible despite the resting bid: the insolvent residual is ADL'd against him and
    // his book in this market is cleared as part of the fill.
    assert_eq!(
        position(&mut ctx, ALICE).amount,
        0,
        "the residual is absorbed, not deferred"
    );
    assert_eq!(
        position(&mut ctx, KEEPER).amount,
        0,
        "the order-holding holder IS the ADL counterparty"
    );
    let keeper_after = position(&mut ctx, KEEPER);
    assert_eq!(
        (keeper_after.total_buy_qty, keeper_after.total_sell_qty),
        (0, 0),
        "his resting bid is gone — this is the cancel that replaces the old exclusion"
    );
    assert!(
        storage::load_bid_prices(&mut ctx, MARKET_ID).unwrap().is_empty(),
        "and it left the book, not just his per-user list"
    );
    // `Adl`, NOT `Liquidation`: KEEPER was profitable and was picked for that reason. Reporting a
    // liquidation here would tell every downstream consumer the opposite of what happened.
    {
        use crate::interface::IPerpDex::OrderCancelled;
        assert!(
            JournalTr::take_logs(ctx.journal_mut())
                .into_iter()
                .filter(|l| l.data.topics().first() == Some(&OrderCancelled::SIGNATURE_HASH))
                .any(|l| {
                    OrderCancelled::decode_raw_log(l.data.topics(), &l.data.data)
                        .unwrap()
                        .reason
                        == CancelReason::Adl as u8
                }),
            "the cancel must be attributed to ADL"
        );
    }
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        if_before,
        "scheme X: ADL never touches the fund"
    );
    assert_eq!(conservation_sum(&mut ctx, &[ALICE, KEEPER]), value_before);
}

/// **The sub-unit bankruptcy regime: fill at one price unit, insurance fund takes the shortfall.**
///
/// `calc_bankruptcy_price` floors to 0 when a position's whole remaining credit is worth less than
/// ONE price unit spread over its size — `perp_core::math`'s
/// `bankruptcy_price_floors_to_zero_for_a_sub_unit_short` pins exactly when, and that it is
/// reachable rather than theoretical. ADL used to give up there, because filling at 0 would hand
/// the position to the winner for free. That stranded a position no price move can ever rescue: it
/// came back every tick, returned `Liquidated`, and burned one of the 50 slots, forever.
///
/// It now fills at ONE price unit — the closest representable price above the true bankruptcy
/// price, hence the one that minimises the shortfall — and routes the shortfall to the insurance
/// fund. ADL is bad-debt-free everywhere else ("scheme X: no IF"); this is the one deliberate
/// exception, and it is a QUOTE claim only: the position is TRANSFERRED, so Σ amount is conserved
/// exactly as on every other ADL path.
///
/// The bound is what makes it safe to grant: the shortfall cannot exceed the position's notional
/// at one price unit, and the regime's defining inequality says the position's entire credit is
/// below that same number. Dust by construction, not an open-ended claim on the fund.
///
/// ⚠️ This market has `base_decimals = 0`, `price_decimals = 2`, where `calc_value(1, q)` is exact
/// — so `p_b == 0` and "price 1 leaves a shortfall" are the SAME condition here and every case is
/// this one. Under decimals that leave flooring slack (the golden market's 8/9) the two separate,
/// and price 1 can come out exactly clean; that sub-case is the one pinned in `perp_core::math`.
#[test]
fn a_sub_unit_bankrupt_short_closes_at_one_price_unit_with_the_fund_covering_the_gap() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // vQuote 1 against QTY units: the exact bankruptcy price is 1e-5 of a price unit → floors to 0.
    seed_position_account(&mut ctx, ALICE, -QTY, 1, 0, 5, USER_WALLET as i64);
    // A healthy, order-free LONG on the other side — a perfectly eligible counterparty, which is
    // what makes "it still did not move" the sharp version of the old failure.
    seed_position_account(&mut ctx, KEEPER, QTY, -1, 1_000_000, 5, 0);
    storage::save_insurance_fund(&mut ctx, 1_000_000).unwrap();

    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);
    // Mark 2: insolvent, so the bankruptcy-price branch is the one taken.
    storage::save_mark_price(&mut ctx, MARKET_ID, 2).unwrap();

    liquidate(&mut ctx, ALICE).unwrap();

    assert_eq!(position(&mut ctx, ALICE).amount, 0, "closed, not stranded");
    assert_eq!(
        position(&mut ctx, ALICE).amount + position(&mut ctx, KEEPER).amount,
        0,
        "Σ amount conserved — the position was TRANSFERRED, which is the whole point"
    );

    let notional_at_one_unit = calc_value(1, QTY as u64, 0, PRICE_DECIMALS).unwrap();
    let shortfall = if_before - storage::load_insurance_fund(&mut ctx).unwrap();
    assert_eq!(
        shortfall,
        notional_at_one_unit - 1,
        "the fund absorbed exactly the loser's gap at price 1 (notional minus its 1 unit of credit)"
    );
    assert!(
        shortfall < notional_at_one_unit,
        "and it is bounded by the notional at one price unit"
    );
    assert_eq!(
        conservation_sum(&mut ctx, &[ALICE, KEEPER]),
        value_before,
        "value conserved: what the winner gained is what the fund paid"
    );
}

/// **The already-bankrupt fallback tier.** When every opposite holder is itself under water, ADL
/// used to find no candidate at all and defer — leaving a bankrupt position open, marking against
/// a market with no solvent counterparty left, re-swept and burning a slot every tick.
///
/// It now conscripts them at the SAME price it was already using (the loser's bankruptcy price)
/// and routes both sides' shortfalls to the insurance fund. The restriction is the whole argument:
/// this holder's deficit is already the fund's — the sweep is going to liquidate it and route its
/// bad debt there anyway — so this realises exposure that exists rather than creating any.
#[test]
fn adl_conscripts_an_already_bankrupt_counterparty_and_bills_the_fund() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // ALICE: 5x long 10 @ $100. At the $75 mark she is insolvent; her bankruptcy price is $80.
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    // KEEPER: 5x short 10 @ $50 — the only opposite holder, and BANKRUPT at $75 himself
    // (equity -$150), so the old filter skipped him outright and nothing could close.
    seed_position_account(&mut ctx, KEEPER, -QTY, 500_000_000, 100_000_000, 5, 0);
    storage::save_insurance_fund(&mut ctx, 1_000_000_000).unwrap();
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);

    oracle_tick(&mut ctx, 7_500, 31);

    assert_eq!(position(&mut ctx, ALICE).amount, 0, "the bankrupt long is closed, not deferred");
    assert_eq!(
        position(&mut ctx, ALICE).amount + position(&mut ctx, KEEPER).amount,
        0,
        "Σ amount conserved — conscripting a counterparty is still a real trade"
    );
    assert!(
        storage::load_insurance_fund(&mut ctx).unwrap() < if_before,
        "the fund took the shortfall it was going to take anyway, just earlier"
    );
    assert_eq!(
        conservation_sum(&mut ctx, &[ALICE, KEEPER]),
        value_before,
        "value conserved once the fund is counted"
    );
}

/// ⚠️ **INVERTED, as a deliberate temporary simplification.** This used to pin the opposite: a
/// holder SOLVENT at the mark but unable to absorb at the loser's bankruptcy price was skipped
/// outright, and the residual deferred.
///
/// Nobody is excluded now. Everything that cannot absorb bad-debt-free goes to the fallback tier
/// and the insurance fund covers the shortfall.
///
/// **The cost is real and is not being denied.** This holder is not bankrupt — nothing would
/// liquidate it at the mark — and the ADL price is a HAIRCUT (an insolvent long's `p_b` sits ABOVE
/// the mark, and a short's equity falls as price rises), so conscripting it manufactures fund
/// exposure that would not otherwise exist, at the expense of someone who did nothing wrong.
///
/// What it buys: ADL essentially cannot fail any more. `Σ amount == 0` holds inside a market
/// (every `apply_position_fill` call site is two-sided), so opposite capacity always covers the
/// residual; with nobody excluded the only remaining limit is the fill budget, and that is
/// progress rather than a stall. Bankrupt positions stop accumulating in the registry, which is
/// what was burning liquidation slots and re-arming `deferred_work` every tick.
///
/// ⚠️ When this is revisited, the fixture is already the right one — flip the assertions back and
/// the narrow-band exclusion returns. Note that "bill the fund only for the part it could not
/// absorb cleanly" is NOT available as a middle ground: `apply_position_fill` prorates both the
/// margin release and the vQuote fraction, so the shortfall is LINEAR in the quantity closed and
/// every slice is short in the same proportion as the whole.
#[test]
fn adl_conscripts_even_a_solvent_holder_and_the_fund_covers_the_haircut() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, MARGIN, 5, USER_WALLET as i64);
    // KEEPER: 5x short 10 @ $62.50. At the $60 mark his equity is +$150 against a $100 maintenance
    // requirement — comfortably ABOVE maintenance, so the sweep does not touch him either — but at
    // ALICE's $80 bankruptcy price it is -$50.
    //
    // ⚠️ The crash has to be this deep for the case to exist at all: the haircut is
    // `(p_b - mark) * qty` and the buffer is `mmr * notional`, so a holder is only "solvent but
    // sunk by the haircut" when `(p_b - mark)/mark` exceeds the maintenance rate. At a $75 mark the
    // 50M haircut is smaller than the 125M buffer and no such holder exists.
    seed_position_account(&mut ctx, KEEPER, -QTY, 625_000_000, 125_000_000, 5, 0);
    storage::save_insurance_fund(&mut ctx, 1_000_000_000).unwrap();
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    let value_before = conservation_sum(&mut ctx, &[ALICE, KEEPER]);

    oracle_tick(&mut ctx, 6_000, 31);

    assert_eq!(position(&mut ctx, ALICE).amount, 0, "the bankrupt long closes");
    assert_eq!(
        position(&mut ctx, KEEPER).amount,
        0,
        "and the solvent holder is conscripted to do it"
    );
    assert_eq!(
        if_before - storage::load_insurance_fund(&mut ctx).unwrap(),
        50_000_000,
        "the fund covers exactly his $50 shortfall at the $80 fill price — exposure that did not \
         exist before this fill, which is the cost being accepted here"
    );
    assert_eq!(
        conservation_sum(&mut ctx, &[ALICE, KEEPER]),
        value_before,
        "value conserved once the fund is counted"
    );
}

fn oracle_tick(ctx: &mut TestCtx, index: u64, ts: u64) {
    run_update_index_price(
        &updateIndexPriceCall { marketId: MARKET_ID, indexPrice: index, timestamp: ts }
            .abi_encode(),
        ADMIN,
        ctx,
    )
    .unwrap();
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

// ── The merged price event ──────────────────────────────────────────────────
//
// `MarkPriceUpdated` absorbed `IndexPriceUpdated`. These pin the two properties the merge exists
// for: exactly ONE price log per update (the duplication that is gone), and a self-contained
// funding payload on EVERY push (the caching a `@markPrice` consumer no longer has to do).

/// Every `MarkPriceUpdated` in `logs`, decoded.
fn mark_price_events(
    logs: &[primitives::Log],
) -> Vec<crate::interface::IPerpDex::MarkPriceUpdated> {
    logs.iter()
        .filter(|log| {
            log.data.topics().first()
                == Some(&crate::interface::IPerpDex::MarkPriceUpdated::SIGNATURE_HASH)
        })
        .map(|log| {
            crate::interface::IPerpDex::MarkPriceUpdated::decode_raw_log(
                log.data.topics(),
                &log.data.data,
            )
            .unwrap()
        })
        .collect()
}

/// `getFundingState`'s `(fundingRate, nextFundingTime)` read through the real selector, so the
/// event is compared against the ABI a consumer would actually poll — not against the struct the
/// emitter already had in hand.
fn get_funding_state_rate_and_next_time(ctx: &mut TestCtx, market_id: u64) -> (i64, u64) {
    let out = run_get_funding_state(
        &crate::interface::IPerpDex::getFundingStateCall {
            marketId: market_id,
        }
        .abi_encode(),
        ctx,
    )
    .unwrap();
    let decoded =
        crate::interface::IPerpDex::getFundingStateCall::abi_decode_returns(&out).unwrap();
    (decoded.fundingRate, decoded.nextFundingTime)
}

/// ANTI-REGRESSION for the duplication the merge removed: `updateIndexPrice` used to emit
/// `IndexPriceUpdated` and then `MarkPriceUpdated` describing the same mark, and a consumer had to
/// de-duplicate. There is now exactly one price log, and it carries the whole payload.
#[test]
fn update_index_price_emits_exactly_one_price_event() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());

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

    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert_eq!(
        logs.len(),
        1,
        "the oracle push emits ONE log on a quiet market, not a price pair"
    );
    let events = mark_price_events(&logs);
    assert_eq!(events.len(), 1, "exactly one MarkPriceUpdated");
    let e = &events[0];

    // Every field is the state this call just wrote, read back independently.
    let state = storage::load_index_price_state(&mut ctx, MARKET_ID).unwrap();
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    assert_eq!(e.marketId, MARKET_ID);
    assert_eq!(e.markPrice, market.mark_price);
    assert_eq!(e.indexPrice, ENTRY_PRICE);
    assert_eq!(e.indexPrice, state.index_price);
    // The FLOORED window timestamp (31 -> 30 at a 15s interval), i.e. what got persisted — NOT the
    // caller's 31, and not a delivery time.
    assert_eq!(e.priceWindowTs, 30);
    assert_eq!(e.priceWindowTs, state.timestamp);
    // Quiet market: no funding configured, no trades, empty basis window -> both components are
    // the index and the median with them is the index.
    assert_eq!(e.price1, ENTRY_PRICE);
    assert_eq!(e.price2, ENTRY_PRICE);
    assert_eq!(e.markPrice, ENTRY_PRICE);
    assert_eq!(e.updater, ADMIN);

    let (settled, next_ts) = get_funding_state_rate_and_next_time(&mut ctx, MARKET_ID);
    // Both 0 here, but for DIFFERENT reasons, and they are no longer the same quantity: with
    // `funding_interval == 0` there is no accumulator and no epoch, so the event falls back to the
    // settled rate (also 0). The predicted-vs-settled distinction is exercised by
    // `mark_price_event_carries_the_predicted_rate_not_the_settled_one`.
    assert_eq!(e.fundingRate, 0);
    assert_eq!(settled, 0);
    assert_eq!(e.nextFundingTime, next_ts);
}

/// `MarkPriceUpdated.fundingRate` is the PREDICTED rate — what this epoch would settle at if it
/// ended now — not the rate already settled at the previous boundary.
///
/// ⚠️ This test used to assert the opposite, and that is the point: it pinned
/// `getFundingState`'s SETTLED rate as the event's value, which was the bug. The discriminator is
/// step 1 — on the very first push the prediction already exists while nothing has settled, so the
/// two numbers DIVERGE. An implementation that publishes the settled rate reports 0 there.
///
/// `nextFundingTime` is unaffected and still tracks the funding state.
#[test]
fn mark_price_event_carries_the_predicted_rate_not_the_settled_one() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let mut market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    // 4 price-update slots per epoch.
    market.funding_interval = 60;
    // A non-zero interest rate is what makes the computed rate non-zero on a market whose mark
    // never leaves the index: rate = avgPI + clamp(interest - avgPI) = 0 + 100.
    market.interest_rate = 100;
    storage::save_market(&mut ctx, &market).unwrap();

    let push = |ctx: &mut TestCtx, ts: u64| {
        run_update_index_price(
            &updateIndexPriceCall {
                marketId: MARKET_ID,
                indexPrice: ENTRY_PRICE,
                timestamp: ts,
            }
            .abi_encode(),
            ADMIN,
            ctx,
        )
        .unwrap();
        JournalTr::take_logs(ctx.journal_mut())
    };

    // 1. ★ THE DISCRIMINATOR. First push of the first epoch: nothing has settled yet, but the
    //    prediction is already defined off this one sample — avgPI 0 (mark never leaves the index)
    //    + clamp(100 - 0) = 100. The settled rate is still 0, so the two DIVERGE here.
    let logs = push(&mut ctx, 16); // -> window 15, next funding at 75
    let e = mark_price_events(&logs).pop().unwrap();
    assert_eq!(e.fundingRate, 100, "prediction off the first sample");
    let (settled, next_ts) = get_funding_state_rate_and_next_time(&mut ctx, MARKET_ID);
    assert_eq!(settled, 0, "nothing has settled yet");
    assert_ne!(
        e.fundingRate, settled,
        "the event must NOT be republishing the settled rate — that was the bug"
    );
    assert_eq!(e.nextFundingTime, 75);
    assert_eq!(e.nextFundingTime, next_ts, "nextFundingTime is unaffected");

    // 2. The epoch boundary: a rate is computed, `FundingRateComputed` fires, and the merged event
    //    already carries the NEW rate (it is emitted after the funding state is written).
    let logs = push(&mut ctx, 76); // -> window 75 == next_funding_ts
    assert!(
        logs.iter().any(|log| log.data.topics().first()
            == Some(&crate::interface::IPerpDex::FundingRateComputed::SIGNATURE_HASH)),
        "the boundary push must still emit FundingRateComputed"
    );
    let boundary = mark_price_events(&logs).pop().unwrap();
    // The accumulator was just reset and re-seeded with THIS sample, so the event carries the
    // prediction for the NEW epoch. It happens to equal the rate that just settled here only
    // because this market's mark never leaves the index — see the reproducibility check below for
    // the version that does not depend on that coincidence.
    assert_eq!(boundary.fundingRate, 100);
    assert_eq!(boundary.nextFundingTime, 135);
    assert_eq!(
        get_funding_state_rate_and_next_time(&mut ctx, MARKET_ID).0,
        100,
        "and the boundary DID settle 100"
    );

    // 3. BETWEEN epochs — the case the merge exists for. No `FundingRateComputed`, but the price
    //    event is still self-contained.
    let logs = push(&mut ctx, 91); // -> window 90, well short of 135
    assert!(
        !logs.iter().any(|log| log.data.topics().first()
            == Some(&crate::interface::IPerpDex::FundingRateComputed::SIGNATURE_HASH)),
        "a mid-epoch push must NOT emit FundingRateComputed — that is the whole premise"
    );
    let mid = mark_price_events(&logs);
    assert_eq!(mid.len(), 1, "still exactly one price event mid-epoch");
    let mid = &mid[0];
    assert_eq!(mid.priceWindowTs, 90);
    assert_eq!(mid.nextFundingTime, 135);

    // ★ Reproducibility, which is what makes the field usable: the published number must be exactly
    // what a consumer gets by applying the SAME formula to the SAME published inputs —
    // `getAveragePremiumIndex` and `getMarket.interestRate`. This holds whatever the premium index
    // happens to be, so it does not lean on this market's mark sitting on the index.
    let avg = run_get_average_premium_index(
        &getAveragePremiumIndexCall { marketId: MARKET_ID }.abi_encode(),
        &mut ctx,
    )
    .unwrap();
    let avg = getAveragePremiumIndexCall::abi_decode_returns(&avg).unwrap();
    assert_eq!(
        mid.fundingRate,
        crate::math::calc_funding_rate(avg.avgPremiumIndex, 100),
        "the event's rate must be reproducible from the published average premium index"
    );

    // ★ The published triple is SELF-CONSISTENT: `P` is a function of the other two fields in the
    // same event, so a consumer can verify it without any second call — and one that disagrees
    // with our definition of `P` can recompute its own from `markPrice` and `fundingRate`.
    for e in [&e, &boundary, mid] {
        assert_eq!(
            e.estimatedSettlePrice,
            crate::math::calc_estimated_settle_price(e.markPrice, e.fundingRate),
            "P must equal mark / (1 + r) from this same event's fields"
        );
    }
    // And it is not a copy of the mark: r = 100 (0.01%) here, so it sits just below.
    assert!(
        mid.estimatedSettlePrice < mid.markPrice,
        "a positive rate must discount the mark DOWN, got P {} vs mark {}",
        mid.estimatedSettlePrice,
        mid.markPrice
    );
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
        "funding never touches the insurance fund"
    );
}

/// **Funding out of EQUITY**: a charge larger than the silo lands on `pos.margin` WHOLE and drives
/// it NEGATIVE — a debt the position owes. Same fixture as the test it replaces
/// (`settle_funding_charge_beyond_margin_absorbs_from_insurance_fund`: 250M against a 200M silo,
/// an 80M fund), whose old answer was margin 0 and the fund down 50M to 30M. Now the fund does not
/// move and the 50M is carried by the position, where the maintenance check, the close and the
/// liquidation all see it (`mod funding_debt`).
#[test]
fn settle_funding_charge_beyond_margin_drives_margin_negative() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    set_funding_index(&mut ctx, 2_500_000_000); // charge = 250M > margin 200M
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let (pos, account) = settle_alice_funding(&mut ctx);

    assert_eq!(
        account.perp_wallet_balance, USER_WALLET as i64,
        "wallet untouched even when the position cannot cover the charge"
    );
    assert_eq!(pos.margin, MARGIN - 250_000_000, "the whole charge, through zero");
    assert_eq!(pos.margin, -50_000_000);
    assert_eq!(pos.last_funding_index, 2_500_000_000, "re-anchored");
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        80_000_000,
        "the insurance fund is not a funding source any more"
    );
}

#[test]
fn healthy_candidate_scan_is_write_free_even_with_accrued_funding() {
    // commit-only #23: liquidate_position on a HEALTHY position computes funding in memory and
    // returns AboveMaintenance with ZERO writes. Previously it persisted the funding settle
    // (pos/account) and relied on checkpoint_revert/tx-revert to discard it; unit tests never
    // revert, so any pre-reject write is visible here.
    //
    // Two fixtures: an ordinary small charge (7.5M against the 200M silo), and a charge LARGER
    // than the silo (250M) on a long held up by unrealized profit at $130 — the state funding out
    // of equity creates. Its in-memory margin is −50M, a debt, yet equity
    // `1.3e9 − 1e9 − 50M = 250M` clears the `1.3e9 / 6 = 216_666_666` requirement, so it too is
    // healthy and must leave no trace.
    for (index, mark) in [(75_000_000i128, ENTRY_PRICE), (2_500_000_000, 13_000)] {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
        storage::save_mark_price(&mut ctx, MARKET_ID, mark).unwrap();
        set_funding_index(&mut ctx, index);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
        let pos_before = position(&mut ctx, ALICE);
        let acct_before = storage::load_account(&mut ctx, ALICE).unwrap();
        let writes_before = ctx.perp_write_count();
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
        let mut adl_budget = 128u32;
        let out = liquidate_position(
            &mut ctx,
            ALICE,
            MARKET_ID,
            &market,
            mark,
            KEEPER,
            &mut adl_budget,
            &mut false,
        )
        .unwrap();
        assert!(
            matches!(out, LiquidationOutcome::AboveMaintenance),
            "index {index}: {out:?}"
        );

        assert_eq!(
            storage::load_insurance_fund(&mut ctx).unwrap(),
            if_before,
            "index {index}: IF written"
        );
        assert_eq!(position(&mut ctx, ALICE), pos_before, "index {index}: position written");
        assert_eq!(
            storage::load_account(&mut ctx, ALICE).unwrap(),
            acct_before,
            "index {index}: account written"
        );
        assert_eq!(ctx.perp_write_count(), writes_before, "index {index}: a perp write");
        assert!(
            JournalTr::take_logs(ctx.journal_mut()).is_empty(),
            "index {index}: a healthy scan publishes no funding events"
        );
    }
}

/// Everything a REFUSED call must leave bit-identical: ALICE's stored position (`margin` AND
/// `last_funding_index`, i.e. the funding was not settled on the way out), her account, the
/// insurance fund and the perp write counter. Paired with an empty log journal at each call site.
fn alice_untouched_snapshot(ctx: &mut TestCtx) -> (PerpPosition, UserAccount, u64, u64) {
    (
        position(ctx, ALICE),
        storage::load_account(ctx, ALICE).unwrap(),
        storage::load_insurance_fund(ctx).unwrap(),
        ctx.perp_write_count(),
    )
}

#[test]
fn add_margin_rejected_after_funding_writes_nothing() {
    // commit-only #23 (validate-then-apply): a margin op that settles funding IN MEMORY and THEN
    // rejects must write nothing — not the settled margin, not the re-anchored index, not one
    // event. (This used to be phrased as "leaves the insurance fund untouched", back when a charge
    // past the silo drew on the fund; funding has no fund leg any more, so the sharp form of the
    // property is the whole write set.)
    let mut ctx = make_ctx();
    setup_market(&mut ctx); // ALICE wallet = USER_WALLET = 50M
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    // Charge 300M against the 200M silo: in memory the margin goes to −100M before the reject.
    set_funding_index(&mut ctx, 3_000_000_000);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    let before = alice_untouched_snapshot(&mut ctx);
    assert_eq!((before.0.margin, before.0.last_funding_index), (MARGIN, 0));
    let _ = JournalTr::take_logs(ctx.journal_mut());

    // Funding never drains the wallet, so the reject is driven by asking for more than the
    // (untouched) 50M wallet holds — the withdrawable-balance cap, Binance's `-4050`.
    let err = add_position_margin(&mut ctx, 100_000_000).unwrap_err();
    assert!(err.to_string().contains(super::ADD_MARGIN_CAP_REJECT), "{err}");
    assert_eq!(
        alice_untouched_snapshot(&mut ctx),
        before,
        "a rejected addPositionMargin must leave position, account, fund and write counter as \
         they were"
    );
    assert!(
        JournalTr::take_logs(ctx.journal_mut()).is_empty(),
        "a rejected addPositionMargin must publish no funding events"
    );
}

#[test]
fn remove_margin_rejected_after_funding_writes_nothing() {
    // Symmetric to the add-margin case. The 300M charge takes the in-memory margin to −100M — a
    // debt — so G1 (`pos.margin < amount`) refuses even a 1M removal from a silo that is STORED at
    // 200M. The control below proves it is the settled debt G1 saw: with no funding pending the
    // same request clears G1 and is refused by G3 instead.
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
    set_funding_index(&mut ctx, 3_000_000_000); // charge 300M > margin 200M → in-memory −100M
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    let before = alice_untouched_snapshot(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    let err = remove_position_margin(&mut ctx, 1_000_000).unwrap_err();
    assert!(
        err.to_string().contains(REMOVE_MARGIN_G1_REJECT),
        "{err}"
    );
    assert_eq!(
        alice_untouched_snapshot(&mut ctx),
        before,
        "a rejected removePositionMargin must leave position, account, fund and write counter as \
         they were"
    );
    assert!(
        JournalTr::take_logs(ctx.journal_mut()).is_empty(),
        "a rejected removePositionMargin must publish no funding events"
    );

    // Control: identical, no funding pending. G1 passes (200M ≥ 1M); G3 is the gate that refuses
    // (equity 199M after the removal against a 200M PIM at leverage 5).
    let mut ctl = make_ctx();
    setup_market(&mut ctl);
    save_position(&mut ctl, QTY, -ENTRY_VALUE);
    let err = remove_position_margin(&mut ctl, 1_000_000).unwrap_err();
    assert!(
        err.to_string().contains(REMOVE_MARGIN_G3_REJECT),
        "control: {err}"
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
            ..crate::margin::margin_type::never_set_position()
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
            flags: 0,
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
        crate::margin::view::position_open_order_margin(&market2, &pos2).unwrap(),
        USER_WALLET,
        "the whole wallet is committable on market 2"
    );
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET,
        "resting escrows nothing — the wallet is untouched by the placement"
    );
    assert_eq!(
        crate::margin::view::derived_available_balance(&mut ctx, ALICE).unwrap(),
        0,
        "and the full 50M was available to commit — market-1 funding took none of it"
    );
}

/// `E` decoded out of `logs`, in stream order.
fn decode_events<E: SolEvent>(logs: &[primitives::Log]) -> Vec<E> {
    logs.iter()
        .filter(|log| log.data.topics().first() == Some(&E::SIGNATURE_HASH))
        .map(|log| E::decode_raw_log(log.data.topics(), &log.data.data).unwrap())
        .collect()
}

/// **A funding charge past the silo is a DEBT on the position, and the event stream says so.**
///
/// Folds two tests the rule change retired: `funding_charge_past_margin_absorbs_from_insurance_fund_
/// and_logs` (250M against a 200M silo, an 80M fund: margin 0, fund −50M, one
/// `InsuranceFundChanged`) and `funding_charge_past_the_insurance_fund_emits_depletion` (500M:
/// margin 0, fund drained to 0, `InsuranceFundDepleted` for 220M). The second is a deep-charge row
/// here rather than a test of its own because funding can no longer create bad debt AT ALL — the
/// only way a debt reaches the fund now is a shortfall at close or liquidation. The depletion that
/// test used to pin moved with it: `mod funding_debt`'s C4 has a row whose fund cannot cover the
/// close's shortfall, and it pins `InsuranceFundDepleted` (marketId, badDebt) there.
///
/// Per row: the margin goes negative by exactly the shortfall; the fund, the wallet and the stored
/// account do not move; there is NO `InsuranceFundChanged` / `InsuranceFundDepleted`; and the stream
/// is exactly the funding group — `FundingSettled` whose `amount` is the payment APPLIED (== Δiw,
/// nothing clamped), then the `FundingFee` header with the signed totals, then the position row
/// carrying the negative margin.
#[test]
fn funding_charge_past_margin_is_a_position_debt_with_no_insurance_fund_event() {
    use crate::events::stream_test_support::{account_update_reasons, stream_shape};
    use crate::interface::IPerpDex::{
        AccountBalanceChanged, FundingSettled, InsuranceFundChanged, InsuranceFundDepleted,
        PositionChanged,
    };
    // (index, charge, margin after). 2.5e9 / 5e9 on a QTY long are 250M / 500M.
    for (index, charge, margin_after) in [
        (2_500_000_000i128, 250_000_000i64, -50_000_000i64),
        (5_000_000_000, 500_000_000, -300_000_000),
    ] {
        let mut ctx = make_ctx();
        setup_market(&mut ctx);
        storage::save_insurance_fund(&mut ctx, 80_000_000).unwrap();
        set_funding_index(&mut ctx, index);
        save_position(&mut ctx, QTY, -ENTRY_VALUE);
        let account_before = storage::load_account(&mut ctx, ALICE).unwrap();
        assert_eq!(
            account_before.total_position_margin, MARGIN,
            "fixture: the stored Σ is the pre-funding silo"
        );
        let _ = JournalTr::take_logs(ctx.journal_mut());

        let (pos, account) = settle_alice_funding(&mut ctx);
        assert_eq!(pos.margin, MARGIN - charge);
        assert_eq!(pos.margin, margin_after, "charge {charge}");
        assert_eq!(account, account_before, "charge {charge}: funding writes no account leg");
        assert_eq!(
            storage::load_insurance_fund(&mut ctx).unwrap(),
            80_000_000,
            "charge {charge}: the fund is not touched"
        );

        let logs = JournalTr::take_logs(ctx.journal_mut());
        assert_eq!(
            stream_shape(&logs),
            vec![
                ("FundingSettled", Some(ALICE)),
                ("AccountBalanceChanged", Some(ALICE)),
                ("PositionChanged", Some(ALICE)),
            ],
            "charge {charge}: the funding group and nothing else"
        );
        assert!(decode_events::<InsuranceFundChanged>(&logs).is_empty());
        assert!(decode_events::<InsuranceFundDepleted>(&logs).is_empty());

        let settled = decode_events::<FundingSettled>(&logs);
        assert_eq!(
            (settled[0].user, settled[0].marketId, settled[0].amount),
            (ALICE, MARKET_ID, -charge),
            "charge {charge}: FundingSettled.amount is the payment applied"
        );
        assert_eq!(
            settled[0].amount,
            pos.margin - MARGIN,
            "charge {charge}: …which is exactly Δiw"
        );

        // The header: cw unchanged, wb = cw + Σ with the Σ leg now carrying the debt — signed.
        let header = decode_events::<AccountBalanceChanged>(&logs);
        assert_eq!(
            account_update_reasons(&logs),
            vec![(ALICE, AccountUpdateReason::FundingFee)]
        );
        assert_eq!(header[0].totalCrossWalletBalance, USER_WALLET as i64);
        assert_eq!(
            header[0].totalWalletBalance,
            USER_WALLET as i64 + margin_after,
            "charge {charge}: wb = cw + Σ margin, the debt included"
        );
        assert_eq!(header[0].totalWalletBalance, USER_WALLET as i64 + MARGIN - charge);

        let row = decode_events::<PositionChanged>(&logs);
        assert_eq!(
            (row[0].amount, row[0].vQuoteBalance, row[0].margin),
            (QTY, -ENTRY_VALUE, margin_after),
            "charge {charge}: the funding row carries the negative margin as the new iw"
        );
    }
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
    // The counterparty the empty-book residual is closed against — without one the liquidation
    // defers and this test would measure deferral instead of "funding alone can trigger a sweep".
    // At the unchanged $100 mark this short has equity $200 against a $166.67 requirement, so it
    // is itself above maintenance, and a positive funding index CREDITS a short.
    seed_absorbing_short(&mut ctx, 1);
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
            flags: 0,
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
            flags: 0,
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

/// `setMarginTiers` with every tier's maintenance rate DERIVED (an all-zero rate column) — the
/// table shape every test wrote before the column existed.
fn set_margin_tiers(
    ctx: &mut TestCtx,
    caller: Address,
    market_id: u64,
    lower_bounds: &[u64],
    max_leverages: &[u32],
) -> Result<Bytes, PerpError> {
    set_margin_tiers_with_rates(
        ctx,
        caller,
        market_id,
        lower_bounds,
        max_leverages,
        &vec![0; lower_bounds.len()],
    )
}

fn set_margin_tiers_with_rates(
    ctx: &mut TestCtx,
    caller: Address,
    market_id: u64,
    lower_bounds: &[u64],
    max_leverages: &[u32],
    maint_margin_rates_ppm: &[u32],
) -> Result<Bytes, PerpError> {
    set_margin_tiers_capped(
        ctx,
        caller,
        market_id,
        lower_bounds,
        max_leverages,
        maint_margin_rates_ppm,
        0,
    )
}

/// The full `setMarginTiers` call, the last bracket's `notionalCap` included (`0` = unbounded).
fn set_margin_tiers_capped(
    ctx: &mut TestCtx,
    caller: Address,
    market_id: u64,
    lower_bounds: &[u64],
    max_leverages: &[u32],
    maint_margin_rates_ppm: &[u32],
    notional_cap: u64,
) -> Result<Bytes, PerpError> {
    run_set_margin_tiers(
        &setMarginTiersCall {
            marketId: market_id,
            lowerBounds: lower_bounds.to_vec(),
            maxLeverages: max_leverages.to_vec(),
            maintMarginRatesPpm: maint_margin_rates_ppm.to_vec(),
            notionalCap: notional_cap,
        }
        .abi_encode(),
        caller,
        ctx,
    )
}

fn get_margin_tiers_full(
    ctx: &mut TestCtx,
    market_id: u64,
) -> crate::interface::IPerpDex::getMarginTiersReturn {
    let out = run_get_margin_tiers(
        &getMarginTiersCall {
            marketId: market_id,
        }
        .abi_encode(),
        ctx,
    )
    .unwrap();
    getMarginTiersCall::abi_decode_returns(&out).unwrap()
}

/// All six `getMarginTiers` columns as one comparable value (the `sol!` return struct derives
/// neither `PartialEq` nor `Debug`).
#[allow(clippy::type_complexity)]
fn get_margin_tiers_columns(
    ctx: &mut TestCtx,
    market_id: u64,
) -> (Vec<u64>, Vec<u32>, Vec<u32>, Vec<u32>, Vec<u64>, Vec<u64>) {
    let d = get_margin_tiers_full(ctx, market_id);
    (
        d.lowerBounds,
        d.maxLeverages,
        d.maintMarginRatesPpm,
        d.effectiveMaintMarginRatesPpm,
        d.cums,
        d.notionalCaps,
    )
}

/// The `(lowerBounds, maxLeverages)` pair — the two columns every pre-rate test compares.
fn get_margin_tiers(ctx: &mut TestCtx, market_id: u64) -> (Vec<u64>, Vec<u32>) {
    let decoded = get_margin_tiers_full(ctx, market_id);
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
    crate::margin::view::position_open_order_margin(&market, &pos).unwrap()
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
            ..crate::margin::margin_type::never_set_position()
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
        .map(|i| crate::types::MAX_MARGIN_TIERS as u32 - i)
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
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            &[17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1],
            "at most 16 tiers allowed",
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
        (&[0], &[0], "maxLeverage must be 1–150"),
        (&[0], &[151], "maxLeverage must be 1–150"),
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
/// `getMarginTiers` and `getPositionRisk`, deliberately NOT `getPosition`'s all-zeros.
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
    assert_eq!(updated[0].maintMarginRatesPpm, vec![0, 0]);
}

// ── Explicit maintenance-margin rates (Binance `leverageBracket`) ─────────────

/// One dollar in quote units.
const USD: u64 = 1_000_000;

/// Binance BTCUSDT's 10-bracket `leverageBracket` table as the cross-margin research read it (the
/// 2023 table; its 3x bracket's 15% was read off an image), loaded VERBATIM — bracket 1's 125x
/// included. `(notionalFloor $, initialLeverage, maintMarginRatio ppm)`. Binance's CURRENT table is
/// [`BTCUSDT_LIVE`].
const BTCUSDT_2023: [(u64, u32, u32); 10] = [
    (0, 125, 4_000),
    (50_000, 100, 5_000),
    (250_000, 50, 10_000),
    (3_000_000, 20, 25_000),
    (15_000_000, 10, 50_000),
    (30_000_000, 5, 100_000),
    (80_000_000, 4, 125_000),
    (100_000_000, 3, 150_000),
    (200_000_000, 2, 250_000),
    (300_000_000, 1, 500_000),
];

/// Binance BTCUSDT's `leverageBracket` as `GET /fapi/v1/leverageBracket?symbol=BTCUSDT` returned
/// it on 2026-10-14 (internal-docs `misc/evidence/binance-run21-upnl.json`, request 11): 12
/// brackets, 150x at 0.4% first, the 3x bracket's 15% confirmed. `(notionalFloor $,
/// initialLeverage, maintMarginRatio ppm)`.
const BTCUSDT_LIVE: [(u64, u32, u32); 12] = [
    (0, 150, 4_000),
    (300_000, 100, 5_000),
    (800_000, 75, 6_500),
    (3_000_000, 50, 10_000),
    (12_000_000, 25, 20_000),
    (70_000_000, 20, 25_000),
    (100_000_000, 10, 50_000),
    (230_000_000, 5, 100_000),
    (480_000_000, 4, 125_000),
    (600_000_000, 3, 150_000),
    (800_000_000, 2, 250_000),
    (1_200_000_000, 1, 500_000),
];
/// The last bracket's `notionalCap` Binance returned with [`BTCUSDT_LIVE`], USDT — its maximum
/// position at any leverage.
const BTCUSDT_LIVE_CAP: u64 = 1_800_000_000;
/// The `cum` column Binance returned with [`BTCUSDT_LIVE`], USDT.
const BTCUSDT_LIVE_CUM: [u64; 12] = [
    0,
    300,
    1_500,
    12_000,
    132_000,
    482_000,
    2_982_000,
    14_482_000,
    26_482_000,
    41_482_000,
    121_482_000,
    421_482_000,
];

/// Installs `rows` (`$` floors) with the last bracket's `notionalCap` (`$`, `0` = unbounded).
fn install_rows(
    ctx: &mut TestCtx,
    rows: &[(u64, u32, u32)],
    cap_usd: u64,
) -> Result<Bytes, PerpError> {
    let bounds: Vec<u64> = rows.iter().map(|r| r.0 * USD).collect();
    let levs: Vec<u32> = rows.iter().map(|r| r.1).collect();
    let rates: Vec<u32> = rows.iter().map(|r| r.2).collect();
    set_margin_tiers_capped(
        ctx,
        ADMIN,
        MARKET_ID,
        &bounds,
        &levs,
        &rates,
        cap_usd * USD,
    )
}

fn install_btcusdt(ctx: &mut TestCtx) {
    install_rows(ctx, &BTCUSDT_2023, 0).unwrap();
}

/// A Binance bracket table loads through `setMarginTiers` and `getMarginTiers` hands back
/// Binance's own `leverageBracket` row: the stored column, the effective rate, the published `cum`
/// (0 / 50 / 1,300 / 46,300 / … / 101,421,300 USDT) and `notionalCap`.
#[test]
fn get_margin_tiers_reports_binance_btcusdt_brackets_verbatim() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    install_btcusdt(&mut ctx);

    let got = get_margin_tiers_full(&mut ctx, MARKET_ID);
    let rates: Vec<u32> = BTCUSDT_2023.iter().map(|r| r.2).collect();
    assert_eq!(
        got.lowerBounds,
        BTCUSDT_2023
            .iter()
            .map(|r| r.0 * USD)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        got.maxLeverages,
        BTCUSDT_2023.iter().map(|r| r.1).collect::<Vec<_>>()
    );
    assert_eq!(
        got.maintMarginRatesPpm, rates,
        "the stored column round-trips"
    );
    assert_eq!(
        got.effectiveMaintMarginRatesPpm, rates,
        "an explicit column IS the effective rate"
    );
    assert_eq!(
        got.cums,
        [
            0u64,
            50,
            1_300,
            46_300,
            421_300,
            1_921_300,
            3_921_300,
            6_421_300,
            26_421_300,
            101_421_300
        ]
        .map(|c| c * USD)
        .to_vec(),
        "Binance's published BTCUSDT cum column"
    );
    let mut caps: Vec<u64> = BTCUSDT_2023[1..].iter().map(|r| r.0 * USD).collect();
    caps.push(u64::MAX);
    assert_eq!(
        got.notionalCaps, caps,
        "next floor; the last band is unbounded"
    );
    assert_eq!(got.maxLeverages[0], 125, "bracket 1's 125x loads as published");
}

/// Binance's CURRENT BTCUSDT table — 12 brackets, 150x first — loads exactly as Binance returned it
/// and reads back as Binance's own `leverageBracket`: every row's `cum` and `notionalCap` are the
/// ones Binance sent with it, the last a finite $1.8B. 150x is `MAX_LEVERAGE_HARD_CAP`, so it is
/// `setLeverage`'s cap on that market; 151 is refused both as a tier and as a leverage, with zero
/// writes.
#[test]
fn binances_live_btcusdt_table_loads_verbatim_at_150x() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    install_rows(&mut ctx, &BTCUSDT_LIVE, BTCUSDT_LIVE_CAP).unwrap();

    let got = get_margin_tiers_full(&mut ctx, MARKET_ID);
    assert_eq!(
        got.lowerBounds,
        BTCUSDT_LIVE.iter().map(|r| r.0 * USD).collect::<Vec<_>>()
    );
    assert_eq!(
        got.maxLeverages,
        BTCUSDT_LIVE.iter().map(|r| r.1).collect::<Vec<_>>()
    );
    let rates: Vec<u32> = BTCUSDT_LIVE.iter().map(|r| r.2).collect();
    assert_eq!(got.maintMarginRatesPpm, rates);
    assert_eq!(got.effectiveMaintMarginRatesPpm, rates);
    assert_eq!(
        got.cums,
        BTCUSDT_LIVE_CUM.map(|c| c * USD).to_vec(),
        "the cum column Binance returned"
    );
    let mut caps: Vec<u64> = BTCUSDT_LIVE[1..].iter().map(|r| r.0 * USD).collect();
    caps.push(BTCUSDT_LIVE_CAP * USD);
    assert_eq!(got.notionalCaps, caps, "the notionalCap column Binance returned");

    set_leverage(&mut ctx, 150).unwrap();
    let err = set_leverage(&mut ctx, 151).unwrap_err();
    assert!(
        err.to_string().contains("setLeverage: leverage must be 1–150"),
        "{err}"
    );

    let before = get_margin_tiers_columns(&mut ctx, MARKET_ID);
    let mut over = BTCUSDT_LIVE;
    over[0].1 = 151;
    let err = install_rows(&mut ctx, &over, BTCUSDT_LIVE_CAP).unwrap_err();
    assert!(
        err.to_string().contains("maxLeverage must be 1–150"),
        "{err}"
    );
    assert_eq!(
        get_margin_tiers_columns(&mut ctx, MARKET_ID),
        before,
        "a refused table writes nothing"
    );
}

/// A DERIVED tier (column 0) reports `1/(2L)` rounded UP to whole ppm, and `cum` is Binance's
/// recursion over that reported column. The default table is one derived 3x tier.
#[test]
fn get_margin_tiers_reports_derived_rates_rounded_up() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);

    let got = get_margin_tiers_full(&mut ctx, MARKET_ID);
    assert_eq!(got.maintMarginRatesPpm, vec![0]);
    assert_eq!(
        got.effectiveMaintMarginRatesPpm,
        vec![166_667],
        "1/6 rounded up"
    );
    assert_eq!(got.cums, vec![0]);
    assert_eq!(got.notionalCaps, vec![u64::MAX]);

    // Mixed: derived 3x, derived 2x (exactly 250_000 ppm), explicit 40% at 1x.
    set_margin_tiers_with_rates(
        &mut ctx,
        ADMIN,
        MARKET_ID,
        &[0, 50_000 * USD, 250_000 * USD],
        &[3, 2, 1],
        &[0, 0, 400_000],
    )
    .unwrap();
    let got = get_margin_tiers_full(&mut ctx, MARKET_ID);
    assert_eq!(got.maintMarginRatesPpm, vec![0, 0, 400_000]);
    assert_eq!(
        got.effectiveMaintMarginRatesPpm,
        vec![166_667, 250_000, 400_000]
    );
    // cum_1 = 50_000e6 · (250_000 − 166_667) / 1e6 = 4_166_650_000
    // cum_2 = cum_1 + 250_000e6 · (400_000 − 250_000) / 1e6 = 4_166_650_000 + 37_500_000_000
    assert_eq!(got.cums, vec![0, 4_166_650_000, 41_666_650_000]);
    assert_eq!(
        got.notionalCaps,
        vec![50_000 * USD, 250_000 * USD, u64::MAX]
    );
}

/// The two new invariants, each with its own error string, each refused with ZERO writes — and the
/// boundary case of each accepted, so the check is exactly the documented inequality.
#[test]
fn set_margin_tiers_rejects_bad_maintenance_rates_individually() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let before = get_margin_tiers_columns(&mut ctx, MARKET_ID);

    // (lowerBounds, maxLeverages, maintMarginRatesPpm, expected error)
    type Case<'a> = (&'a [u64], &'a [u32], &'a [u32], &'a str);
    let cases: &[Case<'_>] = &[
        // The column must be index-aligned with the other two.
        (
            &[0, 1_000],
            &[3, 2],
            &[0],
            "maintMarginRatesPpm and lowerBounds length mismatch",
        ),
        (
            &[0],
            &[3],
            &[0, 0],
            "maintMarginRatesPpm and lowerBounds length mismatch",
        ),
        // MMR >= IMR: 4x has IMR 250_000 ppm; equality is refused (IM must EXCEED MM).
        (
            &[0],
            &[4],
            &[250_000],
            "must be below 1000000 / maxLeverage",
        ),
        (
            &[0],
            &[4],
            &[900_000],
            "must be below 1000000 / maxLeverage",
        ),
        (
            &[0],
            &[1],
            &[1_000_000],
            "must be below 1000000 / maxLeverage",
        ),
        // Only the offending tier is out of range; the first is fine.
        (
            &[0, 1_000],
            &[10, 5],
            &[50_000, 200_000],
            "must be below 1000000 / maxLeverage",
        ),
        // Explicit rates falling.
        (
            &[0, 1_000],
            &[3, 2],
            &[100_000, 90_000],
            "rates must be non-decreasing",
        ),
        // Derived 3x is 1/6 = 166_666.67 ppm: an explicit 166_666 after it FALLS (compared
        // exactly, not after rounding the derived rate to 166_667 or 166_666).
        (
            &[0, 1_000],
            &[3, 2],
            &[0, 166_666],
            "rates must be non-decreasing",
        ),
        // ... and an explicit rate above a following DERIVED one: 2x derived is 250_000.
        (
            &[0, 1_000],
            &[3, 2],
            &[300_000, 0],
            "rates must be non-decreasing",
        ),
    ];
    for (bounds, levs, rates, expected) in cases {
        let err = set_margin_tiers_with_rates(&mut ctx, ADMIN, MARKET_ID, bounds, levs, rates)
            .unwrap_err();
        assert!(
            err.to_string().contains(expected),
            "expected {expected:?}, got {err}"
        );
        assert_eq!(
            get_margin_tiers_columns(&mut ctx, MARKET_ID),
            before,
            "a reject must not write ({expected})"
        );
    }

    // The boundaries themselves are admitted: one ppm under the IMR, an explicit rate EQUAL to
    // the previous one, and 166_667 right after a derived 1/6.
    for (bounds, levs, rates) in [
        (&[0u64][..], &[4u32][..], &[249_999u32][..]),
        (&[0], &[1], &[999_999]),
        (&[0, 1_000], &[3, 2], &[100_000, 100_000]),
        (&[0, 1_000], &[3, 2], &[0, 166_667]),
    ] {
        set_margin_tiers_with_rates(&mut ctx, ADMIN, MARKET_ID, bounds, levs, rates)
            .unwrap_or_else(|e| panic!("{bounds:?} {levs:?} {rates:?} must be admitted: {e}"));
        assert_eq!(
            get_margin_tiers_full(&mut ctx, MARKET_ID).maintMarginRatesPpm,
            rates
        );
    }
}

#[test]
fn set_margin_tiers_emits_the_rate_column() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    set_margin_tiers_with_rates(
        &mut ctx,
        ADMIN,
        MARKET_ID,
        &[0, 1_000_000],
        &[3, 2],
        &[0, 200_000],
    )
    .unwrap();

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
    assert_eq!(
        updated[0].maintMarginRatesPpm,
        vec![0, 200_000],
        "as sent: 0 stays 0"
    );
}

/// The stored column reaches the ENFORCED rule, not just the getter: the maintenance requirement
/// the liquidation predicate applies to a BTCUSDT-table position is Binance's
/// `notional · MMR − cum`, exactly, at notionals in five different brackets.
#[test]
fn an_explicit_rate_column_is_the_maintenance_rule_liquidation_applies() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    install_btcusdt(&mut ctx);
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();

    // (notional $, Binance MM $ = N·MMR − cum)
    for (notional, mm) in [
        (10_000u64, 40u64),        // 0.4%
        (100_000, 450),            // 0.5%  − 50
        (1_000_000, 8_700),        // 1%    − 1,300
        (90_000_000, 7_328_700),   // 12.5% − 3,921,300
        (150_000_000, 16_078_700), // 15%   − 6,421,300 (the bracket Binance does NOT derive)
        (400_000_000, 98_578_700), // 50%   − 101,421,300
    ] {
        assert_eq!(
            crate::math::maintenance_margin(&market.tiers, (notional * USD) as i64).unwrap(),
            (mm * USD) as i64,
            "notional ${notional}"
        );
        // The liquidation predicate flips exactly there: equity == MM is healthy, one unit less
        // is not. A LONG of `notional` base units at price $1 on a 0/0-decimal grid has notional
        // `amount · 1e6` quote units; `v_quote_balance` puts its equity at the probed value.
        let n = (notional * USD) as i64;
        for (equity, healthy) in [((mm * USD) as i64, true), ((mm * USD) as i64 - 1, false)] {
            assert_eq!(
                crate::math::is_above_maintenance_margin(
                    &market.tiers,
                    1,
                    notional as i64,
                    equity - n,
                    0,
                    0,
                    0,
                )
                .unwrap(),
                healthy,
                "notional ${notional} equity {equity}"
            );
        }
    }
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

// ── The terminal notionalCap (Binance's last-bracket `notionalCap`) ───────────

/// `{0, 3x}` capped at `cap` — the default table with a maximum position.
fn install_capped_default(ctx: &mut TestCtx, cap: u64) {
    set_margin_tiers_capped(ctx, ADMIN, MARKET_ID, &[0], &[3], &[0], cap).unwrap();
}

/// An OPEN that leaves `|notional| >= notionalCap` is refused (`-2027`), at any leverage, pre-write;
/// one quote unit more of cap and the same open is accepted — the cap is exclusive, like every
/// tier's upper edge.
#[test]
fn notional_cap_refuses_an_opening_taker_at_the_cap() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // MAKER is already long 2, so his resting sell REDUCES and only ALICE's leg is gated.
    seed_position_account(
        &mut ctx,
        MAKER,
        2,
        -200_000_000,
        200_000_000,
        1,
        MAKER_WALLET as i64,
    );
    // qty 2 at $100: a 200_000_000 notional, at 1x — the most conservative leverage there is.
    stage_open_into_tier(&mut ctx, 1, 2);
    install_capped_default(&mut ctx, 200_000_000);

    let err = try_place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, 2).unwrap_err();
    assert!(
        err.to_string()
            .contains("placeOrder: -2027 position exceeds the market's maximum notional"),
        "{err}"
    );
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    assert_eq!(position(&mut ctx, MAKER).amount, 2, "the maker's ask is untouched");

    install_capped_default(&mut ctx, 200_000_001);
    try_place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, 2).unwrap();
    assert_eq!(position(&mut ctx, ALICE).amount, 2);
    assert_eq!(position(&mut ctx, MAKER).amount, 0);
}

/// The maker leg: a resting order whose fill would take the maker to the cap is cancelled instead
/// of filled — the same route as a tier refusal.
#[test]
fn notional_cap_refuses_the_maker_leg_too() {
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
    place_order(&mut ctx, MAKER, Side::Sell as u8, ENTRY_PRICE, 2);
    install_capped_default(&mut ctx, 150_000_000);

    try_place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, 2).unwrap();

    assert_eq!(position(&mut ctx, MAKER).amount, 0, "maker fill refused");
    assert_eq!(position(&mut ctx, ALICE).amount, 0, "nothing matched");
    assert!(
        storage::load_sell_orders(&mut ctx, MAKER, MARKET_ID)
            .unwrap()
            .is_empty(),
        "the refused maker order is cancelled"
    );
}

/// A position already past the cap (here: the cap lowered under it) may still REDUCE — closing is
/// never refused — but `setLeverage` on it is `-2027`: no leverage admits that size.
#[test]
fn notional_cap_never_blocks_a_reduce_and_refuses_set_leverage_past_it() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    stage_open_into_tier(&mut ctx, 1, 2);
    try_place_order(&mut ctx, ALICE, Side::Buy as u8, ENTRY_PRICE, 2).unwrap();
    assert_eq!(position(&mut ctx, ALICE).amount, 2);
    install_capped_default(&mut ctx, 100_000_000);

    let err = set_leverage(&mut ctx, 1).unwrap_err();
    assert!(
        err.to_string()
            .contains("setLeverage: -2027 position exceeds the market's maximum notional"),
        "{err}"
    );
    assert_eq!(position(&mut ctx, ALICE).leverage, 1, "refused: nothing written");

    place_order(&mut ctx, MAKER, Side::Buy as u8, ENTRY_PRICE, 1);
    try_place_order(&mut ctx, ALICE, Side::Sell as u8, ENTRY_PRICE, 1).unwrap();
    assert_eq!(position(&mut ctx, ALICE).amount, 1, "the reduce went through");
    // Now at exactly the cap: still no leverage admits it.
    assert!(set_leverage(&mut ctx, 1).is_err());
}

/// The setter: a bounded cap must sit strictly above the last floor (refused otherwise, zero
/// writes); `type(uint64).max` — what the getter reports for an unbounded band — is stored as `0`,
/// so a table read back and written again is unchanged; the event carries the STORED cap and the
/// getter reports it as the last `notionalCap`.
#[test]
fn set_margin_tiers_validates_stores_and_reports_the_notional_cap() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let bounds = [0, 1_000_000];
    let before = get_margin_tiers_columns(&mut ctx, MARKET_ID);
    for bad in [1_000_000, 999_999, 1] {
        let err = set_margin_tiers_capped(&mut ctx, ADMIN, MARKET_ID, &bounds, &[3, 2], &[0, 0], bad)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("setMarginTiers: notionalCap must be above the last lowerBound"),
            "cap {bad}: {err}"
        );
        assert_eq!(get_margin_tiers_columns(&mut ctx, MARKET_ID), before, "cap {bad} wrote");
    }

    let _ = JournalTr::take_logs(ctx.journal_mut());
    let caps_of_events = |ctx: &mut TestCtx| -> Vec<u64> {
        JournalTr::take_logs(ctx.journal_mut())
            .into_iter()
            .filter(|log| {
                log.data.topics().first() == Some(&IPerpDex::MarginTiersUpdated::SIGNATURE_HASH)
            })
            .map(|log| {
                IPerpDex::MarginTiersUpdated::decode_raw_log(log.data.topics(), &log.data.data)
                    .unwrap()
                    .notionalCap
            })
            .collect()
    };

    set_margin_tiers_capped(&mut ctx, ADMIN, MARKET_ID, &bounds, &[3, 2], &[0, 0], 1_000_001)
        .unwrap();
    assert_eq!(caps_of_events(&mut ctx), vec![1_000_001]);
    assert_eq!(
        get_margin_tiers_full(&mut ctx, MARKET_ID).notionalCaps,
        vec![1_000_000, 1_000_001]
    );
    let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    assert_eq!(market.tiers.notional_cap(), 1_000_001);

    for unbounded in [0, u64::MAX] {
        set_margin_tiers_capped(&mut ctx, ADMIN, MARKET_ID, &bounds, &[3, 2], &[0, 0], unbounded)
            .unwrap();
        assert_eq!(caps_of_events(&mut ctx), vec![0], "stored and published as 0");
        let market = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
        assert_eq!(market.tiers.notional_cap(), 0);
        assert_eq!(
            get_margin_tiers_full(&mut ctx, MARKET_ID).notionalCaps,
            vec![1_000_000, u64::MAX]
        );
    }
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
        crate::margin::view::derived_available_balance(&mut ctx, ALICE).unwrap(),
        166_666_666
    );

    // The top-up needed is 500_000_000 − 333_333_334 = 166_666_666 — exactly the available.
    set_leverage(&mut ctx, 2).unwrap();

    let pos = position(&mut ctx, ALICE);
    assert_eq!(pos.leverage, 2);
    assert_eq!(oo_im(&mut ctx, ALICE), 500_000_000);
    assert_eq!(wallet(&mut ctx, ALICE), 500_000_000);
    assert_eq!(
        crate::margin::view::derived_available_balance(&mut ctx, ALICE).unwrap(),
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
        crate::margin::view::derived_available_balance(&mut ctx, ALICE).unwrap(),
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
    assert!(err.to_string().contains(super::ADD_MARGIN_CAP_REJECT), "{err}");
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

/// 2b gate order on a $1 000 long, `L = 5`, margin $200, mark = entry (uPnL 0):
/// `PIM = 1000/5 = $200`, `MM = 1000/6 = $166_666_666`.
///
/// * Removing $40 would leave $160 < MM, so G2 (`iw − MM`) refuses it — the FIRST failing gate
///   names it, even though G3 would refuse it too.
/// * The cap is `max(0, min(200 − 166.67, 200 + 0 − 200)) = 0`: the equity term binds, and a
///   single micro-unit is refused by G3. Under the maintenance-only rule this replaced, $33.33
///   came out here — it let equity fall to `MM` rather than `PIM`.
#[test]
fn remove_position_margin_rejects_when_it_would_breach_maintenance() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);

    let err = remove_position_margin(&mut ctx, 40_000_000).unwrap_err();
    assert!(
        err.to_string()
            .contains(REMOVE_MARGIN_G2_REJECT),
        "G2, the balance term, is the first gate that fails: {err}"
    );
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET);
    assert_eq!(position(&mut ctx, ALICE).margin, MARGIN);

    // $33.33 clears G2 (leaves 166_666_667 ≥ MM) but not G3: equity 166_666_667 < PIM.
    let err = remove_position_margin(&mut ctx, 33_333_333).unwrap_err();
    assert!(
        err.to_string()
            .contains(REMOVE_MARGIN_G3_REJECT),
        "{err}"
    );
    // And the cap is exactly zero: one micro-unit is refused by the same gate.
    let err = remove_position_margin(&mut ctx, 1).unwrap_err();
    assert!(
        err.to_string()
            .contains(REMOVE_MARGIN_G3_REJECT),
        "{err}"
    );
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET);
    assert_eq!(position(&mut ctx, ALICE).margin, MARGIN);
}

/// The fresh-position case B2 was about, under 2b: at a market's max leverage a just-opened
/// position sits one opening fee BELOW its initial margin (since `7cc26360` the fill funds the fee
/// out of the margin it creates), so its equity is short of `PIM` and the cap is negative.
/// `addPositionMargin(X)` lifts the cap to `X − shortfall`, not to `X`: a full round trip of `X` is
/// REFUSED, and exactly `X − shortfall` comes back out. Binance behaves the same way (R21b refused
/// 0.01 on a fresh position, both sides).
#[test]
fn remove_position_margin_round_trip_returns_only_what_clears_initial_margin() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // Max leverage 3, and margin 1_000 below floor(N/3) — the post-fill shape. `PIM` rounds UP to
    // 333_333_334, so the silo is 1_001 short of it.
    let post_fee_margin = 1_000_000_000i64 / 3 - 1_000;
    let shortfall = 333_333_334 - post_fee_margin;
    assert_eq!(shortfall, 1_001);
    storage::save_position(
        &mut ctx,
        ALICE,
        MARKET_ID,
        &PerpPosition {
            amount: QTY,
            v_quote_balance: -ENTRY_VALUE,
            margin: post_fee_margin,
            leverage: 3,
            ..crate::margin::margin_type::never_set_position()
        },
        AccountUpdateReason::Adjustment,
    )
    .unwrap();
    let wallet_before = wallet(&mut ctx, ALICE);

    // Nothing comes out of a fresh position.
    let err = remove_position_margin(&mut ctx, 1).unwrap_err();
    assert!(
        err.to_string()
            .contains(REMOVE_MARGIN_G3_REJECT),
        "{err}"
    );

    add_position_margin(&mut ctx, 10_000_000).unwrap();
    // The full round trip is refused, whole — not clipped to the cap.
    let err = remove_position_margin(&mut ctx, 10_000_000).unwrap_err();
    assert!(
        err.to_string()
            .contains(REMOVE_MARGIN_G3_REJECT),
        "{err}"
    );
    assert_eq!(position(&mut ctx, ALICE).margin, post_fee_margin + 10_000_000);
    // One past the cap is refused; the cap itself comes out exactly.
    let cap = 10_000_000 - shortfall as u64;
    assert!(remove_position_margin(&mut ctx, cap + 1).is_err());
    remove_position_margin(&mut ctx, cap).unwrap();
    let pos = position(&mut ctx, ALICE);
    assert_eq!(pos.margin, 333_333_334, "the silo lands exactly on PIM");
    assert_eq!(wallet(&mut ctx, ALICE), wallet_before - shortfall as u64);
}

/// A position at a loss: long 10 @ $100 marked at $90, margin $200, `L = 5`.
/// `N = 900`, `uPnL = −100`, `PIM = 900/5 = 180`, `MM = 900/6 = 150`.
///
/// Removing $20 leaves equity `180 − 100 = 80`. The equity term (G3, `≥ PIM = 180`) refuses it;
/// the loss is what refuses it — the balance alone (180) would still clear both `PIM` and `MM`.
/// The cap is `max(0, min(200 − 150, 200 − 100 − 180)) = 0`.
#[test]
fn remove_position_margin_rejects_when_a_loss_takes_equity_below_initial_margin() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    // Mark drops to $90 → position carries a $100 unrealized loss.
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();

    let err = remove_position_margin(&mut ctx, 20_000_000).unwrap_err();
    assert!(
        err.to_string()
            .contains(REMOVE_MARGIN_G3_REJECT),
        "the unrealized LOSS counts against the equity term: {err}"
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
            ..crate::margin::margin_type::never_set_position()
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

/// **The clearance fee is levied on what this call CLOSED, so n rounds cost one fee.** (On the
/// notional closed since cross P3 / Q3; on the silo margin consumed before — the property is the
/// same either way.)
///
/// A liquidation does not always finish in one call — the book leg can stop on the band or the
/// distinct-maker cap, the ADL leg on the shared fill budget or on too little eligible opposite
/// interest — and the position is then re-swept. The fee used to be `entry margin x rate` every
/// round, so finishing in n rounds cost a geometric series: the user paying repeatedly, for one
/// liquidation, because of OUR gas caps.
///
/// Here the book absorbs half, there is no ADL counterparty, so the rest defers; a second round
/// finishes it. The two fees must sum to the ONE-SHOT fee, which the control half measures rather
/// than restates. Under the old rule the split run was strictly more expensive — that is the
/// `assert!` at the end, and it is what fails if the base reverts to the ENTRY size.
#[test]
fn a_liquidation_split_over_two_rounds_pays_one_clearance_fee() {
    // ── split: 5 units through the book, 5 deferred, then the rest ──
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    m.liquidation_fee_rate_bps = 500;
    storage::save_market(&mut ctx, &m).unwrap();
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();

    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64 / 2);
    liquidate(&mut ctx, ALICE).unwrap();
    let after_round_1 = position(&mut ctx, ALICE).amount;
    assert_eq!(after_round_1, QTY / 2, "half closed, half deferred — no ADL counterparty");
    let fee_1 = storage::load_insurance_fund(&mut ctx).unwrap();
    assert!(fee_1 > 0, "a round that DID close something must still charge");

    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64 / 2);
    liquidate(&mut ctx, ALICE).unwrap();
    assert_eq!(position(&mut ctx, ALICE).amount, 0, "finished");
    let split_total = storage::load_insurance_fund(&mut ctx).unwrap();

    // ── control: the identical position closed in ONE round ──
    let mut ctl = make_ctx();
    setup_market(&mut ctl);
    let mut m = storage::load_market(&mut ctl, MARKET_ID).unwrap().unwrap();
    m.liquidation_fee_rate_bps = 500;
    storage::save_market(&mut ctl, &m).unwrap();
    save_position(&mut ctl, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctl, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    place_maker_order(&mut ctl, Side::Buy as u8, LONG_LIQ_PRICE, QTY as u64);
    liquidate(&mut ctl, ALICE).unwrap();
    assert_eq!(position(&mut ctl, ALICE).amount, 0);
    let one_shot = storage::load_insurance_fund(&mut ctl).unwrap();

    assert_eq!(
        split_total, one_shot,
        "splitting a liquidation must not change what it costs the user"
    );
    // The sharp one. Round 1 closed exactly half the position, so it pays exactly half the fee.
    // Under the old `entry margin x rate` base it would have paid the WHOLE fee up front — and the
    // deferred half would then have been charged again next round, for 1.5x in total. Reverting
    // the base makes this `one_shot == one_shot`... on the left and fails here.
    assert_eq!(
        fee_1 * 2,
        one_shot,
        "half the position closed must cost half the fee: one_shot={one_shot} round1={fee_1}"
    );
}

/// **Q3: the clearance fee is a rate on the NOTIONAL closed — and J5: an isolated one never takes
/// more than its close returned to the wallet.**
///
/// Long 10 @ $100 on a $200 silo, closed into a $81 bid: realized −$190, so the close returns only
/// $10 to the wallet. 5% of the $810 notional closed is $40.50 — but the cap is the $10 close
/// credit, not the $60 the wallet then holds: the other $50 is collateral the close did not
/// release (since cross margin, the wallet IS the cross pool), and the fee must not reach it.
#[test]
fn an_isolated_clearance_fee_is_capped_at_the_close_credit() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    m.liquidation_fee_rate_bps = 500;
    storage::save_market(&mut ctx, &m).unwrap();
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, 8_100).unwrap();
    place_maker_order(&mut ctx, Side::Buy as u8, 8_100, QTY as u64);
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();

    liquidate(&mut ctx, ALICE).unwrap();

    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    let fee = storage::load_insurance_fund(&mut ctx).unwrap() - if_before;
    // The uncapped fee at the market's stored rate, on the $810 notional closed.
    let uncapped = 810_000_000u64 * m.liquidation_fee_rate_bps as u64 / 10_000;
    assert_eq!(uncapped, 40_500_000);
    assert!(
        uncapped > fee,
        "the uncapped notional fee ($40.50) must exceed the cap for this to test it"
    );
    assert_eq!(fee, 10_000_000, "capped at the $10 close credit (J5)");
    assert_eq!(
        wallet(&mut ctx, ALICE),
        USER_WALLET,
        "the wallet keeps exactly what it held before the close: the fee took the whole credit \
         and nothing else"
    );
}

/// **A BANKRUPT isolated liquidation pays no clearance fee** (Binance: none on a bankrupt
/// position). Long 10 @ $100 on a $200 silo closed into a $75 bid: realized −$250, the silo covers
/// $200 and $50 is bad debt — the close returns nothing to the wallet, so the J5 cap is 0 however
/// large the notional fee would be. (That is a fill WORSE than its $80 bankruptcy price; a residual
/// ADL'd AT a rounded bankruptcy price pays the rounding dust — the next test.)
#[test]
fn a_bankrupt_isolated_liquidation_pays_no_clearance_fee() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    m.liquidation_fee_rate_bps = 500;
    storage::save_market(&mut ctx, &m).unwrap();
    storage::save_insurance_fund(&mut ctx, 1_000_000_000).unwrap();
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, 7_500).unwrap();
    place_maker_order(&mut ctx, Side::Buy as u8, 7_500, QTY as u64);
    let _ = JournalTr::take_logs(ctx.journal_mut());

    liquidate(&mut ctx, ALICE).unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());

    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    let liq = decode_events::<crate::interface::IPerpDex::Liquidation>(&logs);
    assert_eq!(liq.len(), 1);
    assert_eq!(liq[0].reward, 0, "bankrupt: no clearance fee");
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET, "the wallet is untouched");
    assert_eq!(
        storage::load_insurance_fund(&mut ctx).unwrap(),
        1_000_000_000 - 50_000_000,
        "the fund only absorbed the $50 of bad debt"
    );
}

/// **…but a bankrupt residual ADL'd AT its bankruptcy price pays that price's ROUNDING DUST.** The
/// devnet liquidation gate's `fee` case on its own numbers (2026-10-27), where the harness had
/// asserted 0 and the engine charged 33.
///
/// A BTC-PERP-like market — base 3 / price 2 decimals, tick 10, so `calc_value(P, q) = 10·P·q`
/// exactly — at 50 bps. ALICE: ISOLATED long 0.01 (`amount` 10) @ $100,000 at 3x, `vQuote` −1e9,
/// silo 332_333_333 (the 1e9 / 3 initial margin less the gate's 1_000_000 taker fee); KEEPER the
/// opposite isolated short. The oracle ticks to $40,000 with an empty book: ALICE's equity there is
/// −267_666_667, so the whole 10 go to ADL at her bankruptcy price, which a long rounds UP —
/// `⌈667_666_667 / 100⌉ = 6_676_667` — and at which she keeps `667_666_700 − 667_666_667 = 33`.
///
/// That 33 is the close credit (J5), so of the 2_000_000 due (50 bps of the $400 closed at the
/// mark) she pays exactly 33: the dust goes to the insurance fund and her wallet nets 0. It sits
/// inside the bound `calc_value(1, 10) − 1 = 99` (`pd + bd ≤ 6`), and the system's value is
/// conserved — no bad debt, nothing minted.
#[test]
fn a_bankrupt_isolated_adl_pays_its_bankruptcy_price_rounding_dust_as_the_clearance_fee() {
    const SILO: i64 = 332_333_333;
    const P_B: u64 = 6_676_667;
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let mut m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    m.base_decimals = 3;
    m.tick_size = 10;
    m.max_price = 100_000_000;
    m.liquidation_fee_rate_bps = 50;
    storage::save_market(&mut ctx, &m).unwrap();
    storage::save_mark_price(&mut ctx, MARKET_ID, 10_000_000).unwrap();
    seed_position_account(&mut ctx, ALICE, QTY, -ENTRY_VALUE, SILO, 3, USER_WALLET as i64);
    seed_position_account(&mut ctx, KEEPER, -QTY, ENTRY_VALUE, 333_333_333, 3, 0);
    let users = [ALICE, KEEPER];
    let value_before = conservation_sum(&mut ctx, &users);
    let if_before = storage::load_insurance_fund(&mut ctx).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());

    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: 4_000_000,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    let logs = JournalTr::take_logs(ctx.journal_mut());

    // The close: no book, so all 10 by ADL against KEEPER at the rounded-up bankruptcy price.
    let adl = decode_events::<crate::interface::IPerpDex::Adl>(&logs);
    assert_eq!(adl.len(), 1);
    assert_eq!(
        (adl[0].liquidatedUser, adl[0].adlUser, adl[0].qty, adl[0].price),
        (ALICE, KEEPER, QTY as u64, P_B)
    );
    let surplus = calc_value(P_B, QTY as u64, 3, 2).unwrap() as i64 - (ENTRY_VALUE - SILO);
    assert_eq!(surplus, 33, "the liquidated equity at P_b: the rounding surplus");
    let changes = decode_events::<crate::interface::IPerpDex::PositionChanged>(&logs);
    let alice_close: Vec<_> = changes.iter().filter(|c| c.user == ALICE).collect();
    assert_eq!(alice_close.len(), 1);
    assert_eq!(
        alice_close[0].realizedPnl,
        surplus - SILO,
        "she loses her silo less the surplus"
    );

    // The fee: due 2_000_000, capped at the 33 close credit.
    let due = clearance_fee_due(&m, 4_000_000, QTY as u64);
    assert_eq!(due, 2_000_000);
    let liq = decode_events::<crate::interface::IPerpDex::Liquidation>(&logs);
    assert_eq!(liq.len(), 1);
    assert_eq!(
        (liq[0].user, liq[0].amount, liq[0].reward, liq[0].markPrice, liq[0].marginType),
        (ALICE, QTY, 33, 4_000_000, crate::types::MarginType::Isolated.code())
    );
    assert!((liq[0].reward as u128) < due, "dust, not the fee due");
    assert!(
        liq[0].reward < calc_value(1, QTY as u64, 3, 2).unwrap(),
        "under one price unit's worth of the closed size (`pd + bd ≤ 6`)"
    );
    let if_events = decode_events::<crate::interface::IPerpDex::InsuranceFundChanged>(&logs);
    assert_eq!(if_events.len(), 1);
    assert_eq!((if_events[0].delta, if_events[0].newBalance), (33, if_before + 33));
    assert_eq!(storage::load_insurance_fund(&mut ctx).unwrap(), if_before + 33);

    // The parties: ALICE's wallet rose by the 33 and paid it on, KEEPER booked the rest.
    assert_eq!(position(&mut ctx, ALICE).amount, 0);
    assert_eq!(position(&mut ctx, KEEPER).amount, 0);
    assert_eq!(wallet(&mut ctx, ALICE), USER_WALLET, "+33 close credit, −33 clearance fee");
    assert_eq!(
        wallet(&mut ctx, KEEPER),
        (333_333_333 + ENTRY_VALUE - 667_666_700) as u64,
        "his silo plus the short's gain to P_b"
    );
    assert_eq!(conservation_sum(&mut ctx, &users), value_before, "conserved: no bad debt, no mint");
}

#[test]
fn liquidate_closes_the_book_unfillable_residual_by_adl_at_mark() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    save_position(&mut ctx, QTY, -ENTRY_VALUE);
    storage::save_mark_price(&mut ctx, MARKET_ID, LONG_LIQ_PRICE).unwrap();
    // Book only provides QTY-1 of closing liquidity.
    place_maker_order(&mut ctx, Side::Buy as u8, LONG_LIQ_PRICE, (QTY as u64) - 1);
    // ...and WHALE absorbs the 1-unit remainder. The book maker ends up LONG once ALICE sells into
    // its bid, i.e. the same side as ALICE, so it is never an ADL candidate — the residual has no
    // counterparty without this and the liquidation would leave 1 unit open.
    seed_absorbing_short(&mut ctx, 1);

    // Close what the book can absorb, then ADL the 1-unit residual at MARK. At the $90 mark the
    // residual is still solvent (equity +$100), which is what selects mark over the bankruptcy
    // price. The position ends fully closed, not rejected and not deferred.
    liquidate(&mut ctx, ALICE).unwrap();

    let alice = position(&mut ctx, ALICE);
    assert_eq!(
        alice.amount, 0,
        "position fully closed via book + residual-by-ADL"
    );
    assert_eq!(alice.v_quote_balance, 0);

    // ── `cr` ACROSS THE TWO-LEG CLOSE ──────────────────────────────────────────────────────────
    //
    // Both legs now reach `settlement::apply_position_fill` — the book absorbs `QTY − 1` through
    // the taker path and the 1-unit residual through `adl_fill` — so `cr` has a single
    // accumulation site and cannot disagree with itself. (It used to have two: the residual was
    // settled at mark against NO counterparty by a function that computed and credited its own
    // `realized_pnl`. That function is gone.)
    //
    // The total must equal the WHOLE position's realised loss, split over the two legs exactly as
    // the emitted events split it — checked against the events rather than restated, and pinned to
    // a literal so a derivation broken identically on both sides still fails.
    let changes = take_position_changes(&mut ctx);
    let legs: Vec<i64> = changes
        .iter()
        .filter(|c| c.user == ALICE)
        .map(|c| c.realizedPnl)
        .collect();
    assert_eq!(
        legs.len(),
        2,
        "the fixture must close in TWO legs (book + residual), else the residual site is untested"
    );
    assert!(
        legs.iter().all(|&l| l != 0),
        "both legs must realise something: {legs:?}"
    );
    assert_eq!(
        alice.cumulative_realized_pnl,
        legs.iter().sum::<i64>(),
        "`cr` is the sum of BOTH legs — the book fill and the residual settled at mark"
    );
    assert_eq!(
        alice.cumulative_realized_pnl, -100_000_000,
        "the whole long realised −$100 closing at the $90 mark from a $100 entry"
    );
    // The LAST event ALICE got is the residual's, and it publishes the completed total.
    assert_eq!(
        changes
            .iter()
            .rfind(|c| c.user == ALICE)
            .unwrap()
            .cumulativeRealizedPnl,
        -100_000_000
    );
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

/// `addMarket` emits NO price event, and this test exists so nobody reinstates one.
///
/// It used to. The seeding emission looked harmless — the intuition was that every component
/// genuinely equals `initialMarkPrice` ("funding is 0 so price1 = index, no trades so
/// contractPrice = index, empty basis window so price2 = index, hence median = index"). That
/// silently assumes an index price EXISTS and equals the initial mark. It does not: `addMarket`
/// writes no `IndexPriceState`, so `getIndexPrice` returns 0 and five of the nine fields would be
/// 0. Reporting them as `initialMarkPrice` would contradict the view selectors; reporting them as
/// the true 0 makes `MarkPriceUpdated`'s fields only CONDITIONALLY meaningful, and a backend that
/// handles every instance uniformly then publishes `i = 0, r = 0, T = 0` once per market with no
/// error raised anywhere. Both options are bad, so the event does not fire here at all: the initial
/// mark is published by `MarketAdded.initialMarkPrice`, which a consumer reads anyway.
///
/// The property being bought is that EVERY `MarkPriceUpdated` is uniformly valid — so the second
/// half of this test pins the other side of it: the first real oracle push does emit one, with
/// every component populated.
#[test]
fn add_market_emits_no_price_event() {
    let mut ctx = make_ctx();
    storage::save_admin(&mut ctx, ADMIN).unwrap();
    let _ = JournalTr::take_logs(ctx.journal_mut());

    call_add_market(&mut ctx, 7, ENTRY_PRICE);

    let logs = JournalTr::take_logs(ctx.journal_mut());
    assert!(
        mark_price_events(&logs).is_empty(),
        "addMarket must not emit MarkPriceUpdated — see this test's doc comment before changing it"
    );
    // The mark IS set, and IS published — by `MarketAdded`, the event a consumer must read anyway
    // for symbol/decimals/tick.
    assert_eq!(storage::load_mark_price(&mut ctx, 7).unwrap(), ENTRY_PRICE);
    let added: Vec<_> = logs
        .iter()
        .filter(|log| {
            log.data.topics().first()
                == Some(&crate::interface::IPerpDex::MarketAdded::SIGNATURE_HASH)
        })
        .map(|log| {
            crate::interface::IPerpDex::MarketAdded::decode_raw_log(
                log.data.topics(),
                &log.data.data,
            )
            .unwrap()
        })
        .collect();
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].initialMarkPrice, ENTRY_PRICE);

    // And the first real oracle push is where the stream starts — fully populated, no special case.
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: 7,
            indexPrice: ENTRY_PRICE,
            timestamp: 31,
        }
        .abi_encode(),
        ADMIN,
        &mut ctx,
    )
    .unwrap();
    let first = mark_price_events(&JournalTr::take_logs(ctx.journal_mut()))
        .pop()
        .unwrap();
    assert_eq!(
        (
            first.indexPrice,
            first.price1,
            first.price2,
            first.markPrice
        ),
        (ENTRY_PRICE, ENTRY_PRICE, ENTRY_PRICE, ENTRY_PRICE),
        "once an index exists, every component IS the index"
    );
    assert_eq!(first.priceWindowTs, 30);
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
// * **Funding**, which is not an exact zero-sum transfer between longs and shorts here: each
//   position settles against the index on its own first touch, with its own floored payment, so
//   the two sides' payments need not net to zero at any instant. (It no longer touches the
//   insurance fund — a charge lands on `pos.margin` whole, possibly as a debt.) Disabled in this
//   market.
// * **`transferToPerp` / `transferFromPerp`**, which are genuine external flows. Not used here.
#[cfg(test)]
mod value_conservation;

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
mod usdc_custody;

// ── Out-of-band resting-order expiry (band GC) ─────────────────────────────────
//
// `match_order` bands FILLS, not placements, so a mark move can leave a level sitting AT THE TOUCH
// and unmatchable forever. `run_out_of_band_expiry_sweep` (inside `updateIndexPrice`, where the band
// moves) collects the NEAR-SIDE prefix of such levels — asks below the lower edge, bids above the
// upper edge — and leaves the FAR ends alone, because those are the ordinary deep passive orders the
// fill-time band is written to let rest.
mod band_expiry;

// ── ORDERED `ACCOUNT_UPDATE` STREAM SHAPE: liquidation and ADL ───────────────────────────────────
//
// Both of these were structurally uncoverable by the old hand-attached snapshot helper (it ran only
// in `trading::tests::account_snapshot_events`), and both are the paths where an orphan
// `PositionChanged` is most damaging: a liquidation close and an ADL fill each move TWO users'
// positions in one call. The invariant itself is enforced at emit time by the guard in
// `crate::events`; these pin the exact ordered `(event, subject)` sequence, which is the only form
// that catches a row landing one slot early or one slot late.
mod account_update_stream;

// ── updateIndexPriceFromBook: the closed-external-session index ───────────────

/// Seed the market with an external index price, as a live feed would before it went away.
fn seed_external_index(ctx: &mut TestCtx, price: u64, ts: u64) {
    run_update_index_price(
        &updateIndexPriceCall {
            marketId: MARKET_ID,
            indexPrice: price,
            timestamp: ts,
        }
        .abi_encode(),
        ADMIN,
        ctx,
    )
    .unwrap();
}

fn call_from_book(ctx: &mut TestCtx, block_ts: u64, caller: Address) -> Result<Bytes, PerpError> {
    ctx.block.timestamp = U256::from(block_ts);
    run_update_index_price_from_book(
        &updateIndexPriceFromBookCall {
            marketId: MARKET_ID,
        }
        .abi_encode(),
        caller,
        ctx,
    )
}

fn index_now(ctx: &mut TestCtx) -> u64 {
    storage::load_index_price_state(ctx, MARKET_ID)
        .unwrap()
        .index_price
}

#[test]
fn from_book_walks_the_index_toward_a_book_above_it() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_external_index(&mut ctx, ENTRY_PRICE, 30);

    // Whole book above the index: bid 101.00 / ask 101.10.
    storage::save_best_bid(&mut ctx, MARKET_ID, 10_100).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, 10_110).unwrap();

    // Many ticks, because one 15s step moves a fraction of a cent.
    let mut ts = 30;
    for _ in 0..600 {
        ts += 15;
        call_from_book(&mut ctx, ts, ADMIN).unwrap();
    }
    let got = index_now(&mut ctx);
    assert!(
        got > ENTRY_PRICE && got <= 10_100,
        "index should climb toward the bid, got {got}"
    );
}

#[test]
fn from_book_walks_the_index_down_toward_a_book_below_it() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_external_index(&mut ctx, ENTRY_PRICE, 30);

    storage::save_best_bid(&mut ctx, MARKET_ID, 9_890).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, 9_900).unwrap();

    let mut ts = 30;
    for _ in 0..600 {
        ts += 15;
        call_from_book(&mut ctx, ts, ADMIN).unwrap();
    }
    let got = index_now(&mut ctx);
    assert!(
        got < ENTRY_PRICE && got >= 9_900,
        "index should fall toward the ask, got {got}"
    );
}

#[test]
fn from_book_does_not_move_the_index_on_an_empty_book() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_external_index(&mut ctx, ENTRY_PRICE, 30);
    // best_bid/best_ask are both 0 — no quotes at all.

    let mut ts = 30;
    for _ in 0..50 {
        ts += 15;
        call_from_book(&mut ctx, ts, ADMIN).unwrap();
    }
    assert_eq!(
        index_now(&mut ctx),
        ENTRY_PRICE,
        "an empty book must leave the index exactly where it was"
    );
}

#[test]
fn from_book_is_a_deadband_while_the_index_sits_inside_the_spread() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_external_index(&mut ctx, ENTRY_PRICE, 30);

    storage::save_best_bid(&mut ctx, MARKET_ID, ENTRY_PRICE - 10).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, ENTRY_PRICE + 10).unwrap();

    let mut ts = 30;
    for _ in 0..200 {
        ts += 15;
        call_from_book(&mut ctx, ts, ADMIN).unwrap();
    }
    assert_eq!(
        index_now(&mut ctx),
        ENTRY_PRICE,
        "straddled index must not drift — moving it requires a quote past it"
    );
}

#[test]
fn from_book_tracks_the_only_side_quoted() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_external_index(&mut ctx, ENTRY_PRICE, 30);

    // Ask-only book. A zero best_bid must NOT be read as a zero-priced bid.
    storage::save_best_ask(&mut ctx, MARKET_ID, 9_900).unwrap();

    let mut ts = 30;
    for _ in 0..600 {
        ts += 15;
        call_from_book(&mut ctx, ts, ADMIN).unwrap();
    }
    let got = index_now(&mut ctx);
    assert!(
        got < ENTRY_PRICE && got >= 9_900,
        "tracks the ask, got {got}"
    );
}

#[test]
fn from_book_rejects_a_market_that_never_had_an_index() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    storage::save_best_bid(&mut ctx, MARKET_ID, 10_100).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, 10_110).unwrap();

    let err = call_from_book(&mut ctx, 45, ADMIN).unwrap_err();
    assert!(
        format!("{err:?}").contains("no index price yet"),
        "a closed-session oracle needs an open session first, got {err:?}"
    );
}

#[test]
fn from_book_requires_the_oracle_or_admin_role() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_external_index(&mut ctx, ENTRY_PRICE, 30);

    let err = call_from_book(&mut ctx, 45, ALICE).unwrap_err();
    assert!(
        format!("{err:?}").contains("not authorised"),
        "unexpected error: {err:?}"
    );
}

#[test]
fn from_book_is_a_no_op_inside_one_price_bucket() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_external_index(&mut ctx, ENTRY_PRICE, 30);
    storage::save_best_bid(&mut ctx, MARKET_ID, 20_000).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, 20_010).unwrap();

    call_from_book(&mut ctx, 45, ADMIN).unwrap();
    let after_first = storage::load_index_mode_state(&mut ctx, MARKET_ID)
        .unwrap()
        .index_scaled;

    // Same 15s bucket: the staleness gate must swallow these, so the accumulator cannot be
    // advanced many times within one block by repeating the call.
    for ts in [45, 50, 59] {
        call_from_book(&mut ctx, ts, ADMIN).unwrap();
    }
    assert_eq!(
        storage::load_index_mode_state(&mut ctx, MARKET_ID)
            .unwrap()
            .index_scaled,
        after_first,
        "repeat calls inside one bucket must not each apply an EWMA step"
    );
}

#[test]
fn the_external_path_is_unchanged_when_no_ramp_is_in_flight() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);

    // Plain weekday pushes: the exposed index must be the oracle's argument, bit for bit.
    for (px, ts) in [(ENTRY_PRICE, 30u64), (10_250, 45), (9_875, 60)] {
        seed_external_index(&mut ctx, px, ts);
        assert_eq!(index_now(&mut ctx), px, "external index must pass through");
    }
    let mode = storage::load_index_mode_state(&mut ctx, MARKET_ID).unwrap();
    assert!(!mode.from_book);
    assert_eq!(mode.ramp_from, 0, "no ramp without a prior book update");
    assert_eq!(
        crate::math::index_from_scaled(mode.index_scaled),
        9_875,
        "accumulator tracks the exposed index"
    );
}

#[test]
fn the_feed_returning_ramps_instead_of_jumping_and_lands_exactly() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_external_index(&mut ctx, ENTRY_PRICE, 30);

    // Drive the index away from the feed with a book well below it.
    storage::save_best_bid(&mut ctx, MARKET_ID, 9_000).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, 9_010).unwrap();
    let mut ts = 30;
    for _ in 0..400 {
        ts += 15;
        call_from_book(&mut ctx, ts, ADMIN).unwrap();
    }
    let weekend_index = index_now(&mut ctx);
    assert!(weekend_index < ENTRY_PRICE, "book drove the index down");

    // Feed returns, far above where the book left it.
    let external = 10_500;
    ts += 15;
    seed_external_index(&mut ctx, external, ts);
    let first = index_now(&mut ctx);
    assert_eq!(
        first, weekend_index,
        "handover is continuous: the first external push publishes where the book left off"
    );

    // Partway through the ramp it is strictly between the two.
    ts += crate::math::INDEX_RAMP_SECS / 2;
    seed_external_index(&mut ctx, external, ts);
    let mid = index_now(&mut ctx);
    assert!(
        mid > weekend_index && mid < external,
        "mid-ramp should be between {weekend_index} and {external}, got {mid}"
    );

    // Past the window it lands exactly on the feed and the ramp is cleared.
    ts += crate::math::INDEX_RAMP_SECS;
    seed_external_index(&mut ctx, external, ts);
    assert_eq!(index_now(&mut ctx), external, "ramp must land on the feed");
    let mode = storage::load_index_mode_state(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(mode.ramp_from, 0, "ramp cleared once complete");
    assert!(!mode.from_book);

    // And a later push is pass-through again.
    ts += 15;
    seed_external_index(&mut ctx, 10_600, ts);
    assert_eq!(index_now(&mut ctx), 10_600);
}

#[test]
fn a_book_update_mid_ramp_cancels_the_ramp() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    seed_external_index(&mut ctx, ENTRY_PRICE, 30);
    storage::save_best_bid(&mut ctx, MARKET_ID, 9_000).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, 9_010).unwrap();

    let mut ts = 45;
    call_from_book(&mut ctx, ts, ADMIN).unwrap();
    ts += 15;
    seed_external_index(&mut ctx, 10_500, ts); // opens a ramp
    assert_ne!(
        storage::load_index_mode_state(&mut ctx, MARKET_ID)
            .unwrap()
            .ramp_from,
        0
    );

    // The feed drops out again before the ramp finished.
    ts += 15;
    call_from_book(&mut ctx, ts, ADMIN).unwrap();
    let mode = storage::load_index_mode_state(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(
        mode.ramp_from, 0,
        "book update supersedes an in-flight ramp"
    );
    assert!(mode.from_book);
}

// ── basis_mode: the per-market price2 split ──────────────────────────────────

fn set_basis_mode(ctx: &mut TestCtx, mode: u8, caller: Address) -> Result<Bytes, PerpError> {
    run_set_basis_mode(
        &setBasisModeCall {
            marketId: MARKET_ID,
            mode,
        }
        .abi_encode(),
        caller,
        ctx,
    )
}

#[test]
fn a_market_is_born_on_the_legacy_window_basis() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    let m = storage::load_market(&mut ctx, MARKET_ID).unwrap().unwrap();
    assert_eq!(
        m.basis_mode,
        crate::types::BASIS_MODE_WINDOW,
        "every market, including every USD-M market, defaults to the mechanism it already had"
    );
}

#[test]
fn set_basis_mode_is_gated_and_validated() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);

    let err = set_basis_mode(&mut ctx, crate::types::BASIS_MODE_EWMA, ALICE).unwrap_err();
    assert!(format!("{err:?}").contains("not authorised"), "{err:?}");

    let err = set_basis_mode(&mut ctx, 9, ADMIN).unwrap_err();
    assert!(format!("{err:?}").contains("unknown mode"), "{err:?}");

    set_basis_mode(&mut ctx, crate::types::BASIS_MODE_EWMA, ADMIN).unwrap();
    assert_eq!(
        storage::load_market(&mut ctx, MARKET_ID)
            .unwrap()
            .unwrap()
            .basis_mode,
        crate::types::BASIS_MODE_EWMA
    );
}

/// The USD-M guarantee, executable: a market left on the window basis touches NEITHER the EWMA
/// blob nor any of the new code. If this ever fails, the split has leaked.
#[test]
fn the_window_basis_market_never_touches_the_ewma_state() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    // REAL quotes, so the per-best-quote-change sampler that feeds the legacy window runs.
    place_order(&mut ctx, MAKER, 0, ENTRY_PRICE - 10, 1);
    place_order(&mut ctx, MAKER, 1, ENTRY_PRICE + 10, 1);

    let mut ts = 30;
    for _ in 0..40 {
        seed_external_index(&mut ctx, ENTRY_PRICE, ts);
        ts += 15;
    }
    let ewma = storage::load_price_basis_ewma(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(ewma.last_sample_ts, 0, "EWMA state must stay untouched");
    assert_eq!(ewma.basis_scaled, 0);
    // And the legacy window IS still being maintained for it.
    let window = storage::load_price_basis_window(&mut ctx, MARKET_ID).unwrap();
    assert!(window.count > 0, "the window mechanism must still run");
}

#[test]
fn the_ewma_basis_market_uses_the_new_mechanism_instead() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_basis_mode(&mut ctx, crate::types::BASIS_MODE_EWMA, ADMIN).unwrap();
    storage::save_best_bid(&mut ctx, MARKET_ID, ENTRY_PRICE + 90).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, ENTRY_PRICE + 110).unwrap();

    seed_external_index(&mut ctx, ENTRY_PRICE, 30);

    let ewma = storage::load_price_basis_ewma(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(ewma.last_sample_ts, 30, "sampled at oracle cadence");
    assert_eq!(
        crate::math::effective_basis(ewma.basis_scaled, ENTRY_PRICE),
        100,
        "seeded at the real offset rather than decaying in from zero"
    );
    // The legacy index history is NOT written for this market — that blob exists only for the
    // window mechanism, and writing it here would be state nothing reads.
    let hist = storage::load_index_price_history(&mut ctx, MARKET_ID).unwrap();
    assert!(hist.checkpoints.is_empty(), "no legacy history for an EWMA market");
}

#[test]
fn an_ewma_market_with_an_empty_book_never_samples() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_basis_mode(&mut ctx, crate::types::BASIS_MODE_EWMA, ADMIN).unwrap();

    let mut ts = 30;
    for _ in 0..5 {
        seed_external_index(&mut ctx, ENTRY_PRICE, ts);
        ts += 15;
    }
    let ewma = storage::load_price_basis_ewma(&mut ctx, MARKET_ID).unwrap();
    assert_eq!(ewma.last_sample_ts, 0, "no observation, no sample");
}

#[test]
fn an_ewma_markets_basis_cannot_exceed_the_clamp() {
    let mut ctx = make_ctx();
    setup_market(&mut ctx);
    set_basis_mode(&mut ctx, crate::types::BASIS_MODE_EWMA, ADMIN).unwrap();
    storage::save_best_bid(&mut ctx, MARKET_ID, ENTRY_PRICE * 5).unwrap();
    storage::save_best_ask(&mut ctx, MARKET_ID, ENTRY_PRICE * 5 + 10).unwrap();

    let mut ts = 30;
    for _ in 0..400 {
        seed_external_index(&mut ctx, ENTRY_PRICE, ts);
        ts += 15;
    }
    let ewma = storage::load_price_basis_ewma(&mut ctx, MARKET_ID).unwrap();
    let bound = ENTRY_PRICE as i64 * crate::math::MAX_BASIS_BPS as i64 / 10_000;
    assert_eq!(
        crate::math::effective_basis(ewma.basis_scaled, ENTRY_PRICE),
        bound,
        "price2's input is bounded however far the book goes"
    );
}


// ── MAX_SWEEP_ACTIONS_PER_UPDATE ──────────────────────────────────────────────
//
// The 50-cap had NO test at all: `grep MAX_SWEEP_ACTIONS_PER_UPDATE crates/` found only its
// definition, its use, and a doc link. Its two siblings are covered
// (`MAX_BAND_EXPIRIES_PER_UPDATE` by `the_cap_is_on_orders_and_a_second_update_finishes_the_job`,
// the distinct-maker cap by `liquidation_close_stops_at_the_maker_account_cap`), and the whole
// deferral story — "overflow defers to the next update, nothing is permanently missed" — rests on
// behaviour nothing exercised.
#[cfg(test)]
mod liquidation_cap;

mod algo;

// ── Roadmap 2b: `removePositionMargin` on Binance's isolated withdrawal cap ─────────────────────
//
//   cap = max(0, min( iw − MM ,  iw + uPnL − max(PIM, MM) ))      amount > cap ⇒ refused WHOLE
//
// Every expected cap in this module is computed the way a CLIENT would compute it — from the
// `getPositionRisk` row alone (`isolatedWallet`, `isolatedMargin`, `positionInitialMargin`,
// `maintMargin`) — and never by calling the gate or any cap helper. Where the spec gives a closed
// form, the hand-computed literal is asserted as well, so a bug SHARED by `margin_info_of` and the
// gate (they read the same function) still fails here.
//
// Off-trie writes cannot be rolled back, so every boundary probe sends `cap + 1` FIRST — refused,
// by a named gate, with the position, the account, the insurance fund and the perp write counter
// bit-identical and no log emitted — and only then `cap`, which must move exactly `cap`.
//
// Spec errata applied (IMPL_PROMPT_2b §4, verified against the code):
// * the "old rule" is HEAD's `amount ≤ iw ∧ iw − amount + uPnL ≥ MM`, i.e. old cap
//   `min(iw, iw + uPnL − MM)` — there has been no `N/L` gate since `4409a033`, so 2b only tightens;
// * G4 can be the failing gate only when `uPnL < 0 ∧ MM > PIM`, so T15 is built through
//   `setMarginTiers` under a live losing long, and through a short drifting into a 1× tier — not
//   through a long drifting up (that is in profit, and G2 fires first);
// * T9's "PIM = 2·MM" is `PIM ≥ 2·MM`;
// * T3's exact slope needs both marks in the equity regime, untruncated notionals and
//   `n(m₁) ≡ n(m₂) (mod L)`; `q·entry` is `|v_quote_balance|`;
// * `uPnL = trunc0(notional) + vQuote`, and a ×1 long's equity term is exactly `iw + vQuote`.
mod withdrawal_cap;

// ── Funding out of EQUITY: the debt a funding charge can leave on a position ─────────────────────
//
// A funding charge lands on `pos.margin` WHOLE (`crate::risk::funding`), so a position that keeps
// paying
// can run its margin NEGATIVE — a debt it owes. These pin what every consumer of that debt does
// with it: the maintenance check and the sweep (C1), a voluntary close, partial close and flip
// (C2, C3), an insolvent close (C4), a liquidation through the book (C5), the oracle sweep on a
// short whose debt outgrew its entry proceeds (C6), ADL counterparties carrying UNSETTLED funding
// (C7), the margin add/remove gates (C8) and the open-into-insolvency guard K9 (C9). The
// `calc_bankruptcy_price` unit half of C6 lives in `perp_core::math`'s `bankruptcy_price_tests`.
//
// Fixture market (`setup_market`): `base_decimals = 0`, `price_decimals = 2`, so
// `calc_value(p, q) = p · q · 1e4` exactly; one 3× tier, `MM = ⌊|N| / 6⌋`; funding index → payment
// on a `QTY` (= 10) position is `∓ ⌊index_delta / 10⌋`. Every number below is hand-derived.
mod funding_debt;
